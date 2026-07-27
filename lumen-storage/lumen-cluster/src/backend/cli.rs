//! The supported command line: reads from the cluster stack's own tools,
//! writes as transient units through `lumen_sys::exec`.
//!
//! Reads are unprivileged — `crm_mon`, `corosync-quorumtool`,
//! `corosync-cfgtool`, and `chronyc` answer root over sockets the sandbox
//! does not cover. The writes are the privileged half: `/etc/corosync` is
//! read-only inside `ProtectSystem=strict`, so the configuration is written
//! by `install` running as a transient unit, the content arriving over the
//! unit's standard input — typed argument arrays, never an interpolated
//! shell string, and never file content as an argument.
//!
//! Every parser is a free function over `&str`, outside the impl, so it
//! tests without a process. The fixtures in the tests are the formats EL10
//! ships: corosync 3.1, pacemaker 2.1, chrony 4.6.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lumen_sys::exec::{Exec, Request as ExecRequest};
use tokio::time::timeout;

use super::{ClusterBackend, LocalPreflight};
use crate::error::{ClusterError, Result};
use crate::state::{ClusterState, FenceDeviceState, NodeState, QuorumState, RingLink};

/// Reads answer in milliseconds on a healthy node and not at all on one whose
/// cluster stack has hung — and an unbounded read here would hang the whole
/// environment view. Same reasoning as `lumen_zfs::backend::cli::DEADLINE`.
const DEADLINE: Duration = Duration::from_secs(30);

const COROSYNC_CONF: &str = "/etc/corosync/corosync.conf";
const COROSYNC_AUTHKEY: &str = "/etc/corosync/authkey";
/// Pacemaker's CIB store. Removed with the corosync configuration: a stale
/// CIB would resurrect the old cluster's resources — fence devices with old
/// BMC passwords among them — into the next cluster built on this node.
/// Pacemaker recreates the directory on start.
const PACEMAKER_CIB: &str = "/var/lib/pacemaker/cib";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
const INSTALL: &str = "/usr/bin/install";
const RM: &str = "/usr/bin/rm";
const PCS: &str = "/usr/sbin/pcs";
const CIBADMIN: &str = "/usr/sbin/cibadmin";

pub struct CliBackend {
    crm_mon: String,
    quorumtool: String,
    cfgtool: String,
    chronyc: String,
    corosync_conf: PathBuf,
    exec: Arc<dyn Exec>,
}

impl CliBackend {
    pub fn new(exec: Arc<dyn Exec>) -> Self {
        CliBackend {
            crm_mon: "/usr/sbin/crm_mon".into(),
            quorumtool: "/usr/sbin/corosync-quorumtool".into(),
            cfgtool: "/usr/sbin/corosync-cfgtool".into(),
            chronyc: "/usr/bin/chronyc".into(),
            corosync_conf: PathBuf::from(COROSYNC_CONF),
            exec,
        }
    }

    /// Repoint the corosync.conf read, for tests.
    pub fn with_corosync_conf(mut self, path: impl Into<PathBuf>) -> Self {
        self.corosync_conf = path.into();
        self
    }

