//! The workflows that change an environment: joining it, and building a
//! cluster inside it.
//!
//! The domain crate never speaks HTTP. Everything that crosses to another
//! node goes through [`PeerChannel`] — a trait the control plane implements
//! with its TLS client and tests implement in memory — so the whole engine
//! here, unwind included, runs under `cargo test` without a network, a
//! corosync, or a second machine. The local node is a peer like any other:
//! the channel implementation short-circuits calls to itself, and the engine
//! does not care.
//!
//! ## The shape of a create
//!
//! Preflight every member → validate → generate (corosync.conf from the
//! topology engine, a fresh authkey) → prepare every member → start every
//! member → wait for the cluster to form → set Pacemaker properties → create
//! the fence devices → write the membership record. **The record is written
//! last**, which is what makes "a half-created cluster is not a representable
//! state" true: any failure before the record unwinds the members to exactly
//! where they started, and the record never knew the cluster existed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::backend::ClusterBackend;
use crate::environment::{ClusterRecord, EnvironmentMembership, EnvironmentNode};
use crate::error::{ClusterError, Result};
use crate::model::ClusterDefinition;
use crate::networks::ClusterNetworks;
use crate::topology::ClusterTopology;

/// How long the coordinator waits for a freshly started cluster to become
/// quorate with every member online before declaring the create failed.
pub const FORM_DEADLINE: Duration = Duration::from_secs(120);

/// What one node reports when asked "could you join a cluster right now?".
/// The links ride along so the wizard's NIC pickers show what each member
/// actually has, carrier state included.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightReport {
    pub node: String,
    pub controlplane_version: String,
    /// The hostname the node itself reports — compared against the record,
    /// because corosync matches node names against it.
    pub hostname: String,
    /// chrony reports the clock is synchronized.
    pub time_synchronized: bool,
    /// Offset from the reference, milliseconds, when chrony would say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_offset_ms: Option<i64>,
    /// A corosync configuration already exists on the node.
    pub already_clustered: bool,
    pub links: Vec<lumen_net::ObservedLink>,
}

/// A preflight report, judged. `problems` are complete sentences the wizard
/// pins to the node; an empty list is a node that may proceed. `report` is
/// absent when the node could not be asked at all — which is itself the
/// problem.
#[derive(Debug, Clone, Serialize)]
pub struct PreflightView {
    pub node: String,
    pub ok: bool,
    pub problems: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<PreflightReport>,
}

/// The clock skew beyond which a member is refused. corosync tolerates more,
/// but certificate validation and every log an operator will ever read do
/// not.
pub const MAX_TIME_OFFSET_MS: i64 = 1_000;

/// Judge one report against the coordinator's expectations.
pub fn judge_preflight(
    report: &PreflightReport,
    expected_node: &str,
    coordinator_version: &str,
) -> PreflightView {
    let mut problems = Vec::new();
    if report.hostname != expected_node {
        problems.push(format!(
            "The node calls itself \"{}\", not \"{expected_node}\" — corosync matches on the \
             hostname, so the record and the node must agree.",
            report.hostname
        ));
    }
    if report.controlplane_version != coordinator_version {
        problems.push(format!(
            "It runs control plane {}, this node runs {coordinator_version} — update it first; \
             a cluster is built from one version.",
            report.controlplane_version
        ));
    }
    if !report.time_synchronized {
        problems.push(
            "Its clock is not synchronized. Fix chrony first — certificates and every log \
             depend on the members agreeing what time it is."
                .to_string(),
        );
    } else if let Some(offset) = report.time_offset_ms {
        if offset.abs() > MAX_TIME_OFFSET_MS {
            problems.push(format!(
                "Its clock is {offset} ms from the reference — outside the {MAX_TIME_OFFSET_MS} \
                 ms a cluster is built with."
            ));
        }
    }
    if report.already_clustered {
        problems.push(
            "It already carries a corosync configuration. A node belongs to at most one \
             cluster, and leftovers from an old one must be removed deliberately, not \
             overwritten."
                .to_string(),
        );
    }
    PreflightView {
        node: expected_node.to_string(),
        ok: problems.is_empty(),
        problems,
        report: Some(report.clone()),
    }
}

/// The Core-network seat a member is given during prepare: the one piece of
/// addressing the workflow creates rather than adopts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreAssignment {
    pub interface: String,
    pub address: std::net::Ipv4Addr,
    pub prefix: u8,
    pub mtu: u32,
}

