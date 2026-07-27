//! The supported command line: what corosync and Pacemaker report, parsed.
//!
//! Reads only, at this stage, and unprivileged ones — `crm_mon`,
//! `corosync-quorumtool`, and `corosync-cfgtool` answer root without leaving
//! the sandbox, because they talk to their daemons over sockets that
//! `ProtectSystem=strict` does not cover. The privileged verbs (`pcs`, fence
//! agents) arrive with the join workflow and go through `lumen_sys::exec`.
//!
//! Every parser is a free function over `&str`, outside the impl, so it
//! tests without a process. The fixtures in the tests are the formats EL10
//! ships: corosync 3.1 and pacemaker 2.1, the HighAvailability stack this
//! appliance pins.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::timeout;

use super::ClusterBackend;
use crate::environment::EnvironmentMembership;
use crate::error::{ClusterError, Result};
use crate::state::{ClusterState, FenceDeviceState, NodeState, QuorumState, RingLink};

/// Reads answer in milliseconds on a healthy node and not at all on one whose
/// cluster stack has hung — and an unbounded read here would hang the whole
/// environment view. Same reasoning as `lumen_zfs::backend::cli::DEADLINE`.
const DEADLINE: Duration = Duration::from_secs(30);

/// The file the join workflow keeps the membership record in, under the
/// control plane's state directory.
pub const MEMBERSHIP_FILE: &str = "environment.json";

pub struct CliBackend {
    crm_mon: String,
    quorumtool: String,
    cfgtool: String,
    corosync_conf: PathBuf,
    membership_path: PathBuf,
}

impl CliBackend {
    pub fn new(state_dir: &Path) -> Self {
        CliBackend {
            crm_mon: "/usr/sbin/crm_mon".into(),
            quorumtool: "/usr/sbin/corosync-quorumtool".into(),
            cfgtool: "/usr/sbin/corosync-cfgtool".into(),
            corosync_conf: PathBuf::from("/etc/corosync/corosync.conf"),
            membership_path: state_dir.join(MEMBERSHIP_FILE),
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
}

#[async_trait]
impl ClusterBackend for CliBackend {
    async fn membership(&self) -> Result<Option<EnvironmentMembership>> {
        let raw = match tokio::fs::read_to_string(&self.membership_path).await {
            Ok(raw) => raw,
            // No record is the ordinary standalone appliance.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(ClusterError::backend(anyhow::anyhow!(
                    "could not read {}: {err}",
                    self.membership_path.display()
                )))
            }
        };
        let membership = serde_json::from_str(&raw).map_err(|err| {
            ClusterError::backend(anyhow::anyhow!(
                "{} does not hold a membership record: {err}",
                self.membership_path.display()
            ))
        })?;
        Ok(Some(membership))
    }

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

    #[tokio::test]
    async fn a_node_with_no_record_is_standalone_not_broken() {
        let dir = std::env::temp_dir().join(format!(
            "lumen-cluster-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let backend = CliBackend::new(&dir);
        assert_eq!(backend.membership().await.unwrap(), None);

        // A corrupt record is an error that names the file, not a silent
        // fall back to standalone — losing the environment quietly would be
        // far worse than an error message.
        std::fs::write(dir.join(MEMBERSHIP_FILE), "not json").unwrap();
        let err = backend.membership().await.unwrap_err();
        assert!(err.to_string().contains(MEMBERSHIP_FILE), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