    async fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = timeout(DEADLINE, command.output()).await.map_err(|_| {
            ClusterError::backend(anyhow::anyhow!(
                "{program} did not answer within {} seconds. The cluster stack on this node \
                 may have hung; `systemctl status corosync pacemaker` will say.",
                DEADLINE.as_secs()
            ))
        })?;
        let output = output.map_err(|err| {
            ClusterError::backend(anyhow::anyhow!("could not run {program}: {err}"))
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ClusterError::backend(anyhow::anyhow!(
                "{program} failed: {}",
                stderr.lines().last().unwrap_or("no output").trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// A privileged command, delegated to systemd. Same shape as the storage
    /// domain's: outcome checked, the tool's own last sentence reported.
    async fn run_privileged(&self, description: String, request: ExecRequest) -> Result<()> {
        let outcome = self
            .exec
            .run(request)
            .await
            .map_err(ClusterError::Backend)?;
        if !outcome.ok() {
            return Err(ClusterError::Conflict(format!(
                "{description}: {}",
                outcome.failure()
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl ClusterBackend for CliBackend {
    async fn cluster_state(&self, name: &str) -> Result<ClusterState> {
        // This node can only speak for the cluster it is in; the environment
        // view reaches other clusters through their own members.
        let conf = tokio::fs::read_to_string(&self.corosync_conf)
            .await
            .map_err(|err| {
                ClusterError::Conflict(format!(
                    "This node is not running a cluster — {} is not readable: {err}",
                    self.corosync_conf.display()
                ))
            })?;
        match parse_cluster_name(&conf) {
            Some(local) if local == name => {}
            Some(local) => {
                return Err(ClusterError::Conflict(format!(
                    "This node is a member of \"{local}\", not \"{name}\" — ask a member of \
                     \"{name}\"."
                )))
            }
            None => {
                return Err(ClusterError::backend(anyhow::anyhow!(
                    "{} carries no cluster_name",
                    self.corosync_conf.display()
                )))
            }
        }

        let quorum_out = self.run(&self.quorumtool, &["-s"]).await?;
        let cfg_out = self.run(&self.cfgtool, &["-s"]).await?;
        let crm_out = self
            .run(&self.crm_mon, &["--output-as=xml", "--inactive"])
            .await?;

        let quorum = parse_quorumtool(&quorum_out);
        let local_rings = parse_cfgtool(&cfg_out);
        let (mut nodes, fence_devices) = parse_crm_mon(&crm_out)?;

        // cfgtool answers for this node only; peers' rings arrive with the
        // environment federation, and until then an empty list is honest.
        let local = crate::state::hostname();
        if let Some(node) = nodes.iter_mut().find(|n| n.name == local) {
            node.rings = local_rings;
        }

        Ok(ClusterState {
            name: name.to_string(),
            quorum,
            nodes,
            fence_devices,
        })
    }

    async fn local_preflight(&self) -> Result<LocalPreflight> {
        // chrony first: a node that cannot answer for its clock is refused
        // by preflight, and the sentence should be chrony's, not a guess.
        let tracking = self.run(&self.chronyc, &["-c", "tracking"]).await?;
        let (time_synchronized, time_offset_ms) = parse_chronyc_tracking(&tracking);
        Ok(LocalPreflight {
            time_synchronized,
            time_offset_ms,
            already_clustered: self.corosync_conf.exists(),
        })
    }

    async fn write_cluster_config(&self, conf: &str, authkey: &str) -> Result<()> {
        // `install` reads the unit's standard input and writes the target
        // with the mode asked for — content over the pipe, never an
        // argument, and no shell anywhere. `-D` makes /etc/corosync on a
        // node where the package did not.
        self.run_privileged(
            "writing the cluster configuration failed".into(),
            ExecRequest::new("write the cluster configuration", INSTALL)
                .args(["-D", "-m", "0644", "/dev/stdin", COROSYNC_CONF])
                .stdin(conf),
        )
        .await?;
        self.run_privileged(
            "writing the cluster key failed".into(),
            ExecRequest::new("write the cluster authentication key", INSTALL)
                .args(["-m", "0600", "/dev/stdin", COROSYNC_AUTHKEY])
                .stdin(authkey),
        )
        .await
    }

    async fn enable_stack(&self) -> Result<()> {
        self.run_privileged(
            "starting the cluster stack failed".into(),
            ExecRequest::new("enable and start the cluster stack", SYSTEMCTL).args([
                "enable",
                "--now",
                "corosync",
                "pacemaker",
            ]),
        )
        .await
    }

    async fn disable_stack(&self) -> Result<()> {
        // Pacemaker first: stopping corosync out from under a running
        // Pacemaker is a fencing story, not a shutdown.
        self.run_privileged(
            "stopping the cluster stack failed".into(),
            ExecRequest::new("stop and disable the cluster stack", SYSTEMCTL).args([
                "disable",
                "--now",
                "pacemaker",
                "corosync",
            ]),
        )
        .await
    }

    async fn remove_cluster_config(&self) -> Result<()> {
        self.run_privileged(
            "removing the cluster configuration failed".into(),
            ExecRequest::new("remove the cluster configuration", RM).args([
                "-rf",
                COROSYNC_CONF,
                COROSYNC_AUTHKEY,
                PACEMAKER_CIB,
            ]),
        )
        .await
    }

    async fn set_pacemaker_properties(&self, properties: &[(String, String)]) -> Result<()> {
        if properties.is_empty() {
            return Ok(());
        }
        let mut request =
            ExecRequest::new("set the cluster properties", PCS).args(["property", "set"]);
        for (key, value) in properties {
            request = request.arg(format!("{key}={value}"));
        }
        self.run_privileged("setting the cluster properties failed".into(), request)
            .await
    }

    async fn create_vip(
        &self,
        cluster: &str,
        address: std::net::Ipv4Addr,
        prefix: u8,
    ) -> Result<()> {
        self.run_privileged(
            "creating the cluster address failed".into(),
            ExecRequest::new("create the cluster address", PCS).args([
                "resource",
                "create",
                &format!("{cluster}-vip"),
                "ocf:heartbeat:IPaddr2",
                &format!("ip={address}"),
                &format!("cidr_netmask={prefix}"),
                "op",
                "monitor",
                "interval=10s",
            ]),
        )
        .await
    }

    async fn create_fence_device(
        &self,
        device: &crate::topology::FenceDevice,
        password: &str,
    ) -> Result<()> {
        // `pcs stonith create` would put the password in an argument vector,
        // which lands in the journal and /proc. cibadmin reads the resource
        // as XML from the unit's standard input instead — the same
        // content-over-the-pipe rule as the corosync key, and the password
        // then exists only where Pacemaker keeps its CIB.
        self.run_privileged(
            format!("creating the fence device for {} failed", device.target),
            ExecRequest::new("create a fence device", CIBADMIN)
                .args(["--create", "--scope", "resources", "--xml-pipe"])
                .stdin(fence_primitive_xml(device, password)),
        )
        .await?;
        // A fence device must not run on the node it powers off: fencing a
        // node that hosts its own executioner is a race the cluster loses.
        self.run_privileged(
            format!("placing the fence device for {} failed", device.target),
            ExecRequest::new("keep a fence device off its target", CIBADMIN)
                .args(["--create", "--scope", "constraints", "--xml-pipe"])
                .stdin(fence_location_xml(device)),
        )
        .await
    }

    async fn fence_node(&self, target: &str) -> Result<()> {
        self.run_privileged(
            format!("fencing {target} failed"),
            ExecRequest::new("fence a node", PCS).args(["stonith", "fence", target]),
        )
        .await
    }

    async fn confirm_node_dead(&self, target: &str) -> Result<()> {
        // --force answers pcs's own are-you-sure prompt; the human
        // confirmation this operation actually rests on happened in the
        // console, against the typed name of the node.
        self.run_privileged(
            format!("confirming {target} dead failed"),
            ExecRequest::new("confirm a node is dead", PCS)
                .args(["stonith", "confirm", target, "--force"]),
        )
        .await
    }
}

/// The CIB XML for one `fence_ipmilan` primitive. Rendered here, next to the
/// parsers, as a free function over the device — the password is a parameter
/// precisely so nothing above this line ever holds it together with anything
/// that logs. The 60-second monitor is the continuous BMC connectivity check:
/// its failure is what flips `FenceDeviceState::failed` in `crm_mon`.
fn fence_primitive_xml(device: &crate::topology::FenceDevice, password: &str) -> String {
    use quick_xml::escape::escape;
    let id = &device.id;
    let mut xml = format!(
        r#"<primitive id="{}" class="stonith" type="fence_ipmilan">
  <instance_attributes id="{}-attrs">
    <nvpair id="{}-ip" name="ip" value="{}"/>
    <nvpair id="{}-username" name="username" value="{}"/>
    <nvpair id="{}-password" name="password" value="{}"/>
    <nvpair id="{}-lanplus" name="lanplus" value="1"/>
    <nvpair id="{}-host" name="pcmk_host_list" value="{}"/>
"#,
        escape(id.as_str()),
        escape(id.as_str()),
        escape(id.as_str()),
        escape(device.bmc_address.as_str()),
        escape(id.as_str()),
        escape(device.bmc_username.as_str()),
        escape(id.as_str()),
        escape(password),
        escape(id.as_str()),
        escape(id.as_str()),
        escape(device.target.as_str()),
    );
    if device.delay_base_secs > 0 {
        // The fence-race bias: the peer waits this long before killing this
        // device's target. Emitted only when the topology engine set it —
        // a zero here would still be an attribute an operator has to read.
        xml.push_str(&format!(
            "    <nvpair id=\"{}-delay\" name=\"pcmk_delay_base\" value=\"{}s\"/>\n",
            escape(id.as_str()),
            device.delay_base_secs
        ));
    }
    xml.push_str(&format!(
        r#"  </instance_attributes>
  <operations>
    <op id="{}-monitor" name="monitor" interval="60s"/>
  </operations>
</primitive>
"#,
        escape(id.as_str()),
    ));
    xml
}

/// The location constraint keeping a fence device off its own target.
fn fence_location_xml(device: &crate::topology::FenceDevice) -> String {
    use quick_xml::escape::escape;
    format!(
        "<rsc_location id=\"{}-placement\" rsc=\"{}\" node=\"{}\" score=\"-INFINITY\"/>\n",
        escape(device.id.as_str()),
        escape(device.id.as_str()),
        escape(device.target.as_str()),
    )
}

/// The `cluster_name` out of a corosync.conf.
fn parse_cluster_name(conf: &str) -> Option<String> {
    conf.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("cluster_name:"))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// `corosync-quorumtool -s`. Line-oriented; the flags line is where the
/// regime shows itself: `2Node` and `WaitForAll` are the two-node mechanisms
/// read back from the running cluster.
fn parse_quorumtool(output: &str) -> QuorumState {
    let mut state = QuorumState::default();
    for line in output.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Quorate:") {
            state.quorate = value.trim().eq_ignore_ascii_case("yes");
        } else if let Some(value) = line.strip_prefix("Expected votes:") {
            state.expected_votes = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("Total votes:") {
            state.votes = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("Flags:") {
            let flags: Vec<&str> = value.split_whitespace().collect();
            state.two_node = flags.contains(&"2Node");
            state.wait_for_all = flags.contains(&"WaitForAll");
        }
    }
    state
}

/// `corosync-cfgtool -s`: this node's knet links and whether each peer is
/// connected on them. A link is connected when every peer on it is —
/// "localhost" is this node's own entry and does not count either way.
fn parse_cfgtool(output: &str) -> Vec<RingLink> {
    let mut links: Vec<RingLink> = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("LINK ID") {
            let id = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            links.push(RingLink {
                link: id,
                address: String::new(),
                connected: true,
            });
        } else if let Some(current) = links.last_mut() {
            if let Some(addr) = trimmed.strip_prefix("addr") {
                current.address = addr.trim_start_matches(['=', ' ', '\t']).trim().to_string();
            } else if trimmed.starts_with("nodeid:") {
                let status = trimmed.rsplit([':', '\t', ' ']).next().unwrap_or("");
                if status != "localhost" && status != "connected" {
                    current.connected = false;
                }
            }
        }
    }
    links
}

/// `chronyc -c tracking`: one CSV line. The two answers preflight needs are
/// the leap status (the last field) and the system-time offset in seconds
/// (the fifth) — everything between is for eyes reading `chronyc tracking`
/// without `-c`.
fn parse_chronyc_tracking(output: &str) -> (bool, Option<i64>) {
    let line = output.trim();
    let fields: Vec<&str> = line.split(',').collect();
    if fields.len() < 6 {
        return (false, None);
    }
    let synchronized = fields
        .last()
        .is_some_and(|leap| leap.trim().eq_ignore_ascii_case("normal"));
    let offset_ms = fields[4]
        .trim()
        .parse::<f64>()
        .ok()
        .map(|seconds| (seconds * 1000.0).round() as i64);
    (synchronized, offset_ms)
}

/// `crm_mon --output-as=xml`: node membership and the STONITH devices.
/// Pacemaker's one machine-readable status format — the text form is for
/// eyes and changes between releases; this one carries an api-version.
fn parse_crm_mon(xml: &str) -> Result<(Vec<NodeState>, Vec<FenceDeviceState>)> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut nodes = Vec::new();
    let mut devices = Vec::new();
    // The <node> elements under <nodes> are members; the ones nested inside
    // a <resource> only say where it runs. Depth-free flag on the section.
    let mut in_nodes = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let tag = name.as_ref();
                let attr = |wanted: &str| -> Option<String> {
                    e.attributes().flatten().find_map(|a| {
                        (a.key.as_ref() == wanted.as_bytes())
                            .then(|| String::from_utf8_lossy(&a.value).into_owned())
                    })
                };
                let flag = |wanted: &str| attr(wanted).as_deref() == Some("true");

                match tag {
                    b"nodes" => in_nodes = true,
                    b"node" if in_nodes => nodes.push(NodeState {
                        name: attr("name").unwrap_or_default(),
                        online: flag("online"),
                        standby: flag("standby"),
                        unclean: flag("unclean"),
                        rings: Vec::new(),
                    }),
                    b"resource" => {
                        let agent = attr("resource_agent").unwrap_or_default();
                        if agent.starts_with("stonith:") {
                            let id = attr("id").unwrap_or_default();
                            let target = id.strip_prefix("fence-").unwrap_or(&id).to_string();
                            devices.push(FenceDeviceState {
                                target,
                                device: id,
                                active: flag("active"),
                                failed: flag("failed"),
                                last_test: None,
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"nodes" => in_nodes = false,
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(err) => {
                return Err(ClusterError::backend(anyhow::anyhow!(
                    "crm_mon answered something that is not its XML: {err}"
                )))
            }
        }
    }
    Ok((nodes, devices))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// corosync 3.1 on EL10, two-node cluster, both members up.
    const QUORUMTOOL_TWO_NODE: &str = "\
Quorum information
------------------
Date:             Mon Jul 20 10:10:21 2026
Quorum provider:  corosync_votequorum
Nodes:            2
Node ID:          1
Ring ID:          1.e
Quorate:          Yes

Votequorum information
----------------------
Expected votes:   2
Highest expected: 2
Total votes:      2
Quorum:           1
Flags:            2Node Quorate WaitForAll

Membership information
----------------------
    Nodeid      Votes Name
         1          1 alpha-1 (local)
         2          1 alpha-2
";

    /// The same tool on a three-node cluster that has lost a member and its
    /// quorum with it.
    const QUORUMTOOL_MINORITY: &str = "\
Quorum information
------------------
Date:             Mon Jul 20 10:31:44 2026
Quorum provider:  corosync_votequorum
Nodes:            1
Node ID:          2
Ring ID:          2.11
Quorate:          No

Votequorum information
----------------------
Expected votes:   3
Highest expected: 3
Total votes:      1
Quorum:           2 Activity blocked
Flags:

Membership information
----------------------
    Nodeid      Votes Name
         2          1 beta-2 (local)
";

    /// corosync-cfgtool -s on EL10: knet transport, two links, one peer
    /// disconnected on the second.
    const CFGTOOL_KNET: &str = "\
Local node ID 1, transport knet
LINK ID 0 udp
\taddr\t= 10.10.0.1
\tstatus:
\t\tnodeid:          1:\tlocalhost
\t\tnodeid:          2:\tconnected
LINK ID 1 udp
\taddr\t= 192.168.10.1
\tstatus:
\t\tnodeid:          1:\tlocalhost
\t\tnodeid:          2:\tdisconnected
";

    /// chronyc -c tracking, chrony 4.6 on EL10: synchronized, 23 µs off.
    const CHRONYC_SYNCED: &str = "A29FC87B,192.168.10.250,2,1753500000.123456789,\
-0.000023614,0.000012000,0.000150000,0.023,0.001,0.010,0.000250000,0.000800000,64.2,Normal\n";

    /// The same node before chrony has a source: reference 127.127.1.1,
    /// stratum 0, and the leap status says it plainly.
    const CHRONYC_UNSYNCED: &str = "7F7F0101,,0,1753500000.000000000,\
0.000000000,0.000000000,0.000000000,0.000,0.000,0.000,0.000000000,0.000000000,0.0,\
Not synchronised\n";

    /// crm_mon --output-as=xml on EL10 (pacemaker 2.1), trimmed to the
    /// elements this parser reads: a healthy member, a lost-and-unfenced
    /// member, one running fence device and one failed.
    const CRM_MON_XML: &str = r#"<pacemaker-result api-version="2.32" request="crm_mon --output-as=xml --inactive">
  <summary>
    <stack type="corosync" pacemakerd-state="running"/>
    <current_dc present="true" version="2.1.9-1.el10-7188dbeb82" name="alpha-1" id="1" with_quorum="true" mixed_versions="false"/>
    <nodes_configured number="2"/>
    <resources_configured number="2" disabled="0" blocked="0"/>
    <cluster_options stonith-enabled="true" symmetric-cluster="true" no-quorum-policy="stop" maintenance-mode="false" stop-all-resources="false" stonith-timeout-ms="60000" priority-fencing-delay-ms="0"/>
  </summary>
  <nodes>
    <node name="alpha-1" id="1" online="true" standby="false" standby_onfail="false" maintenance="false" pending="false" unclean="false" health="green" feature_set="3.19.0" shutdown="false" expected_up="true" is_dc="true" resources_running="1" type="member"/>
    <node name="alpha-2" id="2" online="false" standby="false" standby_onfail="false" maintenance="false" pending="false" unclean="true" health="green" feature_set="3.19.0" shutdown="false" expected_up="true" is_dc="false" resources_running="0" type="member"/>
  </nodes>
  <resources>
    <resource id="fence-alpha-1" resource_agent="stonith:fence_ipmilan" role="Started" target_role="Started" active="true" orphaned="false" blocked="false" maintenance="false" managed="true" failed="false" failure_ignored="false" nodes_running_on="1">
      <node name="alpha-2" id="2" cached="true"/>
    </resource>
    <resource id="fence-alpha-2" resource_agent="stonith:fence_ipmilan" role="Stopped" active="false" orphaned="false" blocked="false" maintenance="false" managed="true" failed="true" failure_ignored="false" nodes_running_on="0"/>
  </resources>
</pacemaker-result>
"#;

    #[test]
    fn a_two_node_quorum_reads_back_with_both_mechanisms() {
        let quorum = parse_quorumtool(QUORUMTOOL_TWO_NODE);
        assert!(quorum.quorate);
        assert_eq!(quorum.votes, 2);
        assert_eq!(quorum.expected_votes, 2);
        assert!(quorum.two_node);
        assert!(quorum.wait_for_all);
    }

    #[test]
    fn a_partitioned_minority_reads_as_not_quorate_and_not_two_node() {
        let quorum = parse_quorumtool(QUORUMTOOL_MINORITY);
        assert!(!quorum.quorate);
        assert_eq!(quorum.votes, 1);
        assert_eq!(quorum.expected_votes, 3);
        assert!(!quorum.two_node);
        assert!(!quorum.wait_for_all);
    }

    #[test]
    fn knet_links_parse_with_their_addresses_and_peer_health() {
        let links = parse_cfgtool(CFGTOOL_KNET);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].link, 0);
        assert_eq!(links[0].address, "10.10.0.1");
        assert!(links[0].connected);
        assert_eq!(links[1].link, 1);
        assert_eq!(links[1].address, "192.168.10.1");
        // One peer disconnected makes the link degraded, and localhost —
        // this node's own entry — never counts either way.
        assert!(!links[1].connected);
    }

    #[test]
    fn a_synchronized_clock_reads_with_its_offset() {
        let (synced, offset) = parse_chronyc_tracking(CHRONYC_SYNCED);
        assert!(synced);
        // -0.000023614 s rounds to 0 ms — the point is the magnitude, and
        // sub-millisecond is what a healthy LAN looks like.
        assert_eq!(offset, Some(0));
    }

    #[test]
    fn an_unsynchronized_clock_says_so_rather_than_passing() {
        let (synced, _) = parse_chronyc_tracking(CHRONYC_UNSYNCED);
        assert!(!synced);
        // Garbage is "not synchronized", never a panic.
        assert!(!parse_chronyc_tracking("").0);
        assert!(!parse_chronyc_tracking("one,two").0);
    }

    #[test]
    fn crm_mon_yields_members_and_fence_devices_but_never_confuses_the_two() {
        let (nodes, devices) = parse_crm_mon(CRM_MON_XML).unwrap();
        // The <node> inside a <resource> says where it runs; it is not a
        // third member.
        assert_eq!(nodes.len(), 2);
        assert!(nodes[0].online && !nodes[0].unclean);
        assert!(!nodes[1].online && nodes[1].unclean);

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].device, "fence-alpha-1");
        assert_eq!(devices[0].target, "alpha-1");
        assert!(devices[0].active && !devices[0].failed);
        assert!(!devices[1].active && devices[1].failed);
    }

    #[test]
    fn the_cluster_name_comes_out_of_the_conf() {
        let conf = "totem {\n    version: 2\n    cluster_name: alpha\n}\n";
        assert_eq!(parse_cluster_name(conf).as_deref(), Some("alpha"));
        assert_eq!(parse_cluster_name("totem {\n}\n"), None);
    }

    #[test]
    fn something_that_is_not_crm_mon_xml_is_an_error_not_a_panic() {
        assert!(parse_crm_mon("<unclosed").is_err());
    }

    /// The one command in this crate where getting an argument wrong writes
    /// the wrong file: content rides standard input, the mode and target are
    /// arguments, and there is no shell for anything to escape into.
    #[tokio::test]
    async fn the_configuration_is_written_over_stdin_never_as_an_argument() {
        let exec = lumen_sys::exec::MockExec::working();
        let backend = CliBackend::new(exec.clone());
        backend
            .write_cluster_config("totem { }", "the-key-material")
            .await
            .unwrap();

        let ran = exec.ran().await;
        assert_eq!(ran.len(), 2);
        assert_eq!(ran[0].program, INSTALL);
        assert_eq!(
            ran[0].args,
            vec!["-D", "-m", "0644", "/dev/stdin", COROSYNC_CONF]
        );
        assert_eq!(ran[0].stdin.as_deref(), Some("totem { }"));
        assert_eq!(
            ran[1].args,
            vec!["-m", "0600", "/dev/stdin", COROSYNC_AUTHKEY]
        );
        // The key is content, not an argument — nothing to leak into a
        // journal line or /proc.
        assert_eq!(ran[1].stdin.as_deref(), Some("the-key-material"));
        assert!(!ran[1].args.iter().any(|a| a.contains("the-key-material")));
    }

    #[tokio::test]
    async fn the_stack_is_enabled_started_and_undone_symmetrically() {
        let exec = lumen_sys::exec::MockExec::working();
        let backend = CliBackend::new(exec.clone());
        backend.enable_stack().await.unwrap();
        backend.disable_stack().await.unwrap();
        backend.remove_cluster_config().await.unwrap();

        let ran = exec.ran().await;
        assert!(
            exec.ran_with(SYSTEMCTL, &["enable", "--now", "corosync", "pacemaker"])
                .await
        );
        // Pacemaker stops before corosync: the reverse is a fencing story.
        assert!(
            exec.ran_with(SYSTEMCTL, &["disable", "--now", "pacemaker", "corosync"])
                .await
        );
        assert_eq!(ran[2].program, RM);
    }

    #[tokio::test]
    async fn properties_and_the_vip_go_through_pcs() {
        let exec = lumen_sys::exec::MockExec::working();
        let backend = CliBackend::new(exec.clone());
        backend
            .set_pacemaker_properties(&[
                ("stonith-enabled".into(), "true".into()),
                ("no-quorum-policy".into(), "stop".into()),
            ])
            .await
            .unwrap();
        backend
            .create_vip("alpha", "192.168.10.100".parse().unwrap(), 24)
            .await
            .unwrap();

        let ran = exec.ran().await;
        assert_eq!(ran[0].program, PCS);
        assert!(ran[0].args.contains(&"stonith-enabled=true".to_string()));
        assert!(ran[1].args.contains(&"ip=192.168.10.100".to_string()));
        assert!(ran[1].args.contains(&"cidr_netmask=24".to_string()));
    }

    /// The other place a secret could leak into an argument vector: the BMC
    /// password rides inside CIB XML over the unit's standard input, and the
    /// argv both cibadmin calls get is fixed text.
    #[tokio::test]
    async fn the_bmc_password_rides_the_cib_xml_over_stdin_never_as_an_argument() {
        let exec = lumen_sys::exec::MockExec::working();
        let backend = CliBackend::new(exec.clone());
        let device = crate::topology::FenceDevice {
            id: "fence-alpha-1".into(),
            target: "alpha-1".into(),
            bmc_address: "10.20.0.1".into(),
            bmc_username: "ADMIN".into(),
            delay_base_secs: 10,
        };
        backend
            .create_fence_device(&device, "s3cret&<pass>")
            .await
            .unwrap();

        let ran = exec.ran().await;
        assert_eq!(ran.len(), 2);
        assert_eq!(ran[0].program, CIBADMIN);
        assert_eq!(
            ran[0].args,
            vec!["--create", "--scope", "resources", "--xml-pipe"]
        );
        let xml = ran[0].stdin.as_deref().unwrap();
        // The password is in the piped XML — escaped, so the CIB parses it
        // back to exactly what the operator typed — and in no argument.
        assert!(xml.contains("s3cret&amp;&lt;pass&gt;"), "{xml}");
        assert!(!ran[0].args.iter().any(|a| a.contains("s3cret")));
        assert!(xml.contains(r#"name="pcmk_host_list" value="alpha-1""#));
        assert!(xml.contains(r#"name="pcmk_delay_base" value="10s""#));
        // The continuous BMC connectivity check.
        assert!(xml.contains(r#"name="monitor" interval="60s""#));

        // The constraint keeps the device off the node it powers off.
        let constraint = ran[1].stdin.as_deref().unwrap();
        assert!(
            constraint.contains(r#"rsc="fence-alpha-1" node="alpha-1" score="-INFINITY""#),
            "{constraint}"
        );
    }

    /// The unpreferred device carries no delay attribute at all: a zero
    /// would still be a knob an operator has to read and reason about.
    #[test]
    fn an_undelayed_device_omits_the_delay_rather_than_writing_zero() {
        let device = crate::topology::FenceDevice {
            id: "fence-beta-2".into(),
            target: "beta-2".into(),
            bmc_address: "10.20.0.2".into(),
            bmc_username: "ADMIN".into(),
            delay_base_secs: 0,
        };
        assert!(!fence_primitive_xml(&device, "pw").contains("pcmk_delay_base"));
    }

    #[tokio::test]
    async fn a_live_fence_and_a_confirmation_go_through_pcs() {
        let exec = lumen_sys::exec::MockExec::working();
        let backend = CliBackend::new(exec.clone());
        backend.fence_node("alpha-2").await.unwrap();
        backend.confirm_node_dead("alpha-2").await.unwrap();

        assert!(exec.ran_with(PCS, &["stonith", "fence", "alpha-2"]).await);
        assert!(
            exec.ran_with(PCS, &["stonith", "confirm", "alpha-2", "--force"])
                .await
        );
    }

    /// A node that leaves a cluster leaves nothing behind for the next one:
    /// the CIB goes with the corosync configuration, or stale fence devices
    /// — old BMC passwords included — would resurrect into a new cluster.
    #[tokio::test]
    async fn removing_the_configuration_takes_the_cib_with_it() {
        let exec = lumen_sys::exec::MockExec::working();
        let backend = CliBackend::new(exec.clone());
        backend.remove_cluster_config().await.unwrap();
        assert!(
            exec.ran_with(RM, &["-rf", COROSYNC_CONF, COROSYNC_AUTHKEY, PACEMAKER_CIB])
                .await
        );
    }

    #[tokio::test]
    async fn a_failed_transient_unit_reports_the_tools_own_sentence() {
        let exec = lumen_sys::exec::MockExec::working();
        exec.fail_next(
            1,
            "Failed to enable unit: Unit corosync.service does not exist.",
        )
        .await;
        let backend = CliBackend::new(exec);
        let err = backend.enable_stack().await.unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }
}