/// Everything one member needs to become part of the cluster, pushed in one
/// call so a node is prepared whole or not at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparePayload {
    pub cluster: String,
    pub corosync_conf: String,
    /// The corosync authentication key. Text rather than raw bytes on
    /// purpose: 256 characters drawn from a 62-symbol alphabet carry ~1500
    /// bits of entropy — far beyond the key's need — and text survives every
    /// pipe and JSON body between here and `/etc/corosync/authkey` without
    /// an encoding step.
    pub authkey: String,
    pub core: CoreAssignment,
}

/// What teardown needs to know to put a node back exactly as it was.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeardownPayload {
    pub cluster: String,
    /// The Core interface whose address the prepare assigned, to be released
    /// again. `None` tears down configuration only.
    pub core_interface: Option<String>,
}

/// A regenerated configuration for a member already in the cluster — the
/// scale-out's reach into the running nodes: write it, reload corosync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconfigurePayload {
    pub cluster: String,
    pub corosync_conf: String,
    /// The cluster's existing key, unchanged — rewritten alongside the conf
    /// because they travel as one file pair everywhere else.
    pub authkey: String,
}

/// What a joining node sends the issuer, over the fingerprint-pinned
/// connection the token authorises.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    pub token_id: String,
    pub secret: String,
    pub node: String,
    pub address: String,
    pub controlplane_version: String,
}

/// What the issuer answers with: the environment, whole. The response
/// deliberately carries the CA key and the session secret — every member can
/// mint tokens and sign joins, and one console session works everywhere —
/// which is why the channel it rides is authenticated *before* the request
/// is sent, by the fingerprint in the token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinGrant {
    pub membership: EnvironmentMembership,
    pub ca_pem: String,
    pub ca_key_pem: String,
    pub node_cert_pem: String,
    pub node_key_pem: String,
    /// Base64 of the environment's shared web-session secret.
    pub session_secret: String,
}

/// The seam between the workflows and the other nodes. The control plane
/// implements it over TLS; tests implement it in memory. The local node is
/// addressed like any other member.
#[async_trait]
pub trait PeerChannel: Send + Sync {
    /// The join handshake: called by a node that is not yet in the
    /// environment, against the issuer named in the token, verified by the
    /// fingerprint pinned in the token.
    async fn request_join(
        &self,
        issuer: &str,
        fingerprint: &str,
        request: &JoinRequest,
    ) -> Result<JoinGrant>;

    async fn preflight(&self, node: &EnvironmentNode) -> Result<PreflightReport>;
    async fn prepare(&self, node: &EnvironmentNode, payload: &PreparePayload) -> Result<()>;
    async fn start(&self, node: &EnvironmentNode) -> Result<()>;
    async fn teardown(&self, node: &EnvironmentNode, payload: &TeardownPayload) -> Result<()>;

    /// Push a regenerated configuration to a running member and reload its
    /// corosync — the scale-out's write to the nodes already in.
    async fn reconfigure(&self, node: &EnvironmentNode, payload: &ReconfigurePayload)
        -> Result<()>;

    /// Push a membership record to a peer; the peer reconciles and answers
    /// with its own. Both directions of gossip are this one call.
    async fn push_membership(
        &self,
        node: &EnvironmentNode,
        membership: &EnvironmentMembership,
    ) -> Result<EnvironmentMembership>;

    /// Hand a peer one machine's definition to keep — the HA manager's
    /// restart inventory, filled at define time because libvirt on a dead
    /// node cannot be asked for it. The definition carries its home node.
    async fn store_definition(
        &self,
        node: &EnvironmentNode,
        definition: &crate::store::StoredDefinition,
    ) -> Result<()>;

    /// Tell a peer to forget one.
    async fn drop_definition(&self, node: &EnvironmentNode, vmid: u32) -> Result<()>;
}

// --- create progress --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    Running,
    Done,
    Failed,
    /// Ran, then was reverted by the unwind.
    Unwound,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepProgress {
    /// A short verb the console renders: `preflight`, `prepare`, `start`,
    /// `form`, `properties`, `record`, `unwind`.
    pub step: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub state: StepState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPhase {
    Running,
    Complete,
    /// Failed, and the unwind has finished: every member is back where it
    /// started.
    Failed,
}

/// The whole of a create, as the console polls it.
#[derive(Debug, Clone, Serialize)]
pub struct CreateProgress {
    pub cluster: String,
    pub phase: WorkflowPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub steps: Vec<StepProgress>,
}

/// Shared handle the engine writes and the progress endpoint reads.
#[derive(Clone, Default)]
pub struct ProgressHandle {
    inner: Arc<Mutex<Option<CreateProgress>>>,
}

impl ProgressHandle {
    pub fn snapshot(&self) -> Option<CreateProgress> {
        self.inner.lock().unwrap().clone()
    }

    /// Whether a create is currently running.
    pub fn busy(&self) -> bool {
        matches!(
            self.snapshot(),
            Some(CreateProgress {
                phase: WorkflowPhase::Running,
                ..
            })
        )
    }

    pub fn begin(&self, progress: CreateProgress) {
        *self.inner.lock().unwrap() = Some(progress);
    }

    fn update(&self, f: impl FnOnce(&mut CreateProgress)) {
        if let Some(progress) = self.inner.lock().unwrap().as_mut() {
            f(progress);
        }
    }

    /// Update one step. `pub(crate)`: the create engine here and the
    /// scale-out engine in the service both drive the same progress.
    pub(crate) fn set_step(
        &self,
        step: &str,
        node: Option<&str>,
        state: StepState,
        detail: Option<String>,
    ) {
        self.update(|progress| {
            if let Some(entry) = progress
                .steps
                .iter_mut()
                .find(|s| s.step == step && s.node.as_deref() == node)
            {
                entry.state = state;
                entry.detail = detail;
            }
        });
    }

    fn append_step(&self, step: StepProgress) {
        self.update(|progress| progress.steps.push(step));
    }

    fn finish(&self, phase: WorkflowPhase, error: Option<String>) {
        self.update(|progress| {
            progress.phase = phase;
            progress.error = error;
        });
    }

    /// Close the workflow out — the scale-out engine's ending.
    pub(crate) fn finish_workflow(&self, phase: WorkflowPhase, error: Option<String>) {
        self.finish(phase, error);
    }
}

/// A fresh corosync authentication key. See [`PreparePayload::authkey`] for
/// why it is text.
pub fn generate_authkey() -> String {
    use rand::distributions::Alphanumeric;
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(256)
        .map(char::from)
        .collect()
}

// --- the create engine ------------------------------------------------------

/// Run a validated create to completion or all the way back. Returns the
/// membership record to commit; the caller persists and gossips it. `poll`
/// is how often the form step re-asks — tests shrink it.
#[allow(clippy::too_many_arguments)]
pub async fn run_create(
    definition: ClusterDefinition,
    networks: ClusterNetworks,
    bmc_passwords: std::collections::BTreeMap<String, String>,
    membership: EnvironmentMembership,
    coordinator_version: &str,
    peers: &dyn PeerChannel,
    backend: &dyn ClusterBackend,
    progress: &ProgressHandle,
    poll: Duration,
) -> Result<EnvironmentMembership> {
    let name = definition.name.clone();
    let member_names = definition.node_names();
    let members: Vec<EnvironmentNode> = member_names
        .iter()
        .filter_map(|n| membership.node(n).cloned())
        .collect();

    progress.begin(plan_progress(&definition));

    match drive(
        &definition,
        &networks,
        &bmc_passwords,
        &membership,
        &members,
        coordinator_version,
        peers,
        backend,
        progress,
        poll,
    )
    .await
    {
        Ok(updated) => {
            progress.finish(WorkflowPhase::Complete, None);
            Ok(updated)
        }
        Err((failed_after_prepare, err)) => {
            // Unwind everything that was touched, in reverse. Best effort per
            // node — one node refusing to clean up must not leave the others
            // dirty — and every teardown is its own visible step.
            for node in members.iter().take(failed_after_prepare).rev() {
                progress.append_step(StepProgress {
                    step: "unwind".into(),
                    node: Some(node.name.clone()),
                    state: StepState::Running,
                    detail: None,
                });
                let payload = TeardownPayload {
                    cluster: name.clone(),
                    core_interface: networks
                        .core
                        .members
                        .iter()
                        .find(|m| m.node == node.name)
                        .map(|m| m.interface.clone()),
                };
                match peers.teardown(node, &payload).await {
                    Ok(()) => progress.set_step("unwind", Some(&node.name), StepState::Done, None),
                    Err(unwind_err) => progress.set_step(
                        "unwind",
                        Some(&node.name),
                        StepState::Failed,
                        Some(format!(
                            "could not clean up: {unwind_err} — the node may need \
                             `systemctl disable --now corosync pacemaker` by hand"
                        )),
                    ),
                }
            }
            for node in &member_names {
                for phase in ["prepare", "start"] {
                    progress.update(|p| {
                        if let Some(entry) = p
                            .steps
                            .iter_mut()
                            .find(|s| s.step == phase && s.node.as_deref() == Some(node))
                        {
                            if entry.state == StepState::Done {
                                entry.state = StepState::Unwound;
                            }
                        }
                    });
                }
            }
            progress.finish(WorkflowPhase::Failed, Some(err.to_string()));
            Err(err)
        }
    }
}

fn step(name: &str, node: Option<&String>) -> StepProgress {
    StepProgress {
        step: name.to_string(),
        node: node.map(String::to_string),
        state: StepState::Pending,
        detail: None,
    }
}

/// The full plan, laid out before anything runs, so the console shows what
/// is coming rather than a log of what already happened. The service begins
/// the progress with this before spawning the engine, so the first poll
/// already has every step.
pub fn plan_progress(definition: &ClusterDefinition) -> CreateProgress {
    let member_names = definition.node_names();
    let mut steps = Vec::new();
    for node in &member_names {
        steps.push(step("preflight", Some(node)));
    }
    steps.push(step("generate", None));
    for node in &member_names {
        steps.push(step("prepare", Some(node)));
    }
    for node in &member_names {
        steps.push(step("start", Some(node)));
    }
    steps.push(step("form", None));
    steps.push(step("properties", None));
    for node in &member_names {
        steps.push(step("fence", Some(node)));
    }
    steps.push(step("record", None));
    CreateProgress {
        cluster: definition.name.clone(),
        phase: WorkflowPhase::Running,
        error: None,
        steps,
    }
}

/// The forward path. On failure, returns how many members had been prepared
/// (and possibly started) so the unwind knows exactly how far to reach.
#[allow(clippy::too_many_arguments)]
async fn drive(
    definition: &ClusterDefinition,
    networks: &ClusterNetworks,
    bmc_passwords: &std::collections::BTreeMap<String, String>,
    membership: &EnvironmentMembership,
    members: &[EnvironmentNode],
    coordinator_version: &str,
    peers: &dyn PeerChannel,
    backend: &dyn ClusterBackend,
    progress: &ProgressHandle,
    poll: Duration,
) -> std::result::Result<EnvironmentMembership, (usize, ClusterError)> {
    let name = &definition.name;

    if members.len() != definition.nodes.len() {
        let missing: Vec<&str> = definition
            .nodes
            .iter()
            .filter(|n| membership.node(&n.name).is_none())
            .map(|n| n.name.as_str())
            .collect();
        return Err((
            0,
            ClusterError::Conflict(format!(
                "Not every member is in the environment: {}.",
                missing.join(", ")
            )),
        ));
    }

    // Preflight, every node, all problems collected before anything runs.
    let mut blockers = Vec::new();
    for node in members {
        progress.set_step("preflight", Some(&node.name), StepState::Running, None);
        match peers.preflight(node).await {
            Ok(report) => {
                let view = judge_preflight(&report, &node.name, coordinator_version);
                if view.ok {
                    progress.set_step("preflight", Some(&node.name), StepState::Done, None);
                } else {
                    let detail = view.problems.join(" ");
                    progress.set_step(
                        "preflight",
                        Some(&node.name),
                        StepState::Failed,
                        Some(detail.clone()),
                    );
                    blockers.push(format!("{}: {detail}", node.name));
                }
            }
            Err(err) => {
                progress.set_step(
                    "preflight",
                    Some(&node.name),
                    StepState::Failed,
                    Some(err.to_string()),
                );
                blockers.push(format!("{}: {err}", node.name));
            }
        }
    }
    if !blockers.is_empty() {
        return Err((
            0,
            ClusterError::Conflict(format!("Preflight failed. {}", blockers.join(" "))),
        ));
    }

    // Generate. The conf comes from the topology engine — the same renderer
    // the property tests hold — and the key is fresh per cluster.
    progress.set_step("generate", None, StepState::Running, None);
    let topology = ClusterTopology::new(definition.clone());
    let conf = topology.corosync_conf();
    let authkey = generate_authkey();
    progress.set_step("generate", None, StepState::Done, None);

    // Prepare, then start, counting how far we got for the unwind.
    let mut touched = 0usize;
    for node in members {
        progress.set_step("prepare", Some(&node.name), StepState::Running, None);
        let core = networks
            .core
            .members
            .iter()
            .find(|m| m.node == node.name)
            .map(|m| CoreAssignment {
                interface: m.interface.clone(),
                address: m.address,
                prefix: networks.core.subnet.prefix,
                mtu: networks.core.mtu,
            })
            .expect("validated: every member has a core seat");
        let payload = PreparePayload {
            cluster: name.clone(),
            corosync_conf: conf.clone(),
            authkey: authkey.clone(),
            core,
        };
        touched += 1;
        if let Err(err) = peers.prepare(node, &payload).await {
            progress.set_step(
                "prepare",
                Some(&node.name),
                StepState::Failed,
                Some(err.to_string()),
            );
            return Err((
                touched,
                ClusterError::Conflict(format!("Preparing {} failed: {err}", node.name)),
            ));
        }
        progress.set_step("prepare", Some(&node.name), StepState::Done, None);
    }

    for node in members {
        progress.set_step("start", Some(&node.name), StepState::Running, None);
        if let Err(err) = peers.start(node).await {
            progress.set_step(
                "start",
                Some(&node.name),
                StepState::Failed,
                Some(err.to_string()),
            );
            return Err((
                members.len(),
                ClusterError::Conflict(format!(
                    "Starting the stack on {} failed: {err}",
                    node.name
                )),
            ));
        }
        progress.set_step("start", Some(&node.name), StepState::Done, None);
    }

    // Form: the cluster is not created until corosync says every member is
    // in it and votequorum says it is quorate.
    progress.set_step("form", None, StepState::Running, None);
    let deadline = std::time::Instant::now() + FORM_DEADLINE;
    loop {
        match backend.cluster_state(name).await {
            Ok(state)
                if state.quorum.quorate
                    && definition
                        .nodes
                        .iter()
                        .all(|n| state.node(&n.name).is_some_and(|s| s.online)) =>
            {
                break;
            }
            _ if std::time::Instant::now() > deadline => {
                progress.set_step(
                    "form",
                    None,
                    StepState::Failed,
                    Some("the members never formed a quorate cluster".into()),
                );
                return Err((
                    members.len(),
                    ClusterError::Conflict(format!(
                        "The cluster started but never formed within {} seconds — check the \
                         Core network between the members.",
                        FORM_DEADLINE.as_secs()
                    )),
                ));
            }
            _ => tokio::time::sleep(poll).await,
        }
    }
    progress.set_step("form", None, StepState::Done, None);

    // Properties, and the Management VIP when one was asked for. Local pcs:
    // the coordinator is a member, and the CIB is cluster-wide.
    progress.set_step("properties", None, StepState::Running, None);
    if let Err(err) = backend
        .set_pacemaker_properties(&topology.pacemaker_properties())
        .await
    {
        progress.set_step("properties", None, StepState::Failed, Some(err.to_string()));
        return Err((
            members.len(),
            ClusterError::Conflict(format!("Setting cluster properties failed: {err}")),
        ));
    }
    if let Some(vip) = networks.management.vip {
        if let Err(err) = backend
            .create_vip(name, vip, networks.management.subnet.prefix)
            .await
        {
            progress.set_step("properties", None, StepState::Failed, Some(err.to_string()));
            return Err((
                members.len(),
                ClusterError::Conflict(format!("Creating the cluster address failed: {err}")),
            ));
        }
    }
    progress.set_step("properties", None, StepState::Done, None);

    // Fencing: one fence_ipmilan device per member, written into the CIB
    // with its BMC password — the only place the password goes. STONITH was
    // enabled a step ago, so the cluster is not left enabled-but-deviceless
    // for longer than this loop takes.
    for device in topology.fence_devices() {
        progress.set_step("fence", Some(&device.target), StepState::Running, None);
        let password = bmc_passwords
            .get(&device.target)
            .map(String::as_str)
            .unwrap_or_default();
        if let Err(err) = backend.create_fence_device(&device, password).await {
            progress.set_step(
                "fence",
                Some(&device.target),
                StepState::Failed,
                Some(err.to_string()),
            );
            return Err((
                members.len(),
                ClusterError::Conflict(format!(
                    "Creating the fence device for {} failed: {err}",
                    device.target
                )),
            ));
        }
        progress.set_step("fence", Some(&device.target), StepState::Done, None);
    }

    // Record, last. Failure before this line leaves the record untouched.
    progress.set_step("record", None, StepState::Running, None);
    let mut updated = membership.clone();
    updated.version += 1;
    for node in &mut updated.nodes {
        if definition.node(&node.name).is_some() {
            node.cluster = Some(name.clone());
        }
    }
    updated
        .clusters
        .push(ClusterRecord::new(definition.clone(), networks.clone()));
    progress.set_step("record", None, StepState::Done, None);
    Ok(updated)
}

// --- in-memory peers --------------------------------------------------------

/// Peers that exist only in memory, for tests — this crate's and the control
/// plane's, which is why it is compiled unconditionally like the backend
/// mocks.
pub struct MockPeers {
    inner: Mutex<MockPeersInner>,
    /// When set, a successful start of every member registers the formed
    /// cluster into this backend, the way corosync forming would make it
    /// visible to the coordinator's reads.
    backend: Option<Arc<crate::backend::mock::MockBackend>>,
}

#[derive(Default)]
struct MockPeersInner {
    reports: std::collections::HashMap<String, PreflightReport>,
    fail_prepare: Option<String>,
    fail_start: Option<String>,
    fail_teardown: Option<String>,
    prepared: Vec<(String, PreparePayload)>,
    started: Vec<String>,
    torn_down: Vec<(String, TeardownPayload)>,
    reconfigured: Vec<(String, ReconfigurePayload)>,
    grant: Option<JoinGrant>,
    join_requests: Vec<(String, String, JoinRequest)>,
    definitions: Vec<(String, crate::store::StoredDefinition)>,
    dropped_definitions: Vec<(String, u32)>,
}

impl MockPeers {
    pub fn new() -> Self {
        MockPeers {
            inner: Mutex::new(MockPeersInner::default()),
            backend: None,
        }
    }

    /// Wire the peers to a mock backend so a completed start makes the
    /// cluster observable, as forming does on a real coordinator.
    pub fn with_backend(mut self, backend: Arc<crate::backend::mock::MockBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// A healthy report for one node, links included.
    pub fn with_report(self, node: &str, report: PreflightReport) -> Self {
        self.inner
            .lock()
            .unwrap()
            .reports
            .insert(node.to_string(), report);
        self
    }

    pub fn with_healthy_node(self, node: &str, version: &str, links: &[&str]) -> Self {
        let report = healthy_report(node, version, links);
        self.with_report(node, report)
    }

    pub fn fail_prepare_on(self, node: &str) -> Self {
        self.inner.lock().unwrap().fail_prepare = Some(node.to_string());
        self
    }

    pub fn fail_start_on(self, node: &str) -> Self {
        self.inner.lock().unwrap().fail_start = Some(node.to_string());
        self
    }

    pub fn fail_teardown_on(self, node: &str) -> Self {
        self.inner.lock().unwrap().fail_teardown = Some(node.to_string());
        self
    }

    pub fn with_grant(self, grant: JoinGrant) -> Self {
        self.inner.lock().unwrap().grant = Some(grant);
        self
    }

    pub fn prepared(&self) -> Vec<(String, PreparePayload)> {
        self.inner.lock().unwrap().prepared.clone()
    }

    pub fn started(&self) -> Vec<String> {
        self.inner.lock().unwrap().started.clone()
    }

    pub fn torn_down(&self) -> Vec<(String, TeardownPayload)> {
        self.inner.lock().unwrap().torn_down.clone()
    }

    pub fn reconfigured(&self) -> Vec<(String, ReconfigurePayload)> {
        self.inner.lock().unwrap().reconfigured.clone()
    }

    pub fn join_requests(&self) -> Vec<(String, String, JoinRequest)> {
        self.inner.lock().unwrap().join_requests.clone()
    }

    /// Every definition pushed to a peer: (peer, definition).
    pub fn definitions(&self) -> Vec<(String, crate::store::StoredDefinition)> {
        self.inner.lock().unwrap().definitions.clone()
    }

    pub fn dropped_definitions(&self) -> Vec<(String, u32)> {
        self.inner.lock().unwrap().dropped_definitions.clone()
    }

    fn maybe_form(&self) {
        let inner = self.inner.lock().unwrap();
        let Some(backend) = &self.backend else { return };
        let Some((_, payload)) = inner.prepared.first() else {
            return;
        };
        let prepared: Vec<String> = inner.prepared.iter().map(|(n, _)| n.clone()).collect();
        if prepared.iter().all(|n| inner.started.contains(n)) {
            backend.register_cluster(crate::backend::mock::formed_cluster(
                &payload.cluster,
                &prepared.iter().map(String::as_str).collect::<Vec<_>>(),
            ));
        }
    }
}

impl Default for MockPeers {
    fn default() -> Self {
        Self::new()
    }
}

/// A healthy preflight report for tests and scenario builders.
pub fn healthy_report(node: &str, version: &str, links: &[&str]) -> PreflightReport {
    PreflightReport {
        node: node.to_string(),
        controlplane_version: version.to_string(),
        hostname: node.to_string(),
        time_synchronized: true,
        time_offset_ms: Some(3),
        already_clustered: false,
        links: links
            .iter()
            .map(|name| lumen_net::ObservedLink {
                name: (*name).to_string(),
                kind: lumen_net::LinkKind::Ethernet,
                state: lumen_net::LinkState::Activated,
                carrier: true,
                ..lumen_net::ObservedLink::default()
            })
            .collect(),
    }
}

#[async_trait]
impl PeerChannel for MockPeers {
    async fn request_join(
        &self,
        issuer: &str,
        fingerprint: &str,
        request: &JoinRequest,
    ) -> Result<JoinGrant> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .join_requests
            .push((issuer.to_string(), fingerprint.to_string(), request.clone()));
        inner.grant.clone().ok_or_else(|| {
            ClusterError::Conflict("The issuer did not answer the join.".to_string())
        })
    }

    async fn preflight(&self, node: &EnvironmentNode) -> Result<PreflightReport> {
        self.inner
            .lock()
            .unwrap()
            .reports
            .get(&node.name)
            .cloned()
            .ok_or_else(|| {
                ClusterError::Conflict(format!("\"{}\" did not answer the preflight.", node.name))
            })
    }

    async fn prepare(&self, node: &EnvironmentNode, payload: &PreparePayload) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_prepare.as_deref() == Some(node.name.as_str()) {
            return Err(ClusterError::Conflict(format!(
                "the transient unit writing the configuration on \"{}\" failed",
                node.name
            )));
        }
        inner.prepared.push((node.name.clone(), payload.clone()));
        Ok(())
    }

    async fn start(&self, node: &EnvironmentNode) -> Result<()> {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.fail_start.as_deref() == Some(node.name.as_str()) {
                return Err(ClusterError::Conflict(format!(
                    "corosync on \"{}\" refused to start",
                    node.name
                )));
            }
            inner.started.push(node.name.clone());
        }
        self.maybe_form();
        Ok(())
    }

    async fn teardown(&self, node: &EnvironmentNode, payload: &TeardownPayload) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_teardown.as_deref() == Some(node.name.as_str()) {
            return Err(ClusterError::Conflict(format!(
                "\"{}\" is not answering",
                node.name
            )));
        }
        inner.torn_down.push((node.name.clone(), payload.clone()));
        Ok(())
    }

    async fn reconfigure(
        &self,
        node: &EnvironmentNode,
        payload: &ReconfigurePayload,
    ) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .reconfigured
            .push((node.name.clone(), payload.clone()));
        Ok(())
    }

    async fn push_membership(
        &self,
        _node: &EnvironmentNode,
        membership: &EnvironmentMembership,
    ) -> Result<EnvironmentMembership> {
        Ok(membership.clone())
    }

    async fn store_definition(
        &self,
        node: &EnvironmentNode,
        definition: &crate::store::StoredDefinition,
    ) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .definitions
            .push((node.name.clone(), definition.clone()));
        Ok(())
    }

    async fn drop_definition(&self, node: &EnvironmentNode, vmid: u32) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .dropped_definitions
            .push((node.name.clone(), vmid));
        Ok(())
    }
}
