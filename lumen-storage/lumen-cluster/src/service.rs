//! The clustering domain's one entry point.
//!
//! The control plane's handlers deserialize, call one method here, and
//! serialize the answer — no validation, no corosync, nothing above this
//! line. The reads present the environment; the writes are the environment's
//! own workflows: minting a join token, joining, building a cluster,
//! destroying one. Everything that touches another node goes through the
//! [`PeerChannel`] the control plane injects, and everything that touches
//! this node goes through the backend — which is what lets all of it run
//! against the in-memory implementations under `cargo test`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::Mutex;

use crate::backend::ClusterBackend;
use crate::environment::{
    hash_secret, issue_node_certificate, mint_ca, pem_fingerprint, random_hex, ClusterRecord,
    EnvironmentMembership, EnvironmentNode, JoinToken, Maintenance, TOKEN_TTL_SECS,
};
use crate::error::{ClusterError, Result};
use crate::join::{
    judge_preflight, run_create, CreateProgress, JoinGrant, JoinRequest, PeerChannel,
    PreflightReport, PreflightView, PreparePayload, ProgressHandle, StepProgress, StepState,
    TeardownPayload,
};
use crate::model::Regime;
use crate::networks::{
    valid_bridge_name, validate_networks, ClusterNetworks, CoreNetwork, CoreNetworkUpdate,
    ExternalNetwork, VlanMode,
};
use crate::state::{
    hostname, ClusterState, FenceDeviceState, FenceTest, QuorumState, RingLink, VipState,
};
use crate::store::{EnvironmentStore, Identity, JoinTokenRecord};
use crate::validate::{
    validate_definition, Acknowledgements, ClusterCreate, ValidationCode, ValidationError,
};

/// GET /api/environment. The whole environment in one answer: grouped by
/// cluster, then by node — the repo's "grouped by node" shape extended one
/// level upward, exactly as the comment on `lumen_zfs::PoolsResponse`
/// promised it would be.
#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentResponse {
    /// `None` on a node that never joined an environment — today's
    /// standalone appliance, which is a valid state, not an empty one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentView>,
    pub clusters: Vec<ClusterView>,
    /// Environment nodes not assigned to any cluster: valid standalone
    /// hypervisors, listed rather than hidden. A node with no environment
    /// appears here too, alone — the console renders one shape either way.
    pub unassigned: Vec<UnassignedNodeView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentView {
    pub id: String,
    /// The membership record's last-writer-wins counter.
    pub version: u64,
    pub nodes: usize,
}

/// Overall cluster health, derived here so the console and the API can never
/// disagree about what a card's pill says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterHealth {
    Ok,
    /// Running, but something needs attention: a node down or in standby, a
    /// ring degraded, a fence device failing or never tested.
    Degraded,
    /// Not quorate, or a member is lost and not yet fenced.
    Critical,
    /// The cluster could not be asked — presented honestly rather than
    /// dropped or guessed at.
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClusterView {
    pub name: String,
    pub regime: Regime,
    pub health: ClusterHealth,
    pub quorum: QuorumState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_node: Option<String>,
    pub nodes: Vec<ClusterNodeView>,
    pub fence: FenceSummaryView,
    /// The cluster VIP: what the definition asked for, and what Pacemaker
    /// has actually done about it.
    ///
    /// Absent when the definition names no VIP. Present with `state: None`
    /// when it names one and Pacemaker has no such resource — which is a
    /// fault, not an absence, and the console says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vip: Option<VipView>,
    /// Why the cluster's state could not be read, when it could not. The
    /// nodes are then listed from the membership record with nothing claimed
    /// about them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The cluster VIP, definition and reality side by side.
///
/// Both halves, because either one alone misleads. The definition alone says
/// a VIP exists when Pacemaker may have stopped it; Pacemaker alone cannot
/// say which address was meant when the resource is missing entirely.
#[derive(Debug, Clone, Serialize)]
pub struct VipView {
    /// The address the cluster's Management network defines.
    pub address: std::net::Ipv4Addr,
    /// What Pacemaker reports about the resource, when it has one at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<VipState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClusterNodeView {
    pub node: String,
    pub online: bool,
    pub standby: bool,
    /// Lost and not yet fenced — the state HA waits on and the break-glass
    /// confirm exists for.
    pub unclean: bool,
    pub rings: Vec<RingLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fence: Option<FenceDeviceState>,
    /// From the membership record: where this node's own console answers,
    /// and the control plane version it last reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controlplane_version: Option<String>,
    /// Set while the operator has this node out of service. Read from the
    /// membership record rather than from Pacemaker, so it is known even when
    /// the cluster itself cannot be asked — which is exactly the state a node
    /// being worked on tends to produce.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<Maintenance>,
    /// This is the node answering the request.
    pub local: bool,
}

/// POST/DELETE /api/environment/clusters/{name}/nodes/{node}/maintenance.
#[derive(Debug, Clone, Serialize)]
pub struct MaintenanceView {
    pub node: String,
    pub cluster: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<Maintenance>,
    /// Whether the rest of the cluster would still be quorate without this
    /// node's vote. Maintenance itself never costs the vote — corosync keeps
    /// running — but the reboot the operator is heading towards does, and
    /// this is where they find that out while it is still a decision.
    pub quorum_safe: bool,
}

/// The fencing panel's one-line summary. IPMI is the only fence path, so a
/// failing or untested device is cluster-level news and counted here rather
/// than left for the operator to find in a table.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FenceSummaryView {
    pub devices: usize,
    pub healthy: usize,
    pub failed: usize,
    /// Devices never live-tested. Nonzero pins the persistent warning.
    pub untested: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnassignedNodeView {
    pub node: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controlplane_version: Option<String>,
    pub local: bool,
}

/// POST /api/environment/tokens.
#[derive(Debug, Clone, Serialize)]
pub struct MintedToken {
    /// The one string the operator copies to the joining node's console.
    pub token: String,
    /// Unix seconds.
    pub expires_at: u64,
    /// This mint bootstrapped the environment — the control plane reloads
    /// its TLS listener onto the new certificate when it sees this.
    pub bootstrapped: bool,
}

/// What a completed join hands back to the control plane, which owns the
/// live session secret and the TLS listener.
#[derive(Debug)]
pub struct JoinOutcome {
    pub session_secret: Vec<u8>,
}

/// A validated node-add, ready to run: everything `execute_add_node` needs,
/// computed — and its progress begun — before anything was touched.
pub struct AddNodePlan {
    pub cluster: String,
    old_definition: crate::model::ClusterDefinition,
    definition: crate::model::ClusterDefinition,
    networks: crate::networks::ClusterNetworks,
    newcomer: EnvironmentNode,
    bmc_password: String,
    core: crate::join::CoreAssignment,
}

fn plan_step(name: &str, node: Option<&str>) -> StepProgress {
    StepProgress {
        step: name.to_string(),
        node: node.map(str::to_string),
        state: StepState::Pending,
        detail: None,
    }
}

/// POST /api/environment/clusters/{name}/fence/{node}/test — what a guarded
/// live fence test answered. `passed: false` is a successful request that
/// learned something bad; the transport error, when there was one, rides in
/// `error`.
#[derive(Debug, Clone, Serialize)]
pub struct FenceTestView {
    pub cluster: String,
    pub node: String,
    pub passed: bool,
    /// Unix seconds.
    pub at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct ClusterService {
    backend: Arc<dyn ClusterBackend>,
    peers: Arc<dyn PeerChannel>,
    network: Arc<lumen_net::NetworkService>,
    store: EnvironmentStore,
    node: String,
    controlplane_version: String,
    progress: ProgressHandle,
    form_poll: Duration,
    /// Serializes writes. Two operators clicking Create at once must not
    /// race to two half-clusters.
    gate: Mutex<()>,
}

impl ClusterService {
    pub fn new(
        backend: Arc<dyn ClusterBackend>,
        peers: Arc<dyn PeerChannel>,
        network: Arc<lumen_net::NetworkService>,
        state_dir: &Path,
        controlplane_version: &str,
    ) -> Self {
        ClusterService {
            backend,
            peers,
            network,
            store: EnvironmentStore::new(state_dir),
            node: hostname(),
            controlplane_version: controlplane_version.to_string(),
            progress: ProgressHandle::default(),
            form_poll: Duration::from_secs(2),
            gate: Mutex::new(()),
        }
    }

    /// Pretend to be another node, for tests.
    pub fn with_node(mut self, node: impl Into<String>) -> Self {
        self.node = node.into();
        self
    }

    /// Seed the environment state, for tests — the shape a join would have
    /// left behind.
    pub fn with_environment(self, membership: &EnvironmentMembership) -> Self {
        self.store
            .save_membership(membership)
            .expect("test store is writable");
        self
    }

    /// Shrink the form-step poll, for tests.
    pub fn with_form_poll(mut self, poll: Duration) -> Self {
        self.form_poll = poll;
        self
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    /// The environment credential material, for the control plane's TLS
    /// listener and peer client. `None` until this node bootstraps or joins.
    pub fn identity(&self) -> Result<Option<Identity>> {
        self.store.load_identity()
    }

    /// Where the environment serving certificate lives, for TLS reloads.
    pub fn serving_cert_paths(&self) -> (PathBuf, PathBuf) {
        (self.store.node_cert_path(), self.store.node_key_path())
    }

    fn membership(&self) -> Result<Option<EnvironmentMembership>> {
        self.store.load_membership()
    }

    /// Every node in this environment, local one included, for a caller that
    /// wants to ask each of them the same question.
    ///
    /// A node that has not joined anything answers with itself and no
    /// address: it is an environment of one, and a reader that has to
    /// special-case "no membership yet" is a reader that will forget to.
    pub fn environment_nodes(&self) -> Result<Vec<EnvironmentNode>> {
        Ok(self
            .membership()?
            .map(|membership| membership.nodes)
            .unwrap_or_default())
    }

    fn require_membership(&self) -> Result<EnvironmentMembership> {
        self.membership()?.ok_or_else(|| {
            ClusterError::Conflict(
                "This node has not joined an environment. Mint a token on an environment node \
                 and join first."
                    .to_string(),
            )
        })
    }

    // --- reads ------------------------------------------------------------

    /// The whole environment: every cluster, every node, grouped.
    pub async fn environment(&self) -> Result<EnvironmentResponse> {
        let Some(membership) = self.membership()? else {
            // No environment. The node still exists and the console still
            // renders it — as the one unassigned node of an environment of
            // nobody.
            return Ok(EnvironmentResponse {
                environment: None,
                clusters: Vec::new(),
                unassigned: vec![UnassignedNodeView {
                    node: self.node.clone(),
                    address: None,
                    controlplane_version: Some(self.controlplane_version.clone()),
                    local: true,
                }],
            });
        };

        let mut clusters = Vec::new();
        for name in membership.cluster_names() {
            clusters.push(self.cluster_view(&name, &membership).await);
        }

        let unassigned = membership
            .unassigned()
            .map(|node| self.unassigned_view(node))
            .collect();

        Ok(EnvironmentResponse {
            environment: Some(EnvironmentView {
                id: membership.id.clone(),
                version: membership.version,
                nodes: membership.nodes.len(),
            }),
            clusters,
            unassigned,
        })
    }

    /// One cluster, by name. The same view the environment answer carries —
    /// the detail page is the card, opened.
    pub async fn cluster(&self, name: &str) -> Result<ClusterView> {
        let membership = self.require_membership()?;
        if !membership.cluster_names().iter().any(|c| c == name) {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{name}\" in this environment."
            )));
        }
        Ok(self.cluster_view(name, &membership).await)
    }

    /// This node's own corosync links, one per ring.
    ///
    /// Exists because `corosync-cfgtool -s` answers for the node it is run on
    /// and no other. A cluster view assembled from one member therefore knows
    /// one member's link health and nothing about the rest, which is why a
    /// healthy two-node cluster reads as "Connected · 1 unknown" — the peer's
    /// ring was never asked about, not found down.
    ///
    /// Carried in the environment-wide inventory so every member's answer
    /// arrives alongside every other, and the unknowns resolve. Empty on a
    /// node that is not in a cluster, or whose stack cannot be asked: an empty
    /// list is honest, and this must never fail a read that is mostly about
    /// something else.
    pub async fn local_rings(&self) -> Vec<RingLink> {
        let Ok(Some(membership)) = self.environment_record() else {
            return Vec::new();
        };
        let Some(cluster) = membership
            .node(&self.node)
            .and_then(|n| n.cluster.as_deref())
        else {
            return Vec::new();
        };
        let Ok(state) = self.backend.cluster_state(cluster).await else {
            return Vec::new();
        };
        state
            .node(&self.node)
            .map(|node| node.rings.clone())
            .unwrap_or_default()
    }

    /// One cluster's typed networks — Core, Management, and the External
    /// list — as the replicated record carries them. This is the definition
    /// the members share; what each node has realized of it is the
    /// networking domain's answer, not this one.
    pub fn cluster_networks(&self, name: &str) -> Result<ClusterNetworks> {
        let membership = self.require_membership()?;
        if !membership.cluster_names().iter().any(|c| c == name) {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{name}\" in this environment."
            )));
        }
        // Named but recordless: the membership knows the cluster exists and
        // its definition has not reached this node yet — a replication gap,
        // not a wrong name.
        let record = membership.cluster_record(name).ok_or_else(|| {
            ClusterError::Conflict(format!(
                "\"{name}\" has no stored definition on this node yet. Ask another member, or \
                 ask again once the record has replicated."
            ))
        })?;
        Ok(record.networks.clone())
    }

    /// Clear the cluster VIP's recorded failures and let Pacemaker probe
    /// it again.
    ///
    /// The operation an operator needs after fixing whatever stopped the
    /// address, and it exists because fixing the cause is not enough on its
    /// own. Pacemaker latches a failed operation: the `rc_text` the console
    /// shows — "Not installed" when the agent is missing something it shells
    /// out to — stays in the node history, and the resource is left alone
    /// until somebody forgets it. Installing the missing tool changes nothing
    /// until then, which is a trap worth a button rather than a paragraph of
    /// documentation.
    ///
    /// Deliberately not a repair: this asks again, it does not make the
    /// answer different. A recovery run before the cause is fixed re-probes,
    /// fails the same way, and the view says so — which is the honest
    /// outcome, not a failure of this call.
    pub async fn recover_vip(&self, cluster: &str) -> Result<VipView> {
        let _guard = self.gate.lock().await;
        let membership = self.require_membership()?;
        let Some(record) = membership.cluster_record(cluster) else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        let Some(address) = record.networks.management.vip else {
            return Err(ClusterError::Conflict(format!(
                "\"{cluster}\" defines no cluster VIP, so there is nothing to recover."
            )));
        };

        // From inside the cluster: pcs writes reach the CIB from any member,
        // and only from a member. Same rule as the fence test.
        if membership
            .node(&self.node)
            .and_then(|n| n.cluster.as_deref())
            != Some(cluster)
        {
            return Err(ClusterError::Conflict(format!(
                "A recovery runs from inside the cluster. Open the console of a member of \
                 \"{cluster}\"."
            )));
        }

        let state = self.backend.cluster_state(cluster).await?;
        let Some(vip) = state.vip.as_ref() else {
            return Err(ClusterError::Conflict(format!(
                "Pacemaker has no address resource for \"{cluster}\" at all. There is no failure \
                 to clear — the resource is missing, which is a different fault: the cluster was \
                 built without it, or something removed it."
            )));
        };

        self.backend.cleanup_resource(&vip.resource).await?;
        tracing::info!(
            cluster = cluster,
            resource = %vip.resource,
            "cleared the cluster VIP's recorded failures"
        );

        // Read back rather than reporting success: the probe is what decides
        // whether the address comes up, and the operator asked about the
        // address, not about the cleanup. A view that still says "Not
        // installed" means the cause is still there, and saying so beats a
        // green toast over an address nobody answers on.
        let state = self.backend.cluster_state(cluster).await?;
        Ok(VipView {
            address,
            state: state.vip,
        })
    }

    /// What one member has to build for an External network, given the port
    /// that member carries it on.
    ///
    /// A trunk bridges the uplink directly and turns on VLAN filtering, which
    /// is what "VLAN aware" means: tagged frames reach the machines. An
    /// access network bridges `nic.N` instead — the bridge itself carries
    /// untagged frames, and the tag is put on and taken off by the VLAN
    /// interface underneath. Bridging the raw uplink for an access network
    /// would put the machines on whatever the switch sends untagged, which is
    /// not the VLAN that was asked for.
    fn external_seat(network: &ExternalNetwork, interface: &str) -> crate::join::ExternalSeat {
        let comment = Some(format!("External network \"{}\"", network.name));
        match network.vlan {
            VlanMode::Trunk { .. } => crate::join::ExternalSeat {
                vlan: None,
                bridge: lumen_net::Bridge {
                    name: network.bridge.clone(),
                    ports: vec![interface.to_string()],
                    vlan_filtering: true,
                    comment,
                    ..lumen_net::Bridge::default()
                },
            },
            VlanMode::Access { vlan } => {
                let name = format!("{interface}.{vlan}");
                crate::join::ExternalSeat {
                    vlan: Some(lumen_net::Vlan {
                        name: name.clone(),
                        parent: interface.to_string(),
                        vlan_id: vlan,
                        comment: comment.clone(),
                        ..lumen_net::Vlan::default()
                    }),
                    bridge: lumen_net::Bridge {
                        name: network.bridge.clone(),
                        ports: vec![name],
                        vlan_filtering: false,
                        comment,
                        ..lumen_net::Bridge::default()
                    },
                }
            }
        }
    }

    /// Define an External network on a cluster and build its bridge on every
    /// member.
    ///
    /// Realized before it is recorded, in that order and deliberately. The
    /// consistency rule the whole type rests on is that an External network
    /// is on every member or on none — a machine restarted onto a node where
    /// its network does not resolve is the failure HA exists to prevent — so
    /// a record written before the boxes agreed with it would be a promise
    /// the cluster had not kept.
    ///
    /// A member that cannot build its bridge fails the call and nothing is
    /// recorded. Bridges already built on other members stay: they are
    /// ordinary links, harmless on their own, and `peer_create_bridge` is
    /// idempotent, so fixing the failing node and asking again finishes the
    /// job rather than tripping over the work already done.
    pub async fn create_external_network(
        &self,
        cluster: &str,
        network: ExternalNetwork,
    ) -> Result<ExternalNetwork> {
        let _guard = self.gate.lock().await;
        let mut membership = self.require_membership()?;
        let Some(record) = membership.cluster_record(cluster).cloned() else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };

        if network.name.trim().is_empty() {
            return Err(ClusterError::Conflict(
                "An External network needs a name — it is what a machine's network refers to."
                    .to_string(),
            ));
        }
        if !valid_bridge_name(&network.bridge) {
            return Err(ClusterError::Conflict(format!(
                "\"{}\" is not a usable bridge name.",
                network.bridge
            )));
        }
        if record
            .networks
            .external
            .iter()
            .any(|existing| existing.name == network.name)
        {
            return Err(ClusterError::Conflict(format!(
                "\"{cluster}\" already defines an External network called \"{}\".",
                network.name
            )));
        }
        if record
            .networks
            .external
            .iter()
            .any(|existing| existing.bridge == network.bridge)
        {
            return Err(ClusterError::Conflict(format!(
                "\"{cluster}\" already has an External network on bridge \"{}\" — two networks \
                 sharing a bridge are one network with two names.",
                network.bridge
            )));
        }

        // Every member or none, checked before anything is built rather than
        // discovered halfway through.
        let members = Self::check_uplinks(&membership, cluster, &network)?;

        // Build it everywhere. The bridge carries no host addressing — that
        // is what makes it an External network rather than a second
        // Management one — and takes its uplink as its only port.
        for member in &members {
            let uplink = network
                .uplinks
                .iter()
                .find(|up| up.node == member.name)
                .expect("every member was checked to have one above");
            let seat = Self::external_seat(&network, &uplink.interface);
            if let Err(err) = self.peers.create_bridge(member, &seat).await {
                return Err(ClusterError::Conflict(format!(
                    "\"{}\" could not build the bridge, so the network was not defined: {err}. \
                     Fix that member and try again — the members that did build it will not \
                     object the second time.",
                    member.name
                )));
            }
        }

        // Recorded only now that every member has it.
        let Some(stored) = membership
            .clusters
            .iter_mut()
            .find(|candidate| candidate.definition.name == cluster)
        else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        stored.networks.external.push(network.clone());
        membership.version += 1;
        self.store.save_membership(&membership)?;
        tracing::info!(
            cluster = cluster,
            network = %network.name,
            bridge = %network.bridge,
            "external network defined on every member"
        );
        Ok(network)
    }

    /// Change an External network's definition and rebuild it on every
    /// member.
    ///
    /// The name is the identity — it is what a machine's adapter refers to —
    /// so it is the one field this does not change. Everything else can move:
    /// the bridge, the VLAN semantics, and which port each member carries it
    /// on.
    ///
    /// Rebuilt everywhere before the record admits the change, the same order
    /// and for the same reason as the create: an External network the record
    /// claims and a member has not built is exactly the state the consistency
    /// rule forbids. What this cannot do is undo a rebuild that succeeded on
    /// one member and failed on the next — the members that changed keep the
    /// new bridge, the record keeps the old definition, and the error names
    /// the member so a retry finishes the job.
    ///
    /// A renamed bridge leaves the old one behind. It is an ordinary link
    /// with machines possibly still attached to it, and tearing it out from
    /// under them is not an edit's business; Interfaces is where a link is
    /// removed, and it is the page that can say what is still on one.
    pub async fn update_external_network(
        &self,
        cluster: &str,
        name: &str,
        network: ExternalNetwork,
    ) -> Result<ExternalNetwork> {
        let _guard = self.gate.lock().await;
        let mut membership = self.require_membership()?;
        let Some(record) = membership.cluster_record(cluster).cloned() else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        if !record.networks.external.iter().any(|e| e.name == name) {
            return Err(ClusterError::NotFound(format!(
                "\"{cluster}\" defines no External network called \"{name}\"."
            )));
        }
        if network.name != name {
            return Err(ClusterError::Conflict(format!(
                "An External network's name is what a machine's adapter refers to, so it cannot \
                 be changed here — renaming it would leave every machine on \"{name}\" pointing \
                 at a network that no longer exists. Define a new one and move the machines onto \
                 it."
            )));
        }
        if !valid_bridge_name(&network.bridge) {
            return Err(ClusterError::Conflict(format!(
                "\"{}\" is not a usable bridge name.",
                network.bridge
            )));
        }
        // Another network's bridge. Its own is fine, and that is most edits.
        if record
            .networks
            .external
            .iter()
            .any(|other| other.name != name && other.bridge == network.bridge)
        {
            return Err(ClusterError::Conflict(format!(
                "\"{cluster}\" already has an External network on bridge \"{}\" — two networks \
                 sharing a bridge are one network with two names.",
                network.bridge
            )));
        }

        let members = Self::check_uplinks(&membership, cluster, &network)?;
        for member in &members {
            let uplink = network
                .uplinks
                .iter()
                .find(|up| up.node == member.name)
                .expect("every member was checked to have one above");
            let seat = Self::external_seat(&network, &uplink.interface);
            if let Err(err) = self.peers.create_bridge(member, &seat).await {
                return Err(ClusterError::Conflict(format!(
                    "\"{}\" could not build the network as changed, so the change was not \
                     recorded: {err}. The members that did build it are on the new definition \
                     and the record is still on the old one — fix that member and ask again.",
                    member.name
                )));
            }
        }

        let Some(stored) = membership
            .clusters
            .iter_mut()
            .find(|candidate| candidate.definition.name == cluster)
        else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        if let Some(slot) = stored.networks.external.iter_mut().find(|e| e.name == name) {
            *slot = network.clone();
        }
        membership.version += 1;
        self.store.save_membership(&membership)?;
        tracing::info!(
            cluster = cluster,
            network = name,
            bridge = %network.bridge,
            "external network changed on every member"
        );
        Ok(network)
    }

    /// Forget an External network.
    ///
    /// The definition goes and the bridges stay. Not an oversight: a bridge
    /// is an ordinary link that machines may still be attached to, and a
    /// definition being removed says nothing about whether those machines
    /// should lose their network. What this ends is the cluster's promise
    /// that the network exists on every member; removing the links is done
    /// per node on Interfaces, which is the page that can say what is still
    /// using one.
    pub async fn forget_external_network(&self, cluster: &str, name: &str) -> Result<()> {
        let _guard = self.gate.lock().await;
        let mut membership = self.require_membership()?;
        let Some(stored) = membership
            .clusters
            .iter_mut()
            .find(|candidate| candidate.definition.name == cluster)
        else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        let before = stored.networks.external.len();
        stored.networks.external.retain(|e| e.name != name);
        if stored.networks.external.len() == before {
            return Err(ClusterError::NotFound(format!(
                "\"{cluster}\" defines no External network called \"{name}\"."
            )));
        }
        membership.version += 1;
        self.store.save_membership(&membership)?;
        tracing::info!(
            cluster = cluster,
            network = name,
            "external network removed from the definition; its bridges were left in place"
        );
        Ok(())
    }

    /// Move the cluster VIP, or take it away.
    ///
    /// `None` removes it: the resource goes and the members keep their own
    /// addresses, which is what a cluster defined without a VIP looks like.
    /// `Some` moves it, which is a remove and a create — Pacemaker's
    /// `IPaddr2` has no notion of an address changing under it, and asking it
    /// to would leave the old address up on whichever member holds it.
    ///
    /// **This is the address the console is very likely being reached on.**
    /// The old one comes down before the new one goes up, so a session on the
    /// VIP will lose its connection mid-operation. That is not a failure and
    /// the console says so before asking; the operation completes on the node
    /// regardless of whether anyone is still listening, and the members' own
    /// addresses stay valid throughout, which is what makes it recoverable.
    ///
    /// Validated against the Management network as recorded: an address
    /// outside its subnet, or one a member already holds, is refused here
    /// rather than becoming a resource that never starts.
    pub async fn set_vip(
        &self,
        cluster: &str,
        address: Option<std::net::Ipv4Addr>,
    ) -> Result<Option<VipView>> {
        let _guard = self.gate.lock().await;
        let mut membership = self.require_membership()?;
        let Some(record) = membership.cluster_record(cluster).cloned() else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        if membership
            .node(&self.node)
            .and_then(|n| n.cluster.as_deref())
            != Some(cluster)
        {
            return Err(ClusterError::Conflict(format!(
                "The cluster VIP is changed from inside the cluster. Open the console of a \
                 member of \"{cluster}\"."
            )));
        }

        let management = &record.networks.management;
        if let Some(wanted) = address {
            if !management.subnet.contains(wanted) {
                return Err(ClusterError::invalid(ValidationError::new(
                    ValidationCode::InvalidVip,
                    Some("management.vip"),
                    format!(
                        "The cluster VIP {wanted} is outside the Management subnet {} — \
                         nothing would route to it.",
                        management.subnet
                    ),
                )));
            }
            if let Some(member) = management.members.iter().find(|m| m.address == wanted) {
                return Err(ClusterError::invalid(ValidationError::new(
                    ValidationCode::InvalidVip,
                    Some("management.vip"),
                    format!(
                        "{wanted} is already \"{}\"'s own address. The cluster VIP has to be \
                         one nothing else holds — it moves between members, and two things \
                         answering on it is a broken console for whoever gets the wrong one.",
                        member.node
                    ),
                )));
            }
        }
        if management.vip == address {
            return Err(ClusterError::Conflict(match address {
                Some(current) => format!("The cluster VIP is already {current}."),
                None => format!("\"{cluster}\" has no cluster VIP to remove."),
            }));
        }

        // The existing resource first, whether this is a move or a removal.
        // Pacemaker holds the old address up until it is told not to, and
        // creating the new one alongside it would leave two.
        let state = self.backend.cluster_state(cluster).await?;
        if let Some(existing) = state.vip.as_ref() {
            self.backend.remove_resource(&existing.resource).await?;
        }
        if let Some(wanted) = address {
            self.backend
                .create_vip(cluster, wanted, management.subnet.prefix)
                .await?;
        }

        let Some(stored) = membership
            .clusters
            .iter_mut()
            .find(|candidate| candidate.definition.name == cluster)
        else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        stored.networks.management.vip = address;
        membership.version += 1;
        self.store.save_membership(&membership)?;
        tracing::info!(
            cluster = cluster,
            address = ?address,
            "cluster VIP changed"
        );

        let Some(address) = address else {
            return Ok(None);
        };
        // Read back, for the same reason the recovery does: Pacemaker
        // decides whether the address comes up, and a console told "done"
        // over an address nobody answers on has been told nothing.
        let state = self.backend.cluster_state(cluster).await?;
        Ok(Some(VipView {
            address,
            state: state.vip,
        }))
    }

    /// Change the Core network without destroying the cluster: the MTU, and
    /// which link carries each member's seat.
    ///
    /// What it will not change is the ring's identity. The subnet and every
    /// member's address are corosync's ring 0 addressing — written into
    /// `corosync.conf` on every member, ridden by the pool's peer links —
    /// and changing them stays a destroy-and-recreate; the request shape
    /// itself cannot carry a new subnet, and a `members` list that moves an
    /// address or the set of seats is refused here. The MTU and the seat
    /// interfaces appear in no ring configuration at all, which is what
    /// makes this edit possible: each member re-realizes the same seat
    /// through its own networking domain, inside its own checkpoint.
    ///
    /// Members change one at a time. During a seat move the member's ring 0
    /// drops until the new link carries the address; ring 1 on Management is
    /// what keeps the cluster quorate through the blip, and a member whose
    /// change goes wrong is restored by its own checkpoint. A member that
    /// fails stops the walk: the members already changed are on the new
    /// definition, the record stays on the old one, and the error names the
    /// member — the same non-atomicity the External edit accepts, with the
    /// same repair, because a member already changed stages nothing on the
    /// retry and succeeds.
    pub async fn update_core_network(
        &self,
        cluster: &str,
        update: CoreNetworkUpdate,
    ) -> Result<CoreNetwork> {
        let _guard = self.gate.lock().await;
        if self.progress.busy() {
            return Err(ClusterError::Conflict(
                "A cluster workflow is running. Let it finish before changing the networks."
                    .to_string(),
            ));
        }
        let mut membership = self.require_membership()?;
        let Some(record) = membership.cluster_record(cluster).cloned() else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        if membership
            .node(&self.node)
            .and_then(|n| n.cluster.as_deref())
            != Some(cluster)
        {
            return Err(ClusterError::Conflict(format!(
                "The Core network is changed from inside the cluster. Open the console of a \
                 member of \"{cluster}\"."
            )));
        }

        let old = record.networks.core.clone();
        let mut wanted = old.clone();
        if let Some(mtu) = update.mtu {
            wanted.mtu = mtu;
        }
        if let Some(members) = update.members {
            wanted.members = members;
        }

        // The seats stay the same seats: same members, same addresses. An
        // address is corosync's name for the member and the pool's peer
        // address, on every member at once — not a per-node act this walk
        // could perform safely.
        for member in &old.members {
            let Some(next) = wanted.members.iter().find(|m| m.node == member.node) else {
                return Err(ClusterError::Conflict(format!(
                    "\"{}\" has a Core seat and this edit drops it. Seats are added by growing \
                     the cluster and leave with the member — not here.",
                    member.node
                )));
            };
            if next.address != member.address {
                return Err(ClusterError::Conflict(format!(
                    "\"{}\"'s Core address cannot change here: {} is corosync's ring 0 name for \
                     it and the pool's peer address, written on every member. Re-addressing \
                     Core is a destroy and re-create.",
                    member.node, member.address
                )));
            }
        }
        for member in &wanted.members {
            if !old.members.iter().any(|m| m.node == member.node) {
                return Err(ClusterError::Conflict(format!(
                    "\"{}\" has no Core seat to change.",
                    member.node
                )));
            }
        }
        if wanted == old {
            return Err(ClusterError::Conflict(
                "Nothing in that request differs from what the cluster already has.".to_string(),
            ));
        }

        // Validate the whole document against what every member actually
        // has, read fresh — a seat must move onto a link that exists and
        // carries link, per the owner's own report. A member that cannot be
        // asked blocks the edit: the walk below has to reach every member
        // whose seat changes, and starting one it cannot finish is how the
        // record and the members drift apart.
        let members: Vec<EnvironmentNode> = membership
            .members_of(cluster)
            .into_iter()
            .cloned()
            .collect();
        let mut observed = Vec::new();
        for member in &members {
            match self.peers.preflight(member).await {
                Ok(report) => observed.push(lumen_net::ObservedState {
                    node: member.name.clone(),
                    links: report.links,
                }),
                Err(err) => {
                    return Err(ClusterError::Conflict(format!(
                        "\"{}\" could not be asked about its links, and a Core edit does not \
                         start on a cluster it cannot finish walking: {err}",
                        member.name
                    )))
                }
            }
        }
        let mut candidate = record.networks.clone();
        candidate.core = wanted.clone();
        let node_names: Vec<String> = members.iter().map(|m| m.name.clone()).collect();
        let errors = validate_networks(&candidate, &node_names, &observed);
        if !errors.is_empty() {
            return Err(ClusterError::Invalid(errors));
        }

        // One member at a time, unchanged members skipped. Sequential on
        // purpose: a seat move blips that member's ring 0, and two members
        // blipping at once on a two-node cluster is both rings' worth of
        // trouble instead of one member's.
        for member in &members {
            let Some(seat) = wanted.members.iter().find(|m| m.node == member.name) else {
                continue;
            };
            let Some(was) = old.members.iter().find(|m| m.node == member.name) else {
                continue;
            };
            let moved = seat.interface != was.interface;
            if !moved && wanted.mtu == old.mtu {
                continue;
            }
            let payload = crate::join::CoreSeatUpdate {
                cluster: cluster.to_string(),
                old_interface: moved.then(|| was.interface.clone()),
                core: crate::join::CoreAssignment {
                    interface: seat.interface.clone(),
                    address: seat.address,
                    prefix: wanted.subnet.prefix,
                    mtu: wanted.mtu,
                },
            };
            if let Err(err) = self.peers.update_core_seat(member, &payload).await {
                return Err(ClusterError::Conflict(format!(
                    "\"{}\" could not take the new Core definition, so the change was not \
                     recorded: {err}. The members already changed are on the new definition and \
                     the record is still on the old one — fix that member and ask again; a \
                     member already changed will not object the second time.",
                    member.name
                )));
            }
        }

        // Recorded only now that every member is on it — the External
        // edit's order, for the External edit's reason.
        let Some(stored) = membership
            .clusters
            .iter_mut()
            .find(|candidate| candidate.definition.name == cluster)
        else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        stored.networks.core = wanted.clone();
        membership.version += 1;
        self.store.save_membership(&membership)?;
        tracing::info!(
            cluster = cluster,
            mtu = wanted.mtu,
            "core network changed on every member"
        );
        Ok(wanted)
    }

    /// Every member has an uplink and every uplink names a member — the
    /// every-member-or-none rule, checked before anything is built rather
    /// than discovered halfway through.
    ///
    /// Shared by the create and the change because it is the same rule: a
    /// machine that fails over onto a member without the network comes up
    /// with no network, and it makes no difference whether that member was
    /// missed when the network was defined or when it was edited.
    fn check_uplinks(
        membership: &EnvironmentMembership,
        cluster: &str,
        network: &ExternalNetwork,
    ) -> Result<Vec<EnvironmentNode>> {
        let members: Vec<EnvironmentNode> = membership
            .members_of(cluster)
            .into_iter()
            .cloned()
            .collect();
        for member in &members {
            if !network.uplinks.iter().any(|up| up.node == member.name) {
                return Err(ClusterError::Conflict(format!(
                    "\"{}\" has no uplink for this network. An External network is defined on \
                     every member or on none — a machine that fails over onto a member without \
                     it comes up with no network.",
                    member.name
                )));
            }
        }
        for uplink in &network.uplinks {
            if !members.iter().any(|member| member.name == uplink.node) {
                return Err(ClusterError::Conflict(format!(
                    "\"{}\" is not a member of \"{cluster}\".",
                    uplink.node
                )));
            }
        }
        Ok(members)
    }

    // --- environment membership -------------------------------------------

    /// Mint a one-time join token. The first mint on a fresh node bootstraps
    /// the environment: an id, a CA, this node's own certificate, and a
    /// membership record of one.
    pub async fn mint_token(&self, address: &str) -> Result<MintedToken> {
        let _guard = self.gate.lock().await;

        let (membership, bootstrapped) = match self.membership()? {
            Some(membership) => (membership, false),
            None => {
                let id = random_hex();
                let (ca_pem, ca_key_pem) = mint_ca(&id)?;
                let (node_cert_pem, node_key_pem) =
                    issue_node_certificate(&id, &ca_key_pem, &self.node, address)?;
                self.store.save_identity(&Identity {
                    ca_pem,
                    ca_key_pem,
                    node_cert_pem,
                    node_key_pem,
                })?;
                let membership = EnvironmentMembership {
                    id,
                    version: 1,
                    nodes: vec![EnvironmentNode {
                        name: self.node.clone(),
                        address: address.to_string(),
                        controlplane_version: self.controlplane_version.clone(),
                        cluster: None,
                        maintenance: None,
                    }],
                    clusters: Vec::new(),
                };
                self.store.save_membership(&membership)?;
                tracing::info!(environment = %membership.id, "environment bootstrapped");
                (membership, true)
            }
        };

        // The address the joining node will dial is this node's recorded
        // one, not whatever the request happened to carry.
        let issuer = membership
            .node(&self.node)
            .map(|n| n.address.clone())
            .unwrap_or_else(|| address.to_string());

        let identity = self.store.load_identity()?.ok_or_else(|| {
            ClusterError::Backend(anyhow::anyhow!(
                "the environment state is missing its certificates"
            ))
        })?;

        let secret = random_hex();
        let id = random_hex()[..16].to_string();
        let expires_at = now_unix() + TOKEN_TTL_SECS;
        let mut tokens = self.store.load_tokens()?;
        tokens.retain(|t| t.expires_at > now_unix());
        tokens.push(JoinTokenRecord {
            id: id.clone(),
            secret_hash: hash_secret(&secret),
            expires_at,
        });
        self.store.save_tokens(&tokens)?;

        let token = JoinToken {
            issuer,
            id,
            secret,
            fingerprint: pem_fingerprint(&identity.node_cert_pem)?,
        };
        tracing::info!("join token minted");
        Ok(MintedToken {
            token: token.encode(),
            expires_at,
            bootstrapped,
        })
    }

    /// The issuer's half of a join: validate and consume the token, sign the
    /// newcomer's certificate, add it to the record, and hand the
    /// environment over. `session_secret` is the control plane's live
    /// web-session secret, which becomes the newcomer's too.
    pub async fn grant_join(
        &self,
        request: &JoinRequest,
        session_secret: &[u8],
    ) -> Result<JoinGrant> {
        use base64::Engine;
        let _guard = self.gate.lock().await;

        let mut membership = self.require_membership()?;

        let mut tokens = self.store.load_tokens()?;
        let now = now_unix();
        let position = tokens.iter().position(|t| {
            t.id == request.token_id
                && t.secret_hash == hash_secret(&request.secret)
                && t.expires_at > now
        });
        let Some(position) = position else {
            return Err(ClusterError::Conflict(
                "That join token is not valid here. Tokens are one-time and short-lived — mint \
                 a fresh one."
                    .to_string(),
            ));
        };
        // Consumed before anything else can fail: a token that reached a
        // failed join is spent, not reusable.
        tokens.remove(position);
        self.store.save_tokens(&tokens)?;

        if !crate::model::valid_node_name(&request.node) {
            return Err(ClusterError::Conflict(format!(
                "\"{}\" is not a usable node name.",
                request.node
            )));
        }
        if membership.node(&request.node).is_some() {
            return Err(ClusterError::Conflict(format!(
                "This environment already has a node called \"{}\". Two nodes cannot share a \
                 hostname.",
                request.node
            )));
        }

        let identity = self.store.load_identity()?.ok_or_else(|| {
            ClusterError::Backend(anyhow::anyhow!(
                "the environment state is missing its certificates"
            ))
        })?;
        let (node_cert_pem, node_key_pem) = issue_node_certificate(
            &membership.id,
            &identity.ca_key_pem,
            &request.node,
            &request.address,
        )?;

        membership.version += 1;
        membership.nodes.push(EnvironmentNode {
            name: request.node.clone(),
            address: request.address.clone(),
            controlplane_version: request.controlplane_version.clone(),
            cluster: None,
            maintenance: None,
        });
        self.store.save_membership(&membership)?;
        tracing::info!(node = %request.node, "node joined the environment");

        Ok(JoinGrant {
            membership,
            ca_pem: identity.ca_pem,
            ca_key_pem: identity.ca_key_pem,
            node_cert_pem,
            node_key_pem,
            session_secret: base64::engine::general_purpose::STANDARD.encode(session_secret),
        })
    }

    /// The joining node's half: decode the pasted token, call the issuer
    /// over the fingerprint-pinned channel, and adopt what comes back. The
    /// control plane swaps its live session secret and TLS certificate from
    /// the returned outcome.
    pub async fn join(&self, token_text: &str, local_address: &str) -> Result<JoinOutcome> {
        use base64::Engine;
        let _guard = self.gate.lock().await;

        if self.store.exists() {
            return Err(ClusterError::Conflict(
                "This node is already in an environment. A node belongs to exactly one."
                    .to_string(),
            ));
        }
        let token = JoinToken::decode(token_text)?;
        let request = JoinRequest {
            token_id: token.id.clone(),
            secret: token.secret.clone(),
            node: self.node.clone(),
            address: local_address.to_string(),
            controlplane_version: self.controlplane_version.clone(),
        };
        let grant = self
            .peers
            .request_join(&token.issuer, &token.fingerprint, &request)
            .await?;

        if grant.membership.node(&self.node).is_none() {
            return Err(ClusterError::Backend(anyhow::anyhow!(
                "the issuer's answer does not include this node"
            )));
        }
        let session_secret = base64::engine::general_purpose::STANDARD
            .decode(&grant.session_secret)
            .map_err(|_| {
                ClusterError::Backend(anyhow::anyhow!("the issuer's session secret is garbled"))
            })?;

        self.store.save_identity(&Identity {
            ca_pem: grant.ca_pem,
            ca_key_pem: grant.ca_key_pem,
            node_cert_pem: grant.node_cert_pem,
            node_key_pem: grant.node_key_pem,
        })?;
        self.store.save_membership(&grant.membership)?;
        tracing::info!(environment = %grant.membership.id, "joined the environment");
        Ok(JoinOutcome { session_secret })
    }

    /// Gossip receive: reconcile a peer's record with ours and answer with
    /// the result. A node with no environment refuses — adopting a record
    /// over gossip would be a takeover, not a sync.
    pub async fn receive_membership(
        &self,
        remote: EnvironmentMembership,
    ) -> Result<EnvironmentMembership> {
        let _guard = self.gate.lock().await;
        let local = self.require_membership()?;
        let merged = EnvironmentMembership::reconcile(local, remote);
        self.store.save_membership(&merged)?;
        Ok(merged)
    }

    /// Bring the record's row for this node up to the control plane actually
    /// running. The row is written at join and then trusted, so a package
    /// update plus restart left every console showing the version this node
    /// joined with, forever. Stamped before each gossip send rather than once
    /// at startup: last-writer-wins can drop a bump that tied with a
    /// concurrent write, and a stamp that repeats until the record agrees
    /// heals that on the next pass.
    async fn note_own_version(&self) {
        let _guard = self.gate.lock().await;
        let Ok(Some(mut membership)) = self.membership() else {
            return;
        };
        let Some(node) = membership.nodes.iter_mut().find(|n| n.name == self.node) else {
            return;
        };
        if node.controlplane_version == self.controlplane_version {
            // Already true. The counter is what makes every peer re-read the
            // record; spending it on nothing would be noise (see
            // set_maintenance's no-op).
            return;
        }
        node.controlplane_version = self.controlplane_version.clone();
        membership.version += 1;
        match self.store.save_membership(&membership) {
            Ok(()) => tracing::info!(
                version = %self.controlplane_version,
                "the membership record caught up with the running control plane"
            ),
            Err(err) => {
                tracing::warn!("the record did not take this node's version: {err}")
            }
        }
    }

    /// Gossip send: push our record to every peer, adopt anything newer that
    /// comes back. Best-effort by design — an unreachable peer is reported
    /// by the environment view, not by gossip failing loudly every minute.
    pub async fn gossip_once(&self) {
        self.note_own_version().await;
        let Ok(Some(mut membership)) = self.membership() else {
            return;
        };
        let peers: Vec<EnvironmentNode> = membership
            .nodes
            .iter()
            .filter(|n| n.name != self.node)
            .cloned()
            .collect();
        for peer in peers {
            match self.peers.push_membership(&peer, &membership).await {
                Ok(answer) => {
                    membership = EnvironmentMembership::reconcile(membership, answer);
                }
                Err(err) => {
                    tracing::debug!(peer = %peer.name, "gossip did not reach the peer: {err}");
                }
            }
        }
        let _guard = self.gate.lock().await;
        if let Ok(Some(local)) = self.membership() {
            let merged = EnvironmentMembership::reconcile(local, membership);
            let _ = self.store.save_membership(&merged);
        }
    }

    /// Remove an unassigned node from the environment. A cluster member
    /// leaves its cluster first; this node does not remove itself.
    pub async fn remove_node(&self, name: &str) -> Result<()> {
        let _guard = self.gate.lock().await;
        let mut membership = self.require_membership()?;
        let Some(node) = membership.node(name) else {
            return Err(ClusterError::NotFound(format!(
                "There is no node called \"{name}\" in this environment."
            )));
        };
        if let Some(cluster) = &node.cluster {
            return Err(ClusterError::Conflict(format!(
                "\"{name}\" is a member of \"{cluster}\" — remove it from the cluster first."
            )));
        }
        if name == self.node {
            return Err(ClusterError::Conflict(
                "A node does not remove itself — do this from another environment node, so \
                 there is still a console to finish from."
                    .to_string(),
            ));
        }
        membership.version += 1;
        membership.nodes.retain(|n| n.name != name);
        self.store.save_membership(&membership)?;
        tracing::info!(node = name, "node removed from the environment");
        Ok(())
    }

    // --- peer-side operations ---------------------------------------------

    /// What this node answers when a coordinator preflights it.
    pub async fn peer_preflight(&self) -> Result<PreflightReport> {
        let local = self.backend.local_preflight().await?;
        let links = self
            .network
            .observe()
            .await
            .map(|o| o.links)
            .unwrap_or_default();
        Ok(PreflightReport {
            node: self.node.clone(),
            controlplane_version: self.controlplane_version.clone(),
            hostname: self.node.clone(),
            time_synchronized: local.time_synchronized,
            time_offset_ms: local.time_offset_ms,
            already_clustered: local.already_clustered,
            links,
        })
    }

    /// Build a bond here, through this node's own networking domain.
    ///
    /// The bond is an ordinary link the moment it exists: it appears in the
    /// Networking page, it is edited and deleted there, and a cluster
    /// teardown leaves it alone. The wizard's shortcut only saves the
    /// operator a trip to each node's console — it does not give the cluster
    /// a second, private way to own a link.
    pub async fn peer_create_bond(&self, bond: &lumen_net::Bond) -> Result<()> {
        let observed = self.network.observe().await.map_err(ClusterError::from)?;
        if observed.link(&bond.name).is_some() {
            return Err(ClusterError::Conflict(format!(
                "This node already has a link called \"{}\".",
                bond.name
            )));
        }
        for port in &bond.ports {
            let Some(link) = observed.link(port) else {
                return Err(ClusterError::Conflict(format!(
                    "This node has no link called \"{port}\"."
                )));
            };
            // Enslaving the link the console answers on cuts the operator off
            // mid-wizard, and the confirm window cannot save a session that
            // was severed by the change it was meant to protect.
            if !link.addresses.is_empty() {
                return Err(ClusterError::Conflict(format!(
                    "\"{port}\" already carries {} — a bond takes its ports over entirely, so \
                     move the addressing off it first.",
                    link.addresses.join(", ")
                )));
            }
        }
        self.network
            .create_bond(bond.clone())
            .await
            .map_err(ClusterError::from)?;
        self.network
            .apply(lumen_net::Acknowledgements::default())
            .await
            .map_err(ClusterError::from)?;
        self.network.confirm().await.map_err(ClusterError::from)?;
        Ok(())
    }

    /// Build an External network's bridge here, through this node's own
    /// networking domain.
    ///
    /// Same rule as the bond above: what comes out is an ordinary link. The
    /// cluster's record says the network exists and on which port; the bridge
    /// realizing it belongs to networking, is edited there, and survives a
    /// cluster teardown — because the machines attached to it do.
    ///
    /// Idempotent on the bridge already being right. A create that half
    /// succeeded across members is retried by the coordinator, and a member
    /// that already did its part must not fail the second attempt.
    pub async fn peer_create_bridge(&self, seat: &crate::join::ExternalSeat) -> Result<()> {
        let bridge = &seat.bridge;
        let observed = self.network.observe().await.map_err(ClusterError::from)?;
        if let Some(existing) = observed.link(&bridge.name) {
            if existing.kind != lumen_net::LinkKind::Bridge {
                return Err(ClusterError::Conflict(format!(
                    "This node already has a link called \"{}\", and it is not a bridge.",
                    bridge.name
                )));
            }
            // Already built, by an earlier attempt or by hand. The uplink is
            // what makes it the right bridge; a bridge of the same name with
            // a different port is a different network wearing the name.
            for port in &bridge.ports {
                if !existing.ports.contains(port) {
                    return Err(ClusterError::Conflict(format!(
                        "This node already has a bridge called \"{}\" and \"{port}\" is not one \
                         of its ports.",
                        bridge.name
                    )));
                }
            }
            return Ok(());
        }
        // An access network's port is the VLAN interface, which does not
        // exist yet — so the port check applies to whatever the seat is
        // actually built on: the VLAN's parent when there is one, the
        // bridge's port when there is not.
        let physical: Vec<&String> = match &seat.vlan {
            Some(vlan) => vec![&vlan.parent],
            None => bridge.ports.iter().collect(),
        };
        for port in physical {
            let Some(link) = observed.link(port) else {
                return Err(ClusterError::Conflict(format!(
                    "This node has no link called \"{port}\"."
                )));
            };
            // The same cut-yourself-off rule the bond enforces: a bridge takes
            // its port over, so the link the console answers on may not be
            // swallowed without moving the addressing first. A VLAN interface
            // is the exception — it shares its parent rather than consuming
            // it, which is exactly why an access network is built that way.
            if seat.vlan.is_none() && !link.addresses.is_empty() {
                return Err(ClusterError::Conflict(format!(
                    "\"{port}\" already carries {} — a bridge takes its port over, so move the \
                     addressing off it first.",
                    link.addresses.join(", ")
                )));
            }
        }
        // The VLAN interface first: the bridge names it as its port, so it
        // has to exist before the bridge that claims it.
        if let Some(vlan) = &seat.vlan {
            if observed.link(&vlan.name).is_none() {
                self.network
                    .create_vlan(vlan.clone())
                    .await
                    .map_err(ClusterError::from)?;
            }
        }
        self.network
            .create_bridge(bridge.clone())
            .await
            .map_err(ClusterError::from)?;
        self.network
            .apply(lumen_net::Acknowledgements::default())
            .await
            .map_err(ClusterError::from)?;
        self.network.confirm().await.map_err(ClusterError::from)?;
        Ok(())
    }

    /// Put addressing on this node's Core seat, whatever kind of link it is.
    ///
    /// Applied and confirmed in one motion: a Core address on a link that is
    /// not the console cannot sever the operator, which is what the confirm
    /// window exists to protect against.
    async fn address_core_seat(
        &self,
        interface: &str,
        ip: lumen_net::IpConfig,
        mtu: Option<u32>,
    ) -> Result<()> {
        self.stage_seat_patch(interface, Some(ip), mtu).await?;
        self.apply_and_confirm().await
    }

    /// Stage one seat patch without applying it — the Core edit stages the
    /// old link's release and the new link's addressing as *one* pending set,
    /// so the checkpoint restores both or neither.
    ///
    /// A Core seat is a NIC most of the time and a bond when the operator
    /// wants the ring to survive a cable — and `lumen-net` patches each kind
    /// through its own call, so the kind has to be looked up rather than
    /// assumed.
    async fn stage_seat_patch(
        &self,
        interface: &str,
        ip: Option<lumen_net::IpConfig>,
        mtu: Option<u32>,
    ) -> Result<()> {
        use lumen_net::service::{BondPatch, BridgePatch, NicPatch, VlanPatch};
        use lumen_net::LinkKind;

        let observed = self.network.observe().await.map_err(ClusterError::from)?;
        let kind = observed
            .link(interface)
            .map(|link| link.kind)
            .ok_or_else(|| {
                ClusterError::Conflict(format!("This node has no link called \"{interface}\"."))
            })?;
        match kind {
            LinkKind::Ethernet => {
                self.network
                    .update_nic(
                        interface,
                        NicPatch {
                            ip,
                            mtu,
                            ..Default::default()
                        },
                    )
                    .await
            }
            LinkKind::Bond => {
                self.network
                    .update_bond(
                        interface,
                        BondPatch {
                            ip,
                            mtu,
                            ..Default::default()
                        },
                    )
                    .await
            }
            LinkKind::Bridge => {
                self.network
                    .update_bridge(
                        interface,
                        BridgePatch {
                            ip,
                            mtu,
                            ..Default::default()
                        },
                    )
                    .await
            }
            LinkKind::Vlan => {
                self.network
                    .update_vlan(
                        interface,
                        VlanPatch {
                            ip,
                            mtu,
                            ..Default::default()
                        },
                    )
                    .await
            }
            // Loopback, a guest tap, a tunnel: real links this appliance has
            // no profile for, so there is nothing to patch.
            LinkKind::Other => {
                return Err(ClusterError::Conflict(format!(
                    "\"{interface}\" is not a link this appliance manages, so it cannot carry a \
                     Core seat."
                )))
            }
        }
        .map_err(ClusterError::from)?;
        Ok(())
    }

    /// Apply and confirm whatever seat patches are staged, in one motion —
    /// see [`Self::address_core_seat`] for why no confirm window is kept.
    async fn apply_and_confirm(&self) -> Result<()> {
        self.network
            .apply(lumen_net::Acknowledgements::default())
            .await
            .map_err(ClusterError::from)?;
        self.network.confirm().await.map_err(ClusterError::from)?;
        Ok(())
    }

    /// Re-realize this node's Core seat: the same address on a different
    /// link, a different MTU, or both — the Core edit's per-member half.
    ///
    /// The release of the old link and the addressing of the new one are one
    /// staged apply inside one checkpoint, so a failure restores both. Each
    /// half is staged only where the box disagrees with it, which is what
    /// makes a retry after a half-finished change stage nothing and succeed
    /// rather than refuse — the coordinator's "ask again" depends on it.
    pub async fn peer_update_core_seat(&self, update: &crate::join::CoreSeatUpdate) -> Result<()> {
        let core = &update.core;
        let observed = self.network.observe().await.map_err(ClusterError::from)?;
        let cidr = format!("{}/{}", core.address, core.prefix);
        let Some(link) = observed.link(&core.interface) else {
            return Err(ClusterError::Conflict(format!(
                "This node has no link called \"{}\".",
                core.interface
            )));
        };
        let old = update
            .old_interface
            .as_deref()
            .filter(|old| *old != core.interface);

        let mut staged = false;
        if let Some(old) = old {
            let holds = observed
                .link(old)
                .is_some_and(|link| link.addresses.contains(&cidr));
            if holds {
                // Released, never deleted: a bond the operator built before
                // the cluster outlives the seat that is leaving it.
                self.stage_seat_patch(old, Some(lumen_net::IpConfig::Disabled), None)
                    .await?;
                staged = true;
            }
        }
        let needs_address = !link.addresses.contains(&cidr);
        let needs_mtu = link.mtu != Some(core.mtu);
        if needs_address || needs_mtu {
            self.stage_seat_patch(
                &core.interface,
                needs_address.then(|| lumen_net::IpConfig::Static {
                    cidr: cidr.clone(),
                    // A Core network routes nowhere, exactly as at prepare.
                    gateway: String::new(),
                    dns: Vec::new(),
                }),
                Some(core.mtu),
            )
            .await?;
            staged = true;
        }
        if staged {
            self.apply_and_confirm().await?;
        }

        // The firewall bindings follow the seat. Old closed before new
        // opened so that two links sharing a zone end with the services
        // bound; management's own binding is not this edit's to touch,
        // which is what the `None` says.
        if let Some(old) = old {
            self.backend.set_cluster_ports(old, None, false).await?;
            self.backend
                .set_cluster_ports(&core.interface, None, true)
                .await?;
        }
        tracing::info!(
            cluster = %update.cluster,
            interface = %core.interface,
            mtu = core.mtu,
            moved_from = ?old,
            "core seat re-realized"
        );
        Ok(())
    }

    /// Become part of a cluster: realize the Core seat through the
    /// networking domain, then write the cluster configuration.
    pub async fn peer_prepare(&self, payload: &PreparePayload) -> Result<()> {
        let core = &payload.core;
        let observed = self.network.observe().await.map_err(ClusterError::from)?;
        let cidr = format!("{}/{}", core.address, core.prefix);
        let needs_address = match observed.link(&core.interface) {
            None => {
                return Err(ClusterError::Conflict(format!(
                    "This node has no link called \"{}\".",
                    core.interface
                )))
            }
            Some(link) => !link.addresses.contains(&cidr),
        };
        if needs_address {
            self.address_core_seat(
                &core.interface,
                lumen_net::IpConfig::Static {
                    cidr,
                    // A Core network routes nowhere: replication and
                    // heartbeat between members, and nothing else.
                    gateway: String::new(),
                    dns: Vec::new(),
                },
                Some(core.mtu),
            )
            .await?;
        }
        // Before the configuration, because a node whose stack starts into a
        // closed firewall is a cluster that forms on paper and replicates
        // nowhere: the ports the corosync rings and the replication link
        // need are declared by the package and bound here, on the
        // interfaces that carry them.
        self.backend
            .set_cluster_ports(
                &core.interface,
                payload.management_interface.as_deref(),
                true,
            )
            .await?;
        // Clustering is what makes live migration possible, so clustering
        // opens the hypervisor's door for it — with reachability already
        // confined to the Core interfaces the binding above named. This
        // was the recorded loose end from the replicated-storage program
        // ("enabling it silently for every appliance would make the
        // decision nobody's"); the workflow that needs it enabling it is
        // whose decision it is.
        self.backend.set_migration_listener(true).await?;
        self.backend
            .write_cluster_config(&payload.corosync_conf, &payload.authkey)
            .await
    }

    /// Start the stack — the enable half of the preset story: corosync and
    /// pacemaker are off on a fresh install and become enabled here.
    pub async fn peer_start(&self) -> Result<()> {
        self.backend.enable_stack().await
    }

    /// Put the node back exactly as it was: stack down and disabled,
    /// configuration gone, the Core address released.
    pub async fn peer_teardown(&self, payload: &TeardownPayload) -> Result<()> {
        let mut first_error: Option<ClusterError> = None;
        let mut note = |result: Result<()>| {
            if let Err(err) = result {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        };
        note(self.backend.disable_stack().await);
        // The migration listener came with clustering and leaves with it.
        note(self.backend.set_migration_listener(false).await);
        note(self.backend.remove_cluster_config().await);
        if let Some(interface) = &payload.core_interface {
            // The ports close before the address goes: both are things this
            // node opened for a cluster it is leaving, and neither should
            // outlive it.
            note(
                self.backend
                    .set_cluster_ports(interface, payload.management_interface.as_deref(), false)
                    .await,
            );
            // The seat is released, not the link: a bond the operator built
            // before the create outlives the cluster that borrowed it.
            note(
                self.address_core_seat(interface, lumen_net::IpConfig::Disabled, None)
                    .await,
            );
        }
        match first_error {
            None => Ok(()),
            Some(err) => Err(err),
        }
    }

    // --- cluster workflows ------------------------------------------------

    /// Ask every named node whether it could join a cluster right now — the
    /// wizard's first page, and the same judgement the create re-runs.
    pub async fn preflight(&self, nodes: &[String]) -> Result<Vec<PreflightView>> {
        let membership = self.require_membership()?;
        let mut views = Vec::new();
        for name in nodes {
            let Some(node) = membership.node(name) else {
                views.push(PreflightView {
                    node: name.clone(),
                    ok: false,
                    problems: vec![format!(
                        "\"{name}\" has not joined this environment, so it cannot join a cluster."
                    )],
                    report: None,
                });
                continue;
            };
            match self.peers.preflight(node).await {
                Ok(report) => {
                    views.push(judge_preflight(&report, name, &self.controlplane_version))
                }
                Err(err) => views.push(PreflightView {
                    node: name.clone(),
                    ok: false,
                    problems: vec![err.to_string()],
                    report: None,
                }),
            }
        }
        Ok(views)
    }

    /// Build a bond on one environment node, from the coordinator.
    ///
    /// The create wizard's Core-redundancy shortcut. It reaches the node
    /// through the same peer channel every other cross-node call uses, and
    /// lands in that node's networking domain — so the bond that comes out is
    /// indistinguishable from one built on the node's own Networking page,
    /// and is edited and deleted there afterwards.
    pub async fn bond_node_nics(&self, node: &str, bond: &lumen_net::Bond) -> Result<()> {
        let membership = self.require_membership()?;
        let member = membership.node(node).ok_or_else(|| {
            ClusterError::Conflict(format!(
                "\"{node}\" has not joined this environment, so its links cannot be configured \
                 from here."
            ))
        })?;
        if bond.ports.len() < 2 {
            return Err(ClusterError::Conflict(
                "A bond built for redundancy needs at least two ports — one port is the cable \
                 it was meant to survive."
                    .to_string(),
            ));
        }
        self.peers.create_bond(member, bond).await
    }

    /// Start a cluster create. Validation happens now; the workflow itself
    /// runs in the background and is polled through [`Self::create_progress`]
    /// — per node, per step, unwind included.
    pub async fn create_cluster(
        self: &Arc<Self>,
        request: ClusterCreate,
    ) -> Result<CreateProgress> {
        let _guard = self.gate.lock().await;
        if self.progress.busy() {
            return Err(ClusterError::Conflict(
                "A cluster is already being created. One at a time — watch it finish first."
                    .to_string(),
            ));
        }
        let membership = self.require_membership()?;
        let (definition, networks, bmc_passwords) =
            request.build().map_err(ClusterError::Invalid)?;
        let errors = validate_definition(&definition, &membership);
        if !errors.is_empty() {
            return Err(ClusterError::Invalid(errors));
        }

        // Begun here, before the spawn, so the snapshot below — and the
        // wizard's first poll — already carry the whole plan.
        self.progress.begin(crate::join::plan_progress(&definition));

        let service = self.clone();
        let version = self.controlplane_version.clone();
        let progress = self.progress.clone();
        tokio::spawn(async move {
            let result = run_create(
                definition,
                networks,
                bmc_passwords,
                membership,
                &version,
                service.peers.as_ref(),
                service.backend.as_ref(),
                &progress,
                service.form_poll,
            )
            .await;
            match result {
                Ok(updated) => {
                    let _guard = service.gate.lock().await;
                    if let Err(err) = service.store.save_membership(&updated) {
                        tracing::error!("the cluster formed but the record did not save: {err}");
                    }
                    drop(_guard);
                    service.gossip_once().await;
                    tracing::info!("cluster created");
                }
                Err(err) => {
                    tracing::warn!("cluster create failed and was unwound: {err}");
                }
            }
        });

        // The initial snapshot: every step pending, for the wizard to render
        // immediately.
        Ok(self
            .progress
            .snapshot()
            .expect("run_create begins the progress before its first await"))
    }

    /// The create in flight (or the last one finished), for polling.
    pub fn create_progress(&self) -> Option<CreateProgress> {
        self.progress.snapshot()
    }

    /// Destroy a cluster: every member torn down, then the record forgets
    /// it. Refused unless every member cleans up — a destroy that loses a
    /// node halfway is a stuck cluster, not a smaller one.
    pub async fn destroy_cluster(&self, name: &str, ack: Acknowledgements) -> Result<()> {
        let _guard = self.gate.lock().await;
        if !ack.may_lose_data {
            return Err(ClusterError::invalid(ValidationError::new(
                ValidationCode::UnacknowledgedDestructiveOperation,
                Some("i_understand_this_may_lose_data"),
                "Destroying a cluster stops its stack on every member. Acknowledge that to
                 proceed.",
            )));
        }
        let mut membership = self.require_membership()?;
        let Some(record) = membership.cluster_record(name).cloned() else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{name}\" in this environment."
            )));
        };

        let members: Vec<EnvironmentNode> =
            membership.members_of(name).into_iter().cloned().collect();
        let mut failures = Vec::new();
        for node in &members {
            let payload = TeardownPayload {
                cluster: name.to_string(),
                core_interface: record
                    .networks
                    .core
                    .members
                    .iter()
                    .find(|m| m.node == node.name)
                    .map(|m| m.interface.clone()),
                management_interface: record
                    .networks
                    .management
                    .members
                    .iter()
                    .find(|m| m.node == node.name)
                    .map(|m| m.interface.clone()),
            };
            if let Err(err) = self.peers.teardown(node, &payload).await {
                failures.push(format!("{}: {err}", node.name));
            }
        }
        if !failures.is_empty() {
            return Err(ClusterError::Conflict(format!(
                "Not every member could be cleaned up, so the cluster is still recorded. {}",
                failures.join(" ")
            )));
        }

        membership.version += 1;
        for node in &mut membership.nodes {
            if node.cluster.as_deref() == Some(name) {
                node.cluster = None;
            }
        }
        membership.clusters.retain(|c| c.definition.name != name);
        self.store.save_membership(&membership)?;
        drop(_guard);
        self.gossip_once().await;
        tracing::info!(cluster = name, "cluster destroyed");
        Ok(())
    }

    // --- scale-out ----------------------------------------------------------

    /// Validate a node-add and lay out its plan. The caller — the control
    /// plane — spawns [`Self::execute_add_node`] with what this returns, so
    /// it can chain the volume-policy refresh after the regime flips; the
    /// progress is begun here, so the first poll already has every step.
    pub async fn prepare_add_node(
        self: &Arc<Self>,
        cluster: &str,
        request: crate::validate::MemberCreate,
    ) -> Result<AddNodePlan> {
        let _guard = self.gate.lock().await;
        if self.progress.busy() {
            return Err(ClusterError::Conflict(
                "A cluster workflow is already running. One at a time — watch it finish first."
                    .to_string(),
            ));
        }
        let membership = self.require_membership()?;
        let Some(record) = membership.cluster_record(cluster).cloned() else {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        let Some(newcomer) = membership.node(&request.node).cloned() else {
            return Err(ClusterError::Conflict(format!(
                "\"{}\" has not joined this environment, so it cannot join a cluster.",
                request.node
            )));
        };
        if newcomer.cluster.is_some() {
            return Err(ClusterError::Conflict(format!(
                "\"{}\" is already in a cluster — a node belongs to at most one.",
                request.node
            )));
        }
        if record.definition.nodes.len() >= crate::model::MAX_CLUSTER_NODES {
            return Err(ClusterError::Conflict(format!(
                "\"{cluster}\" already has {} nodes — the most a cluster holds.",
                record.definition.nodes.len()
            )));
        }
        if request.bmc_password.is_empty() {
            return Err(ClusterError::invalid(ValidationError::new(
                ValidationCode::MissingBmcPassword,
                Some("fencing"),
                "The new node needs a BMC password — fencing is not optional.",
            )));
        }

        // The grown shape, validated exactly as a create would be.
        let core_address: std::net::Ipv4Addr = request.core_address.parse().map_err(|_| {
            ClusterError::invalid(ValidationError::new(
                ValidationCode::InvalidAddress,
                Some("core_address"),
                format!("\"{}\" is not an IPv4 address.", request.core_address),
            ))
        })?;
        let management_address: std::net::Ipv4Addr =
            request.management_address.parse().map_err(|_| {
                ClusterError::invalid(ValidationError::new(
                    ValidationCode::InvalidAddress,
                    Some("management_address"),
                    format!("\"{}\" is not an IPv4 address.", request.management_address),
                ))
            })?;
        let old_definition = record.definition.clone();
        let mut definition = record.definition.clone();
        definition.nodes.push(crate::model::MemberNode {
            name: request.node.clone(),
            ring0: core_address,
            ring1: management_address,
            bmc: crate::model::BmcConfig {
                address: request.bmc_address.clone(),
                username: request.bmc_username.clone(),
                cipher: None,
            },
        });
        // A grown cluster is in the quorum regime; a preferred node would be
        // a setting that does nothing, so the definition sheds it.
        if definition.regime() == Regime::Quorum {
            definition.preferred_node = None;
        }
        // Validated against a scratch record where this cluster's own seats
        // are cleared: its existing members are exactly where they should
        // be, and "already clustered" is for *other* clusters' nodes.
        let mut scratch = membership.clone();
        for node in &mut scratch.nodes {
            if node.cluster.as_deref() == Some(cluster) {
                node.cluster = None;
            }
        }
        let errors = crate::validate::validate_definition(&definition, &scratch);
        if !errors.is_empty() {
            return Err(ClusterError::Invalid(errors));
        }

        let mut networks = record.networks.clone();
        networks
            .core
            .members
            .push(crate::networks::AddressedMember {
                node: request.node.clone(),
                interface: request.core_interface.clone(),
                address: core_address,
            });
        networks
            .management
            .members
            .push(crate::networks::AddressedMember {
                node: request.node.clone(),
                interface: request.management_interface.clone(),
                address: management_address,
            });

        // The plan, laid out before anything runs — the first poll already
        // has every step.
        let mut steps = vec![
            plan_step("preflight", Some(&request.node)),
            plan_step("prepare", Some(&request.node)),
        ];
        for member in old_definition.node_names() {
            steps.push(plan_step("reconfigure", Some(&member)));
        }
        steps.push(plan_step("start", Some(&request.node)));
        steps.push(plan_step("form", None));
        steps.push(plan_step("properties", None));
        steps.push(plan_step("delays", None));
        steps.push(plan_step("fence", Some(&request.node)));
        steps.push(plan_step("record", None));
        self.progress.begin(CreateProgress {
            cluster: cluster.to_string(),
            phase: crate::join::WorkflowPhase::Running,
            error: None,
            steps,
        });

        Ok(AddNodePlan {
            cluster: cluster.to_string(),
            old_definition,
            definition,
            networks,
            newcomer,
            bmc_password: request.bmc_password,
            core: crate::join::CoreAssignment {
                interface: request.core_interface,
                address: core_address,
                prefix: record.networks.core.subnet.prefix,
                mtu: record.networks.core.mtu,
            },
        })
    }

    /// Run a planned node-add to completion or back. The regime flip *is*
    /// this workflow: the regenerated corosync.conf stops carrying
    /// `two_node`, the fence delays flatten, and `no-quorum-policy=stop`
    /// arrives — all from the same topology engine that decided them at
    /// two. Volume I/O is never interrupted: corosync reloads and nothing
    /// restarts.
    pub async fn execute_add_node(self: &Arc<Self>, plan: AddNodePlan) -> Result<()> {
        let result = self.drive_add_node(&plan).await;
        match &result {
            Ok(()) => self
                .progress
                .finish_workflow(crate::join::WorkflowPhase::Complete, None),
            Err(err) => self
                .progress
                .finish_workflow(crate::join::WorkflowPhase::Failed, Some(err.to_string())),
        }
        result
    }

    async fn drive_add_node(self: &Arc<Self>, plan: &AddNodePlan) -> Result<()> {
        let cluster = &plan.cluster;
        let node_name = plan.newcomer.name.clone();

        // Preflight the newcomer, hard.
        self.progress
            .set_step("preflight", Some(&node_name), StepState::Running, None);
        let report = self.peers.preflight(&plan.newcomer).await?;
        let view = judge_preflight(&report, &node_name, &self.controlplane_version);
        if !view.ok {
            let detail = view.problems.join(" ");
            self.progress.set_step(
                "preflight",
                Some(&node_name),
                StepState::Failed,
                Some(detail.clone()),
            );
            return Err(ClusterError::Conflict(format!(
                "Preflight failed. {detail}"
            )));
        }
        self.progress
            .set_step("preflight", Some(&node_name), StepState::Done, None);

        // The grown configuration, and the running cluster's own key — the
        // newcomer joins the cluster that exists, not a fresh one.
        let topology = crate::topology::ClusterTopology::new(plan.definition.clone());
        let conf = topology.corosync_conf();
        let authkey = self.backend.authkey().await?;
        let old_conf =
            crate::topology::ClusterTopology::new(plan.old_definition.clone()).corosync_conf();

        // Prepare the newcomer whole.
        self.progress
            .set_step("prepare", Some(&node_name), StepState::Running, None);
        let payload = crate::join::PreparePayload {
            cluster: cluster.clone(),
            corosync_conf: conf.clone(),
            authkey: authkey.clone(),
            core: plan.core.clone(),
            management_interface: plan
                .networks
                .management
                .members
                .iter()
                .find(|m| m.node == plan.newcomer.name)
                .map(|m| m.interface.clone()),
        };
        if let Err(err) = self.peers.prepare(&plan.newcomer, &payload).await {
            self.progress.set_step(
                "prepare",
                Some(&node_name),
                StepState::Failed,
                Some(err.to_string()),
            );
            self.unwind_add_node(plan, &old_conf, false).await;
            return Err(ClusterError::Conflict(format!(
                "Preparing {node_name} failed: {err}"
            )));
        }
        self.progress
            .set_step("prepare", Some(&node_name), StepState::Done, None);

        // Reconfigure the running members: new conf, live reload — the
        // stack never stops, which is what keeps volume I/O flowing.
        let membership = self.require_membership()?;
        for member_name in plan.old_definition.node_names() {
            let Some(member) = membership.node(&member_name).cloned() else {
                continue;
            };
            self.progress
                .set_step("reconfigure", Some(&member.name), StepState::Running, None);
            let payload = crate::join::ReconfigurePayload {
                cluster: cluster.clone(),
                corosync_conf: conf.clone(),
                authkey: authkey.clone(),
            };
            if let Err(err) = self.peers.reconfigure(&member, &payload).await {
                self.progress.set_step(
                    "reconfigure",
                    Some(&member.name),
                    StepState::Failed,
                    Some(err.to_string()),
                );
                self.unwind_add_node(plan, &old_conf, true).await;
                return Err(ClusterError::Conflict(format!(
                    "Reconfiguring {} failed: {err}",
                    member.name
                )));
            }
            self.progress
                .set_step("reconfigure", Some(&member.name), StepState::Done, None);
        }

        // Start the newcomer and wait for the grown cluster to form.
        self.progress
            .set_step("start", Some(&node_name), StepState::Running, None);
        if let Err(err) = self.peers.start(&plan.newcomer).await {
            self.progress.set_step(
                "start",
                Some(&node_name),
                StepState::Failed,
                Some(err.to_string()),
            );
            self.unwind_add_node(plan, &old_conf, true).await;
            return Err(ClusterError::Conflict(format!(
                "Starting the stack on {node_name} failed: {err}"
            )));
        }
        self.progress
            .set_step("start", Some(&node_name), StepState::Done, None);

        self.progress
            .set_step("form", None, StepState::Running, None);
        let deadline = std::time::Instant::now() + crate::join::FORM_DEADLINE;
        loop {
            match self.backend.cluster_state(cluster).await {
                Ok(state)
                    if state.quorum.quorate
                        && plan
                            .definition
                            .nodes
                            .iter()
                            .all(|n| state.node(&n.name).is_some_and(|s| s.online)) =>
                {
                    break;
                }
                _ if std::time::Instant::now() > deadline => {
                    self.progress.set_step(
                        "form",
                        None,
                        StepState::Failed,
                        Some("the grown cluster never formed".into()),
                    );
                    self.unwind_add_node(plan, &old_conf, true).await;
                    return Err(ClusterError::Conflict(format!(
                        "The cluster never saw {node_name} join within {} seconds.",
                        crate::join::FORM_DEADLINE.as_secs()
                    )));
                }
                _ => tokio::time::sleep(self.form_poll).await,
            }
        }
        self.progress.set_step("form", None, StepState::Done, None);

        // The quorum regime's properties, and the delays flattened —
        // majority decides now, so no fence race needs biasing.
        self.progress
            .set_step("properties", None, StepState::Running, None);
        if let Err(err) = self
            .backend
            .set_pacemaker_properties(&topology.pacemaker_properties())
            .await
        {
            self.progress
                .set_step("properties", None, StepState::Failed, Some(err.to_string()));
            self.unwind_add_node(plan, &old_conf, true).await;
            return Err(ClusterError::Conflict(format!(
                "Setting cluster properties failed: {err}"
            )));
        }
        self.progress
            .set_step("properties", None, StepState::Done, None);

        self.progress
            .set_step("delays", None, StepState::Running, None);
        for device in topology.fence_devices() {
            if device.target == node_name {
                continue;
            }
            if let Err(err) = self
                .backend
                .update_fence_delay(&device.id, device.delay_base_secs)
                .await
            {
                self.progress
                    .set_step("delays", None, StepState::Failed, Some(err.to_string()));
                self.unwind_add_node(plan, &old_conf, true).await;
                return Err(ClusterError::Conflict(format!(
                    "Flattening the fence delays failed: {err}"
                )));
            }
        }
        self.progress
            .set_step("delays", None, StepState::Done, None);

        // The newcomer's fence device — created untested, and the warning
        // pins until its guarded live test runs. The test stays a deliberate
        // operator act (the M3 decision): a workflow that power-cycles a
        // node as a side effect would bypass the acknowledgement the test
        // was designed around.
        self.progress
            .set_step("fence", Some(&node_name), StepState::Running, None);
        let device = topology
            .fence_devices()
            .into_iter()
            .find(|d| d.target == node_name)
            .expect("the grown topology has a device per member");
        if let Err(err) = self
            .backend
            .create_fence_device(&device, &plan.bmc_password)
            .await
        {
            self.progress.set_step(
                "fence",
                Some(&node_name),
                StepState::Failed,
                Some(err.to_string()),
            );
            self.unwind_add_node(plan, &old_conf, true).await;
            return Err(ClusterError::Conflict(format!(
                "Creating the fence device for {node_name} failed: {err}"
            )));
        }
        self.progress
            .set_step("fence", Some(&node_name), StepState::Done, None);

        // Record, last.
        self.progress
            .set_step("record", None, StepState::Running, None);
        {
            let _guard = self.gate.lock().await;
            let mut membership = self.require_membership()?;
            if let Some(node) = membership.nodes.iter_mut().find(|n| n.name == node_name) {
                node.cluster = Some(cluster.clone());
            }
            if let Some(record) = membership
                .clusters
                .iter_mut()
                .find(|r| r.definition.name == *cluster)
            {
                record.definition = plan.definition.clone();
                record.networks = plan.networks.clone();
            }
            membership.version += 1;
            self.store.save_membership(&membership)?;
        }
        self.gossip_once().await;
        self.progress
            .set_step("record", None, StepState::Done, None);
        tracing::info!(cluster = %cluster, node = %node_name, "node added to the cluster");
        Ok(())
    }

    /// Put everything back: the newcomer torn down, the old configuration
    /// pushed to the members that had already been reconfigured.
    /// Best-effort throughout — the unwind reports, it does not fail.
    async fn unwind_add_node(&self, plan: &AddNodePlan, old_conf: &str, reconfigured: bool) {
        let payload = crate::join::TeardownPayload {
            cluster: plan.cluster.clone(),
            core_interface: Some(plan.core.interface.clone()),
            management_interface: plan
                .networks
                .management
                .members
                .iter()
                .find(|m| m.node == plan.newcomer.name)
                .map(|m| m.interface.clone()),
        };
        if let Err(err) = self.peers.teardown(&plan.newcomer, &payload).await {
            tracing::error!(node = %plan.newcomer.name, "the newcomer did not unwind: {err}");
        }
        if !reconfigured {
            return;
        }
        let Ok(authkey) = self.backend.authkey().await else {
            return;
        };
        let Ok(membership) = self.require_membership() else {
            return;
        };
        for member_name in plan.old_definition.node_names() {
            let Some(member) = membership.node(&member_name).cloned() else {
                continue;
            };
            let payload = crate::join::ReconfigurePayload {
                cluster: plan.cluster.clone(),
                corosync_conf: old_conf.to_string(),
                authkey: authkey.clone(),
            };
            if let Err(err) = self.peers.reconfigure(&member, &payload).await {
                tracing::error!(node = %member.name, "the old configuration did not restore: {err}");
            }
        }
    }

    /// A peer pushed a regenerated configuration: write it, reload corosync.
    pub async fn peer_reconfigure(&self, payload: &crate::join::ReconfigurePayload) -> Result<()> {
        self.backend
            .write_cluster_config(&payload.corosync_conf, &payload.authkey)
            .await?;
        self.backend.reload_corosync().await
    }

    // --- fencing ------------------------------------------------------------

    /// Live-test one fence direction: actually power-cycle `target` through
    /// its BMC, and record what happened on the membership record either way
    /// — a failed test is an answer about the fence path, not an error in
    /// the request, and it is exactly the news the untested warning exists
    /// to force out before an outage finds it first.
    pub async fn test_fence(
        &self,
        cluster: &str,
        target: &str,
        acknowledged: bool,
    ) -> Result<FenceTestView> {
        let _guard = self.gate.lock().await;
        if !acknowledged {
            return Err(ClusterError::invalid(ValidationError::new(
                ValidationCode::UnacknowledgedDestructiveOperation,
                Some("i_understand_this_power_cycles_the_node"),
                "A live fence test powers the target node off and on through its BMC — its \
                 machines migrate or restart. Acknowledge that to proceed.",
            )));
        }
        let mut membership = self.require_membership()?;
        if membership.cluster_record(cluster).is_none() {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        }
        if !membership
            .members_of(cluster)
            .iter()
            .any(|n| n.name == target)
        {
            return Err(ClusterError::NotFound(format!(
                "\"{target}\" is not a member of \"{cluster}\"."
            )));
        }
        if target == self.node {
            return Err(ClusterError::Conflict(format!(
                "A node does not run the test that powers itself off — the answer would go \
                 down with it. Run this from another member of \"{cluster}\"."
            )));
        }
        if membership
            .node(&self.node)
            .and_then(|n| n.cluster.as_deref())
            != Some(cluster)
        {
            return Err(ClusterError::Conflict(format!(
                "Fence tests run from inside the cluster. Open the console of a member of \
                 \"{cluster}\" — any of them except \"{target}\"."
            )));
        }
        let state = self.backend.cluster_state(cluster).await?;
        if state.fence_for(target).is_none() {
            return Err(ClusterError::Conflict(format!(
                "\"{target}\" has no fence device to test."
            )));
        }
        if !state.quorum.quorate
            || state
                .nodes
                .iter()
                .any(|n| !n.online || n.unclean || n.standby)
        {
            return Err(ClusterError::Conflict(
                "A fence test is for a healthy cluster: quorate, every member online, nobody \
                 in standby. Fencing a cluster that is already struggling is an outage, not a \
                 test."
                    .to_string(),
            ));
        }

        let result = self.backend.fence_node(target).await;
        let at = now_unix();
        let passed = result.is_ok();
        membership.version += 1;
        if let Some(record) = membership
            .clusters
            .iter_mut()
            .find(|r| r.definition.name == cluster)
        {
            record
                .fence_tests
                .insert(target.to_string(), FenceTest { at, passed });
        }
        self.store.save_membership(&membership)?;
        drop(_guard);
        self.gossip_once().await;
        tracing::info!(cluster, node = target, passed, "fence test recorded");
        Ok(FenceTestView {
            cluster: cluster.to_string(),
            node: target.to_string(),
            passed,
            at,
            error: result.err().map(|err| err.to_string()),
        })
    }

    /// Power a member down — or cycle it — through its fence device.
    ///
    /// The path that does not need the target's cooperation. A node whose
    /// operating system is wedged, whose console does not answer, or whose
    /// logind refuses cannot be restarted by asking it nicely; its BMC can
    /// still be told, and the cluster already holds those credentials for
    /// fencing. So this is the same device, the same routing, and the same
    /// journal-free password handling the fence test uses.
    ///
    /// Three guards, and each rules out a different way of being wrong:
    ///
    /// - **Never this node.** The command would go down with the answer, and
    ///   an operator would be left not knowing whether it was sent. A node
    ///   restarts *itself* through Maintenance, where the drain and the
    ///   quorum guard live.
    /// - **It must be a member of a cluster this node is in**, because the
    ///   fence device is the cluster's.
    /// - **It must have a fence device at all** — a cluster whose fencing was
    ///   never built has no power path, and saying so beats a command that
    ///   fails somewhere in Pacemaker.
    ///
    /// Deliberately *not* guarded on quorum or on the target being healthy:
    /// this is the operation an operator reaches for precisely when a node is
    /// unwell, and refusing it there would refuse it exactly when it is
    /// needed. The acknowledgement is what stands in for those guards — the
    /// caller has been told the machines on it stop.
    pub async fn power_member(
        &self,
        target: &str,
        action: crate::backend::HardPower,
        acknowledged: bool,
    ) -> Result<()> {
        if !acknowledged {
            return Err(ClusterError::invalid(ValidationError::new(
                ValidationCode::UnacknowledgedDestructiveOperation,
                Some("i_understand_this_cuts_the_power"),
                "This takes the power away at the machine — every virtual machine on it stops                  where it is, with no shutdown. Acknowledge that first.",
            )));
        }
        if target == self.node {
            return Err(ClusterError::Conflict(format!(
                "A node does not cut its own power this way — the answer would go down with                  it. Restart \"{target}\" from its own Maintenance page, where its machines                  are moved off first, or do this from another member."
            )));
        }
        let membership = self.require_membership()?;
        let Some(cluster) = membership
            .node(&self.node)
            .and_then(|node| node.cluster.clone())
        else {
            return Err(ClusterError::Conflict(
                "This node is not in a cluster, and the power path is the cluster's fence                  device."
                    .to_string(),
            ));
        };
        if membership
            .node(target)
            .and_then(|node| node.cluster.as_deref())
            != Some(cluster.as_str())
        {
            return Err(ClusterError::Conflict(format!(
                "\"{target}\" is not a member of \"{cluster}\", so this cluster has no fence                  device for it."
            )));
        }
        let state = self.backend.cluster_state(&cluster).await?;
        if state.fence_for(target).is_none() {
            return Err(ClusterError::Conflict(format!(
                "\"{target}\" has no fence device, so there is no power path to it. Fencing is                  what gives the cluster one."
            )));
        }
        self.backend.power_node(target, action).await?;
        tracing::warn!(
            node = target,
            ?action,
            "member powered through its fence device"
        );
        Ok(())
    }

    /// Break-glass: the operator vouches that an unfenced-unreachable node is
    /// powered off, so the cluster recovers as if fencing had succeeded.
    /// Offered in exactly one state — a member lost and not successfully
    /// fenced — because everywhere else it is either meaningless or a way to
    /// corrupt data with one request.
    pub async fn confirm_node_dead(
        &self,
        cluster: &str,
        target: &str,
        acknowledged: bool,
    ) -> Result<()> {
        let _guard = self.gate.lock().await;
        if !acknowledged {
            return Err(ClusterError::invalid(ValidationError::new(
                ValidationCode::UnacknowledgedDestructiveOperation,
                Some("i_have_verified_the_node_is_powered_off"),
                "Confirming a node dead makes the cluster recover as if fencing succeeded. If \
                 the node is in fact still running, both sides will write the same volumes and \
                 that data will not survive. Verify the power is off at the machine, then \
                 acknowledge that here.",
            )));
        }
        let membership = self.require_membership()?;
        if !membership
            .members_of(cluster)
            .iter()
            .any(|n| n.name == target)
        {
            return Err(ClusterError::NotFound(format!(
                "\"{target}\" is not a member of \"{cluster}\"."
            )));
        }
        if target == self.node {
            return Err(ClusterError::Conflict(
                "This node is answering the request, so it is not the dead one.".to_string(),
            ));
        }
        let state = self.backend.cluster_state(cluster).await?;
        let unclean = state.node(target).is_some_and(|n| n.unclean);
        if !unclean {
            return Err(ClusterError::Conflict(format!(
                "\"{target}\" is not waiting on a fence confirmation. Break-glass exists for \
                 exactly one state — a node that is unreachable and could not be fenced — and \
                 \"{target}\" is not in it."
            )));
        }
        self.backend.confirm_node_dead(target).await?;
        tracing::warn!(
            cluster,
            node = target,
            "operator confirmed the node dead; recovery proceeds without a successful fence"
        );
        Ok(())
    }

    // --- maintenance --------------------------------------------------------

    /// Take this node out of service, or put it back.
    ///
    /// Two things happen, in an order chosen so that no failure leaves a node
    /// that HA will act on while an operator is working on it. Going out, the
    /// **record is written first** and Pacemaker standby second: the record is
    /// what stops every member's HA manager treating this node's absence as a
    /// failure, and it must be true before anything starts moving. Coming
    /// back, the order reverses — standby is lifted first, and the flag is
    /// cleared only once Pacemaker will really run things here again.
    ///
    /// A failure part-way unwinds rather than leaving the pair disagreeing.
    /// A node flagged out of service but still holding the VIP is a node whose
    /// console says one thing and whose cluster does another, and that is a
    /// worse place to debug from than either end state.
    ///
    /// Local only, deliberately: standby is cluster-wide from any member, but
    /// what makes maintenance worth having is the evacuation that goes with it
    /// — and the machines can only be moved by the node that is running them.
    /// Rather than offer half the operation remotely, this refuses and names
    /// the console to open.
    pub async fn set_maintenance(
        &self,
        target: &str,
        on: bool,
        by: &str,
    ) -> Result<MaintenanceView> {
        let _guard = self.gate.lock().await;
        let mut membership = self.require_membership()?;
        let Some(record) = membership.node(target) else {
            return Err(ClusterError::NotFound(format!(
                "\"{target}\" is not a node in this environment."
            )));
        };
        if target != self.node {
            return Err(ClusterError::Conflict(format!(
                "Maintenance runs on the node it is about — its machines can only be moved by \
                 the node running them. Open the console of \"{target}\"."
            )));
        }
        let Some(cluster) = record.cluster.clone() else {
            return Err(ClusterError::Conflict(
                "This node is not in a cluster, so there is nowhere for its machines to go and \
                 nothing that would restart them. Shut it down when you are ready to work on it."
                    .to_string(),
            ));
        };

        let state = self.backend.cluster_state(&cluster).await?;
        if on {
            // Both of these are "the cluster is already having a bad day".
            // Taking a second node out of service now turns one problem into
            // an outage.
            if let Some(unclean) = state.nodes.iter().find(|n| n.unclean) {
                return Err(ClusterError::Conflict(format!(
                    "\"{}\" is lost and has not been fenced yet. Let the cluster finish with it \
                     before taking another node out of service.",
                    unclean.name
                )));
            }
            if !state.quorum.quorate {
                return Err(ClusterError::Conflict(
                    "This cluster is not quorate. Taking a node out of service now would leave \
                     less of it, not more."
                        .to_string(),
                ));
            }
        }

        let at = now_unix();
        let entry = on.then(|| Maintenance {
            since: at,
            by: by.to_string(),
        });
        let previous = record.maintenance.clone();
        if previous.is_some() == on {
            // Already where it was asked to be. Answer with the state rather
            // than bumping the record's counter for a no-op — the version is
            // how gossip decides who is newer, and spending it on nothing
            // makes every other node re-read the record for no reason.
            return Ok(MaintenanceView {
                node: target.to_string(),
                cluster,
                maintenance: previous,
                quorum_safe: quorum_survives_loss(&state.quorum),
            });
        }

        let write = |membership: &mut EnvironmentMembership, value: Option<Maintenance>| {
            membership.version += 1;
            if let Some(node) = membership.nodes.iter_mut().find(|n| n.name == target) {
                node.maintenance = value;
            }
        };

        if on {
            write(&mut membership, entry.clone());
            self.store.save_membership(&membership)?;
        }
        if let Err(err) = self.backend.set_standby(target, on).await {
            if on {
                // Unwind: the record claimed this node was out of service and
                // Pacemaker never agreed.
                write(&mut membership, previous);
                self.store.save_membership(&membership)?;
            }
            return Err(err);
        }
        if !on {
            write(&mut membership, None);
            self.store.save_membership(&membership)?;
        }

        drop(_guard);
        self.gossip_once().await;
        tracing::warn!(
            cluster,
            node = target,
            maintenance = on,
            "the node's service state changed"
        );
        Ok(MaintenanceView {
            node: target.to_string(),
            cluster,
            maintenance: entry,
            quorum_safe: quorum_survives_loss(&state.quorum),
        })
    }

    /// This node's maintenance state as the record has it, with no side
    /// effects — what the evacuation workflow checks before it starts and what
    /// the power guard consults before it lets a node go down.
    pub fn maintenance_of(&self, node: &str) -> Result<Option<Maintenance>> {
        Ok(self
            .environment_record()?
            .and_then(|m| m.node(node).and_then(|n| n.maintenance.clone())))
    }

    // --- replicated machine definitions -------------------------------------

    /// Keep a machine's definition on every member of this node's cluster —
    /// the HA manager's restart inventory, filled at define time because
    /// libvirt on a dead node cannot be asked for it. The home node is this
    /// one: definitions replicate from where the machine lives, so a
    /// migration or an HA restart moves the recorded home with it.
    /// Best-effort beyond the local copy: a peer that is down misses this
    /// push and is caught up by the next define, and failing the operator's
    /// action over it would make HA prep more important than the machine.
    pub async fn replicate_definition(&self, vmid: u32, xml: &str) -> Result<()> {
        let definition = crate::store::StoredDefinition {
            vmid,
            node: self.node.clone(),
            xml: xml.to_string(),
        };
        self.store.save_definition(&definition)?;
        for member in self.co_members()? {
            if let Err(err) = self.peers.store_definition(&member, &definition).await {
                tracing::warn!(
                    vmid,
                    peer = %member.name,
                    "the definition did not replicate: {err}"
                );
            }
        }
        Ok(())
    }

    /// Forget a machine's definition everywhere — it was deleted, and a
    /// stored definition for a machine that no longer exists is a machine
    /// waiting to be wrongly resurrected.
    pub async fn withdraw_definition(&self, vmid: u32) -> Result<()> {
        self.store.remove_definition(vmid)?;
        for member in self.co_members()? {
            if let Err(err) = self.peers.drop_definition(&member, vmid).await {
                tracing::warn!(
                    vmid,
                    peer = %member.name,
                    "the definition was not withdrawn: {err}"
                );
            }
        }
        Ok(())
    }

    /// The other members of this node's cluster — empty for a standalone or
    /// unassigned node, which makes definition replication a quiet local
    /// copy there.
    fn co_members(&self) -> Result<Vec<EnvironmentNode>> {
        let Some(membership) = self.membership()? else {
            return Ok(Vec::new());
        };
        let Some(cluster) = membership.node(&self.node).and_then(|n| n.cluster.clone()) else {
            return Ok(Vec::new());
        };
        Ok(membership
            .members_of(&cluster)
            .into_iter()
            .filter(|n| n.name != self.node)
            .cloned()
            .collect())
    }

    /// A peer handed this node a definition to keep.
    pub fn peer_store_definition(&self, definition: &crate::store::StoredDefinition) -> Result<()> {
        self.store.save_definition(definition)
    }

    /// A peer told this node to forget one.
    pub fn peer_drop_definition(&self, vmid: u32) -> Result<()> {
        self.store.remove_definition(vmid)
    }

    /// The stored definitions — the HA manager reads these when a node dies.
    pub fn stored_definitions(&self) -> Result<Vec<crate::store::StoredDefinition>> {
        self.store.definitions()
    }

    // --- the storage domain's door ------------------------------------------

    /// The whole membership record, read-only, for the domains built on top
    /// of this one: they need the cluster definitions and Core addresses to
    /// render and validate against, and re-deriving them through the view
    /// types would mean parsing our own presentation.
    pub fn environment_record(&self) -> Result<Option<EnvironmentMembership>> {
        self.membership()
    }

    // --- assembly ---------------------------------------------------------

    async fn cluster_view(&self, name: &str, membership: &EnvironmentMembership) -> ClusterView {
        let preferred = membership
            .cluster_record(name)
            .and_then(|r| r.definition.preferred_node.clone());
        match self.backend.cluster_state(name).await {
            Ok(mut state) => {
                // Pacemaker cannot say whether a device was ever proven —
                // only whether its monitor passes. The proof lives on the
                // membership record and is laid over the observed state here,
                // before health is derived from it.
                overlay_fence_tests(&mut state, membership.cluster_record(name));
                self.view_of(name, &state, membership, preferred)
            }
            Err(err) => {
                // The cluster could not be asked. Say so, list its members
                // from the record, and claim nothing about them — an
                // unreachable cluster shown honestly beats one dropped from
                // the answer.
                tracing::warn!(cluster = name, "cluster state unavailable: {err}");
                let nodes = membership
                    .members_of(name)
                    .into_iter()
                    .map(|member| ClusterNodeView {
                        node: member.name.clone(),
                        online: false,
                        standby: false,
                        unclean: false,
                        rings: Vec::new(),
                        fence: None,
                        address: Some(member.address.clone()),
                        controlplane_version: Some(member.controlplane_version.clone()),
                        // Known even here: maintenance is Lumen's own record,
                        // and a node being worked on is a common reason for
                        // the cluster to be unaskable in the first place.
                        maintenance: member.maintenance.clone(),
                        local: member.name == self.node,
                    })
                    .collect();
                ClusterView {
                    name: name.to_string(),
                    regime: Regime::of(membership.members_of(name).len()),
                    health: ClusterHealth::Unknown,
                    quorum: QuorumState::default(),
                    preferred_node: preferred,
                    nodes,
                    fence: FenceSummaryView::default(),
                    // The definition's address is known from the record even
                    // here; what Pacemaker has done about it is not.
                    vip: membership.cluster_record(name).and_then(|record| {
                        record.networks.management.vip.map(|address| VipView {
                            address,
                            state: None,
                        })
                    }),
                    error: Some(err.to_string()),
                }
            }
        }
    }

    fn view_of(
        &self,
        name: &str,
        state: &ClusterState,
        membership: &EnvironmentMembership,
        preferred_node: Option<String>,
    ) -> ClusterView {
        let nodes: Vec<ClusterNodeView> = state
            .nodes
            .iter()
            .map(|node| {
                let record = membership.node(&node.name);
                ClusterNodeView {
                    node: node.name.clone(),
                    online: node.online,
                    standby: node.standby,
                    unclean: node.unclean,
                    rings: node.rings.clone(),
                    fence: state.fence_for(&node.name).cloned(),
                    address: record.map(|r| r.address.clone()),
                    controlplane_version: record.map(|r| r.controlplane_version.clone()),
                    maintenance: record.and_then(|r| r.maintenance.clone()),
                    local: node.name == self.node,
                }
            })
            .collect();

        let fence = FenceSummaryView {
            devices: state.fence_devices.len(),
            healthy: state
                .fence_devices
                .iter()
                .filter(|d| d.active && !d.failed)
                .count(),
            failed: state.fence_devices.iter().filter(|d| d.failed).count(),
            untested: state
                .fence_devices
                .iter()
                .filter(|d| d.last_test.is_none())
                .count(),
        };

        ClusterView {
            name: name.to_string(),
            regime: if state.quorum.two_node {
                Regime::TwoNode
            } else {
                Regime::of(state.nodes.len())
            },
            health: health_of(state),
            quorum: state.quorum,
            preferred_node,
            nodes,
            fence,
            // Only when the definition asked for one. A cluster with no VIP
            // has nothing to report, and reporting Pacemaker's silence about
            // a resource nobody asked for would read as a fault.
            vip: membership.cluster_record(name).and_then(|record| {
                record.networks.management.vip.map(|address| VipView {
                    address,
                    state: state.vip.clone(),
                })
            }),
            error: None,
        }
    }

    fn unassigned_view(&self, node: &EnvironmentNode) -> UnassignedNodeView {
        UnassignedNodeView {
            node: node.name.clone(),
            address: Some(node.address.clone()),
            controlplane_version: Some(node.controlplane_version.clone()),
            local: node.name == self.node,
        }
    }
}

/// Whether the cluster would still be quorate if one more vote went away —
/// the question behind "is it safe to reboot this node".
///
/// The two-node regime answers yes on purpose. That regime exists precisely so
/// one survivor carries on, and it pays for it with fencing: `two_node` plus
/// `wait_for_all` means the survivor is quorate once it has fenced its peer,
/// and a cold start with only one node up is not. Applying the majority rule
/// there would refuse the one case the regime was chosen for.
pub fn quorum_survives_loss(quorum: &QuorumState) -> bool {
    if quorum.two_node {
        return true;
    }
    let majority = quorum.expected_votes / 2 + 1;
    quorum.votes.saturating_sub(1) >= majority
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs()
}

/// Lay the record's facts over the observed devices: the recorded fence
/// tests (the record wins where it has an answer; a device it has never
/// heard of keeps whatever the backend said — for the real backend, "never
/// tested"), and each device's IPMI target — crm_mon reports a device's
/// state but never its arguments, and the console showing "failing" without
/// saying against which BMC sends the operator to a terminal for the first
/// fact they need.
fn overlay_fence_tests(state: &mut ClusterState, record: Option<&ClusterRecord>) {
    let Some(record) = record else { return };
    for device in &mut state.fence_devices {
        if let Some(test) = record.fence_tests.get(&device.target) {
            device.last_test = Some(*test);
        }
        if let Some(member) = record
            .definition
            .nodes
            .iter()
            .find(|node| node.name == device.target)
        {
            device.bmc_address = Some(member.bmc.address.clone());
            device.bmc_username = Some(member.bmc.username.clone());
        }
    }
}

/// The one derivation of a cluster's health pill. Critical is reserved for
/// "data is at stake now": lost quorum, or a member lost and not yet fenced.
/// Everything an operator should fix but can schedule is Degraded — including
/// a failing or untested fence device, because IPMI is the only fence path
/// this appliance has.
fn health_of(state: &ClusterState) -> ClusterHealth {
    if !state.quorum.quorate || state.nodes.iter().any(|n| n.unclean) {
        return ClusterHealth::Critical;
    }
    let ring_degraded = state
        .nodes
        .iter()
        .any(|n| n.rings.iter().any(|r| !r.connected));
    let fence_worry = state.fence_devices.iter().any(|d| {
        d.failed || !d.active || d.last_test.is_none() || d.last_test.is_some_and(|t| !t.passed)
    });
    // A cluster VIP that exists in Pacemaker and is not running is an
    // address nobody answers on. Degraded rather than Critical: no data is at
    // stake, but every console bookmark pointing at the VIP is dead, which an
    // operator finds out at the worst possible moment otherwise.
    let vip_worry = state
        .vip
        .as_ref()
        .is_some_and(|vip| !vip.active || vip.failed || vip.blocked);
    if state.nodes.iter().any(|n| !n.online || n.standby)
        || ring_degraded
        || fence_worry
        || vip_worry
    {
        return ClusterHealth::Degraded;
    }
    ClusterHealth::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::{environment_membership, membership_of, MockBackend};
    use crate::join::{healthy_report, MockPeers, WorkflowPhase};
    use crate::validate::{CoreCreate, ManagementCreate, MemberCreate};

    fn test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lumen-cluster-service-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn network() -> Arc<lumen_net::NetworkService> {
        Arc::new(lumen_net::NetworkService::new(
            Arc::new(lumen_net::backend::mock::MockBackend::appliance()),
            &test_dir("net"),
            60,
        ))
    }

    fn service_with(
        backend: MockBackend,
        peers: MockPeers,
        membership: Option<&EnvironmentMembership>,
    ) -> Arc<ClusterService> {
        let mut service = ClusterService::new(
            Arc::new(backend),
            Arc::new(peers),
            network(),
            &test_dir("state"),
            "0.3.0",
        )
        .with_node("alpha-1")
        .with_form_poll(Duration::from_millis(5));
        if let Some(membership) = membership {
            service = service.with_environment(membership);
        }
        Arc::new(service)
    }

    fn create_request(nodes: &[&str]) -> ClusterCreate {
        ClusterCreate {
            name: "alpha".into(),
            preferred_node: (nodes.len() == 2).then(|| nodes[0].to_string()),
            core: CoreCreate {
                subnet: "10.10.0.0/24".into(),
                mtu: 9000,
            },
            management: ManagementCreate {
                subnet: "192.168.10.0/24".into(),
                vip: None,
            },
            members: nodes
                .iter()
                .enumerate()
                .map(|(i, node)| MemberCreate {
                    node: (*node).into(),
                    core_interface: "nic1".into(),
                    core_address: format!("10.10.0.{}", i + 1),
                    management_interface: "nic0".into(),
                    management_address: format!("192.168.10.{}", i + 1),
                    bmc_address: format!("10.20.0.{}", i + 1),
                    bmc_username: "ADMIN".into(),
                    bmc_password: "fence-pw".into(),
                })
                .collect(),
            external: Vec::new(),
        }
    }

    async fn finished(service: &Arc<ClusterService>) -> CreateProgress {
        for _ in 0..400 {
            if let Some(progress) = service.create_progress() {
                if progress.phase != WorkflowPhase::Running {
                    return progress;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the create never finished");
    }

    #[tokio::test]
    async fn a_node_with_no_environment_is_one_unassigned_node_not_an_error() {
        let service = service_with(MockBackend::appliance(), MockPeers::new(), None);
        let response = service.environment().await.unwrap();
        assert!(response.environment.is_none());
        assert!(response.clusters.is_empty());
        assert_eq!(response.unassigned.len(), 1);
        assert_eq!(response.unassigned[0].node, "alpha-1");
        assert!(response.unassigned[0].local);
    }

    #[tokio::test]
    async fn the_environment_answer_groups_by_cluster_then_by_node() {
        let service = service_with(
            MockBackend::environment(),
            MockPeers::new(),
            Some(&environment_membership()),
        );
        let response = service.environment().await.unwrap();

        let environment = response.environment.unwrap();
        assert_eq!(environment.nodes, 6);

        assert_eq!(response.clusters.len(), 2);
        assert_eq!(response.clusters[0].name, "alpha");
        assert_eq!(response.clusters[0].regime, Regime::TwoNode);
        assert_eq!(response.clusters[0].health, ClusterHealth::Ok);
        assert_eq!(response.clusters[0].nodes.len(), 2);
        assert_eq!(response.clusters[1].name, "beta");
        assert_eq!(response.clusters[1].regime, Regime::Quorum);
        assert_eq!(response.clusters[1].nodes.len(), 3);

        assert_eq!(response.unassigned.len(), 1);
        assert_eq!(response.unassigned[0].node, "spare-1");
        assert!(!response.unassigned[0].local);

        let locals: Vec<&str> = response
            .clusters
            .iter()
            .flat_map(|c| &c.nodes)
            .filter(|n| n.local)
            .map(|n| n.node.as_str())
            .collect();
        assert_eq!(locals, vec!["alpha-1"]);
    }

    #[tokio::test]
    async fn a_lost_unfenced_member_makes_the_cluster_critical() {
        let service = service_with(
            MockBackend::environment().with_partition("alpha", "alpha-2"),
            MockPeers::new(),
            Some(&environment_membership()),
        );
        let cluster = service.cluster("alpha").await.unwrap();
        assert_eq!(cluster.health, ClusterHealth::Critical);
        let lost = cluster.nodes.iter().find(|n| n.node == "alpha-2").unwrap();
        assert!(lost.unclean && !lost.online);
    }

    #[tokio::test]
    async fn a_failing_fence_device_degrades_the_cluster_and_is_counted() {
        let service = service_with(
            MockBackend::environment().with_fence_failure("alpha", "alpha-2"),
            MockPeers::new(),
            Some(&environment_membership()),
        );
        let cluster = service.cluster("alpha").await.unwrap();
        assert_eq!(cluster.health, ClusterHealth::Degraded);
        assert_eq!(cluster.fence.failed, 1);
        assert_eq!(cluster.fence.healthy, 1);
    }

    #[tokio::test]
    async fn an_unreachable_cluster_is_presented_not_dropped() {
        let service = service_with(
            MockBackend::environment().with_unreachable_cluster("beta"),
            MockPeers::new(),
            Some(&environment_membership()),
        );
        let response = service.environment().await.unwrap();
        assert_eq!(response.clusters.len(), 2, "beta must still be listed");
        let beta = &response.clusters[1];
        assert_eq!(beta.health, ClusterHealth::Unknown);
        assert!(beta.error.is_some());
        assert_eq!(beta.nodes.len(), 3);
        assert!(beta.nodes.iter().all(|n| !n.online && !n.unclean));
    }

    // --- environment membership -------------------------------------------

    #[tokio::test]
    async fn the_first_token_bootstraps_the_environment() {
        let service = service_with(MockBackend::appliance(), MockPeers::new(), None);
        let minted = service.mint_token("192.168.10.1:8443").await.unwrap();
        assert!(minted.bootstrapped);
        assert!(minted.token.starts_with("lumen-join/v1/"));

        let response = service.environment().await.unwrap();
        let environment = response.environment.expect("bootstrapped");
        assert_eq!(environment.nodes, 1);
        assert!(service.identity().unwrap().is_some());

        // The second mint reuses the environment.
        let again = service.mint_token("192.168.10.1:8443").await.unwrap();
        assert!(!again.bootstrapped);
    }

    #[tokio::test]
    async fn a_token_admits_a_node_once_and_only_once() {
        let issuer = service_with(MockBackend::appliance(), MockPeers::new(), None);
        let minted = issuer.mint_token("192.168.10.1:8443").await.unwrap();
        let token = JoinToken::decode(&minted.token).unwrap();

        let request = JoinRequest {
            token_id: token.id.clone(),
            secret: token.secret.clone(),
            node: "alpha-2".into(),
            address: "192.168.10.2:8443".into(),
            controlplane_version: "0.3.0".into(),
        };
        let secret = b"the-environment-secret";
        let grant = issuer.grant_join(&request, secret).await.unwrap();
        assert_eq!(grant.membership.nodes.len(), 2);
        assert!(grant.node_cert_pem.contains("BEGIN CERTIFICATE"));

        // Spent: the same token again is refused.
        let err = issuer.grant_join(&request, secret).await.unwrap_err();
        assert!(err.to_string().contains("one-time"), "{err}");
    }

    #[tokio::test]
    async fn a_wrong_secret_or_duplicate_name_is_refused() {
        let issuer = service_with(MockBackend::appliance(), MockPeers::new(), None);
        let minted = issuer.mint_token("192.168.10.1:8443").await.unwrap();
        let token = JoinToken::decode(&minted.token).unwrap();

        let mut request = JoinRequest {
            token_id: token.id.clone(),
            secret: "wrong".into(),
            node: "alpha-2".into(),
            address: "192.168.10.2:8443".into(),
            controlplane_version: "0.3.0".into(),
        };
        assert!(issuer.grant_join(&request, b"s").await.is_err());

        // A node that shares the issuer's own hostname is refused too — and
        // the refusal must not have spent the token.
        request.secret = token.secret.clone();
        request.node = "alpha-1".into();
        let err = issuer.grant_join(&request, b"s").await.unwrap_err();
        assert!(err.to_string().contains("share a hostname"), "{err}");
    }

    #[tokio::test]
    async fn joining_adopts_the_grant_whole() {
        use base64::Engine;
        // The grant a real issuer would send: alpha-1 (this test's node) has
        // just been added to an environment alpha-2 bootstrapped.
        let grant_membership = membership_of(&[("alpha-2", None), ("alpha-1", None)]);
        let peers = MockPeers::new().with_grant(JoinGrant {
            membership: grant_membership.clone(),
            ca_pem: "CA".into(),
            ca_key_pem: "CAKEY".into(),
            node_cert_pem: "CERT".into(),
            node_key_pem: "KEY".into(),
            session_secret: base64::engine::general_purpose::STANDARD.encode(b"shared-secret"),
        });
        let service = service_with(MockBackend::appliance(), peers, None);

        let token = JoinToken {
            issuer: "192.168.10.2:8443".into(),
            id: "tok".into(),
            secret: "s3cret".into(),
            fingerprint: "ab".repeat(32),
        };
        let outcome = service
            .join(&token.encode(), "192.168.10.1:8443")
            .await
            .unwrap();
        assert_eq!(outcome.session_secret, b"shared-secret");
        assert_eq!(
            service.identity().unwrap().unwrap().ca_pem,
            "CA".to_string()
        );
        let response = service.environment().await.unwrap();
        assert_eq!(response.environment.unwrap().nodes, 2);

        // Joining twice is refused: a node belongs to exactly one.
        let err = service
            .join(&token.encode(), "192.168.10.1:8443")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already"), "{err}");
    }

    #[tokio::test]
    async fn a_create_runs_to_completion_and_records_last() {
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let backend = Arc::new(MockBackend::appliance());
        let peers = MockPeers::new()
            .with_backend(backend.clone())
            .with_healthy_node("alpha-1", "0.3.0", &["nic0", "nic1"])
            .with_healthy_node("alpha-2", "0.3.0", &["nic0", "nic1"]);
        let service = Arc::new(
            ClusterService::new(
                backend.clone() as Arc<dyn ClusterBackend>,
                Arc::new(peers),
                network(),
                &test_dir("create-ok"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_form_poll(Duration::from_millis(5))
            .with_environment(&membership),
        );

        service
            .create_cluster(create_request(&["alpha-1", "alpha-2"]))
            .await
            .unwrap();
        let progress = finished(&service).await;
        assert_eq!(progress.phase, WorkflowPhase::Complete, "{progress:?}");

        // The record now carries the cluster, assignments included.
        let response = service.environment().await.unwrap();
        assert_eq!(response.clusters.len(), 1);
        assert_eq!(response.clusters[0].name, "alpha");
        assert_eq!(response.unassigned.len(), 0);

        // stonith-enabled reached the CIB.
        assert!(backend
            .properties_set()
            .contains(&("stonith-enabled".into(), "true".into())));

        // And so did one fence device per member, each with its password —
        // the delay biasing the fence race sits on the preferred node's own
        // device, exactly as the topology engine rendered it.
        let devices = backend.fence_devices_created();
        assert_eq!(devices.len(), 2);
        assert!(devices.iter().all(|(_, password)| password == "fence-pw"));
        let delay_of = |target: &str| {
            devices
                .iter()
                .find(|(d, _)| d.target == target)
                .map(|(d, _)| d.delay_base_secs)
        };
        assert_eq!(
            delay_of("alpha-1"),
            Some(crate::model::FENCE_RACE_DELAY_SECS)
        );
        assert_eq!(delay_of("alpha-2"), Some(0));

        // The wizard's plan carried the fence steps, and they finished.
        assert!(progress
            .steps
            .iter()
            .filter(|s| s.step == "fence")
            .all(|s| s.state == crate::join::StepState::Done));
    }

    /// A prepare opens the ports the cluster's own traffic needs, on the
    /// interfaces that carry it, and a teardown closes them again.
    ///
    /// This is the step whose absence let a cluster form and replicate
    /// nowhere: the package ships the firewalld service definitions, but a
    /// definition names ports and binds nothing, so replication sat waiting
    /// to connect and the first machine to want its disk could not be
    /// started.
    #[tokio::test]
    async fn a_prepare_opens_the_clusters_ports_and_a_teardown_closes_them() {
        let backend = Arc::new(MockBackend::appliance());
        let service = Arc::new(
            ClusterService::new(
                backend.clone() as Arc<dyn ClusterBackend>,
                Arc::new(MockPeers::new()),
                network(),
                &test_dir("ports"),
                "0.3.0",
            )
            .with_node("alpha-1"),
        );

        let payload = crate::join::PreparePayload {
            cluster: "alpha".into(),
            corosync_conf: "totem {}\n".into(),
            authkey: "k".into(),
            core: crate::join::CoreAssignment {
                interface: "nic1".into(),
                address: "10.10.0.1".parse().unwrap(),
                prefix: 24,
                mtu: 9000,
            },
            management_interface: Some("nic0".into()),
        };
        service.peer_prepare(&payload).await.unwrap();
        assert_eq!(
            backend.cluster_ports(),
            vec![("nic1".to_string(), Some("nic0".to_string()), true)]
        );
        // Clustering opens the hypervisor's migration door...
        assert_eq!(backend.migration_listener(), vec![true]);
        // The configuration still landed — opening the ports is a step
        // before it, not instead of it.
        assert_eq!(backend.written_configs().len(), 1);

        service
            .peer_teardown(&crate::join::TeardownPayload {
                cluster: "alpha".into(),
                core_interface: Some("nic1".into()),
                management_interface: Some("nic0".into()),
            })
            .await
            .unwrap();
        assert_eq!(
            backend.cluster_ports().last().unwrap(),
            &("nic1".to_string(), Some("nic0".to_string()), false)
        );
        // ...and teardown closes it.
        assert_eq!(backend.migration_listener(), vec![true, false]);
    }

    /// The prepare payload carries the Management interface, so the node
    /// receiving it can open the second corosync ring's ports too.
    #[tokio::test]
    async fn a_prepared_member_is_told_both_of_its_cluster_interfaces() {
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(
            MockPeers::new()
                .with_backend(backend.clone())
                .with_healthy_node("alpha-1", "0.3.0", &["nic0", "nic1"])
                .with_healthy_node("alpha-2", "0.3.0", &["nic0", "nic1"]),
        );
        let service = Arc::new(
            ClusterService::new(
                backend.clone() as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir("prepare-interfaces"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_form_poll(Duration::from_millis(5))
            .with_environment(&membership),
        );

        service
            .create_cluster(create_request(&["alpha-1", "alpha-2"]))
            .await
            .unwrap();
        finished(&service).await;

        let prepared = peers.prepared();
        assert_eq!(prepared.len(), 2, "{prepared:?}");
        for (node, payload) in &prepared {
            assert!(
                payload.management_interface.is_some(),
                "{node} was told no Management interface: {payload:?}"
            );
            assert_ne!(
                payload.management_interface.as_deref(),
                Some(payload.core.interface.as_str()),
                "{node}: the two rings must not be read as one interface"
            );
        }
    }

    /// The acceptance criterion: a create failed mid-way unwinds completely —
    /// every node ends unassigned and configuration-free, and the wizard says
    /// which step failed on which node.
    #[tokio::test]
    async fn a_failed_create_unwinds_completely() {
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let backend = Arc::new(MockBackend::appliance());
        let peers = MockPeers::new()
            .with_backend(backend.clone())
            .with_healthy_node("alpha-1", "0.3.0", &["nic0", "nic1"])
            .with_healthy_node("alpha-2", "0.3.0", &["nic0", "nic1"])
            .fail_start_on("alpha-2");
        let peers = Arc::new(peers);
        let service = Arc::new(
            ClusterService::new(
                backend.clone() as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir("create-unwind"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_form_poll(Duration::from_millis(5))
            .with_environment(&membership),
        );

        service
            .create_cluster(create_request(&["alpha-1", "alpha-2"]))
            .await
            .unwrap();
        let progress = finished(&service).await;
        assert_eq!(progress.phase, WorkflowPhase::Failed);
        let error = progress.error.as_deref().unwrap_or_default();
        assert!(
            error.contains("alpha-2"),
            "the error names the node: {error}"
        );

        // Both members were torn down — the one that failed included — and
        // each teardown named the Core interface to release.
        let torn = peers.torn_down();
        assert_eq!(torn.len(), 2, "{torn:?}");
        assert!(torn
            .iter()
            .all(|(_, payload)| payload.core_interface.as_deref() == Some("nic1")));

        // And the record never knew the cluster existed.
        let response = service.environment().await.unwrap();
        assert!(response.clusters.is_empty());
        assert_eq!(response.unassigned.len(), 2);
    }

    #[tokio::test]
    async fn preflight_problems_block_before_anything_is_touched() {
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let backend = Arc::new(MockBackend::appliance());
        let mut bad = healthy_report("alpha-2", "0.3.0", &["nic0", "nic1"]);
        bad.time_synchronized = false;
        let peers = MockPeers::new()
            .with_backend(backend.clone())
            .with_healthy_node("alpha-1", "0.3.0", &["nic0", "nic1"])
            .with_report("alpha-2", bad);
        let peers = Arc::new(peers);
        let service = Arc::new(
            ClusterService::new(
                backend as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir("create-preflight"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_form_poll(Duration::from_millis(5))
            .with_environment(&membership),
        );

        service
            .create_cluster(create_request(&["alpha-1", "alpha-2"]))
            .await
            .unwrap();
        let progress = finished(&service).await;
        assert_eq!(progress.phase, WorkflowPhase::Failed);
        assert!(
            peers.prepared().is_empty(),
            "nothing may have been prepared"
        );
        assert!(progress
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("chrony"));
    }

    /// The wizard's Core-redundancy shortcut: the coordinator reaches the
    /// member through the peer channel, and the bond lands in that node's
    /// networking domain rather than in anything the cluster owns.
    #[tokio::test]
    async fn a_core_bond_is_built_on_the_member_through_its_own_networking() {
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let peers = Arc::new(
            MockPeers::new()
                .with_healthy_node("alpha-1", "0.3.0", &["nic0", "nic1", "nic2"])
                .with_healthy_node("alpha-2", "0.3.0", &["nic0", "nic1", "nic2"]),
        );
        let service = Arc::new(
            ClusterService::new(
                Arc::new(MockBackend::appliance()) as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir("core-bond"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_environment(&membership),
        );

        let bond = lumen_net::Bond {
            name: "bond0".into(),
            mode: lumen_net::BondMode::ActiveBackup,
            ports: vec!["nic1".into(), "nic2".into()],
            miimon: Some(100),
            ..lumen_net::Bond::default()
        };
        service.bond_node_nics("alpha-2", &bond).await.unwrap();

        let built = peers.bonds();
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].0, "alpha-2");
        assert_eq!(
            built[0].1.ports,
            vec!["nic1".to_string(), "nic2".to_string()]
        );

        // …and the node reports it on the next preflight, so the wizard's
        // Core picker can actually offer the seat it just created.
        let views = service.preflight(&["alpha-2".to_string()]).await.unwrap();
        let links = &views[0].report.as_ref().unwrap().links;
        let made = links.iter().find(|l| l.name == "bond0").expect("the bond");
        assert_eq!(made.kind, lumen_net::LinkKind::Bond);
    }

    #[tokio::test]
    async fn a_bond_of_one_port_is_refused_and_a_stranger_cannot_be_configured() {
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let peers =
            Arc::new(MockPeers::new().with_healthy_node("alpha-2", "0.3.0", &["nic0", "nic1"]));
        let service = Arc::new(
            ClusterService::new(
                Arc::new(MockBackend::appliance()) as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir("core-bond-refused"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_environment(&membership),
        );

        // One port is the cable the bond was meant to survive.
        let single = lumen_net::Bond {
            name: "bond0".into(),
            ports: vec!["nic1".into()],
            ..lumen_net::Bond::default()
        };
        assert!(service.bond_node_nics("alpha-2", &single).await.is_err());

        let good = lumen_net::Bond {
            name: "bond0".into(),
            ports: vec!["nic1".into(), "nic2".into()],
            ..lumen_net::Bond::default()
        };
        assert!(service.bond_node_nics("outsider", &good).await.is_err());
        assert!(peers.bonds().is_empty(), "nothing may have been built");
    }

    /// A bond takes its ports over entirely, so enslaving the link the
    /// console answers on would cut the operator off mid-wizard — and the
    /// confirm window cannot rescue a session severed by the change itself.
    #[tokio::test]
    async fn a_bond_refuses_to_swallow_an_addressed_link() {
        let network = network();
        let service = ClusterService::new(
            Arc::new(MockBackend::appliance()),
            Arc::new(MockPeers::new()),
            network.clone(),
            &test_dir("core-bond-addressed"),
            "0.3.0",
        )
        .with_node("alpha-1");

        let addressed = network
            .observe()
            .await
            .unwrap()
            .links
            .into_iter()
            .find(|link| !link.addresses.is_empty())
            .expect("the appliance fixture has an addressed link");

        let bond = lumen_net::Bond {
            name: "bond0".into(),
            ports: vec![addressed.name.clone(), "nic1".into()],
            ..lumen_net::Bond::default()
        };
        let err = service.peer_create_bond(&bond).await.unwrap_err();
        assert!(
            err.to_string().contains(&addressed.name),
            "the refusal names the link: {err}"
        );
    }

    #[tokio::test]
    async fn destroying_a_cluster_needs_the_acknowledgement_and_every_member() {
        let mut membership =
            membership_of(&[("alpha-1", Some("alpha")), ("alpha-2", Some("alpha"))]);
        let request = create_request(&["alpha-1", "alpha-2"]);
        let (definition, networks, _) = request.build().unwrap();
        membership
            .clusters
            .push(crate::environment::ClusterRecord::new(definition, networks));

        let peers = Arc::new(MockPeers::new());
        let service = Arc::new(
            ClusterService::new(
                Arc::new(MockBackend::environment()) as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir("destroy"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_environment(&membership),
        );

        let refused = service
            .destroy_cluster("alpha", Acknowledgements::default())
            .await
            .unwrap_err();
        assert!(matches!(refused, ClusterError::Invalid(_)));

        service
            .destroy_cluster(
                "alpha",
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(peers.torn_down().len(), 2);
        let response = service.environment().await.unwrap();
        assert!(response.clusters.is_empty());
        assert_eq!(response.unassigned.len(), 2);
    }

    #[tokio::test]
    async fn an_unassigned_node_can_be_removed_but_a_member_cannot() {
        let membership = membership_of(&[
            ("alpha-1", None),
            ("beta-1", Some("beta")),
            ("spare-1", None),
        ]);
        let service = service_with(
            MockBackend::appliance(),
            MockPeers::new(),
            Some(&membership),
        );

        service.remove_node("spare-1").await.unwrap();
        let err = service.remove_node("beta-1").await.unwrap_err();
        assert!(err.to_string().contains("beta"), "{err}");
        let err = service.remove_node("alpha-1").await.unwrap_err();
        assert!(
            err.to_string().contains("another environment node"),
            "{err}"
        );

        let response = service.environment().await.unwrap();
        assert_eq!(
            response
                .unassigned
                .iter()
                .map(|n| n.node.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha-1"]
        );
    }

    // --- core network edit --------------------------------------------------

    /// A service standing on a formed two-node cluster, every member with a
    /// spare `nic2`, and the mock peers kept out so a test can read what
    /// crossed the channel.
    fn core_edit_harness(
        tag: &str,
        prime: impl FnOnce(MockPeers) -> MockPeers,
    ) -> (Arc<ClusterService>, Arc<MockPeers>) {
        let peers = Arc::new(prime(
            MockPeers::new()
                .with_healthy_node("alpha-1", "0.3.0", &["nic0", "nic1", "nic2"])
                .with_healthy_node("alpha-2", "0.3.0", &["nic0", "nic1", "nic2"]),
        ));
        let service = Arc::new(
            ClusterService::new(
                Arc::new(MockBackend::appliance()) as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir(tag),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_environment(&clustered_membership()),
        );
        (service, peers)
    }

    #[tokio::test]
    async fn the_core_mtu_changes_on_every_member_and_the_record_last() {
        let (service, peers) = core_edit_harness("core-mtu", |p| p);
        let core = service
            .update_core_network(
                "alpha",
                CoreNetworkUpdate {
                    mtu: Some(1500),
                    members: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(core.mtu, 1500);

        let seats = peers.core_seats();
        assert_eq!(
            seats.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["alpha-1", "alpha-2"]
        );
        // An MTU change moves no seat: no old interface, same links.
        assert!(seats
            .iter()
            .all(|(_, u)| u.old_interface.is_none() && u.core.mtu == 1500));
        assert_eq!(service.cluster_networks("alpha").unwrap().core.mtu, 1500);
    }

    #[tokio::test]
    async fn a_core_seat_moves_to_another_link_and_only_that_member_is_touched() {
        let (service, peers) = core_edit_harness("core-move", |p| p);
        let mut members = service.cluster_networks("alpha").unwrap().core.members;
        members
            .iter_mut()
            .find(|m| m.node == "alpha-2")
            .unwrap()
            .interface = "nic2".into();

        let core = service
            .update_core_network(
                "alpha",
                CoreNetworkUpdate {
                    mtu: None,
                    members: Some(members),
                },
            )
            .await
            .unwrap();

        let seat = |node: &str| {
            core.members
                .iter()
                .find(|m| m.node == node)
                .unwrap()
                .clone()
        };
        assert_eq!(seat("alpha-2").interface, "nic2");
        // The address went nowhere — the ring's name for the member is the
        // one thing a seat move must not touch.
        assert_eq!(seat("alpha-2").address.to_string(), "10.10.0.2");

        let seats = peers.core_seats();
        assert_eq!(seats.len(), 1, "{seats:?}");
        let (node, update) = &seats[0];
        assert_eq!(node, "alpha-2");
        assert_eq!(update.old_interface.as_deref(), Some("nic1"));
        assert_eq!(update.core.interface, "nic2");
        assert_eq!(update.core.mtu, 9000);

        let recorded = service.cluster_networks("alpha").unwrap().core;
        assert_eq!(
            recorded
                .members
                .iter()
                .find(|m| m.node == "alpha-2")
                .unwrap()
                .interface,
            "nic2"
        );
    }

    #[tokio::test]
    async fn the_rings_identity_cannot_change_through_the_edit() {
        let (service, peers) = core_edit_harness("core-identity", |p| p);

        // A different address is refused before any member is asked.
        let mut members = service.cluster_networks("alpha").unwrap().core.members;
        members
            .iter_mut()
            .find(|m| m.node == "alpha-2")
            .unwrap()
            .address = "10.10.0.9".parse().unwrap();
        let error = service
            .update_core_network(
                "alpha",
                CoreNetworkUpdate {
                    mtu: None,
                    members: Some(members),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("destroy"), "{error}");

        // So is dropping a seat.
        let members: Vec<_> = service
            .cluster_networks("alpha")
            .unwrap()
            .core
            .members
            .into_iter()
            .filter(|m| m.node != "alpha-2")
            .collect();
        let error = service
            .update_core_network(
                "alpha",
                CoreNetworkUpdate {
                    mtu: None,
                    members: Some(members),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("alpha-2"), "{error}");

        // And an edit that changes nothing says so.
        let error = service
            .update_core_network(
                "alpha",
                CoreNetworkUpdate {
                    mtu: Some(9000),
                    members: None,
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("differs"), "{error}");

        assert!(peers.core_seats().is_empty());
    }

    #[tokio::test]
    async fn a_seat_cannot_move_onto_a_link_the_member_does_not_have() {
        let (service, peers) = core_edit_harness("core-missing-link", |p| p);
        let mut members = service.cluster_networks("alpha").unwrap().core.members;
        members
            .iter_mut()
            .find(|m| m.node == "alpha-2")
            .unwrap()
            .interface = "nic9".into();
        let error = service
            .update_core_network(
                "alpha",
                CoreNetworkUpdate {
                    mtu: None,
                    members: Some(members),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("nic9"), "{error}");
        assert!(peers.core_seats().is_empty());
    }

    #[tokio::test]
    async fn a_member_that_fails_leaves_the_record_on_the_old_definition() {
        let (service, peers) = core_edit_harness("core-fail", |p| p.fail_core_seat_on("alpha-2"));
        let error = service
            .update_core_network(
                "alpha",
                CoreNetworkUpdate {
                    mtu: Some(1500),
                    members: None,
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("alpha-2"), "{error}");

        // The member before it changed; the record did not — the error told
        // the operator to fix the member and ask again.
        assert_eq!(
            peers
                .core_seats()
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha-1"]
        );
        assert_eq!(service.cluster_networks("alpha").unwrap().core.mtu, 9000);
    }

    /// The member's own half, against the real (mock-backed) networking
    /// domain: the release of the old link and the addressing of the new one
    /// are one staged apply, the firewall bindings follow the seat, and a
    /// retry stages nothing.
    #[tokio::test]
    async fn a_seat_move_releases_the_old_link_and_rebinds_the_ports() {
        use lumen_net::{IpConfig, LinkKind, LinkState, ObservedLink, ObservedState};
        let observed = ObservedState {
            node: "alpha-1".into(),
            links: vec![
                ObservedLink {
                    name: "nic0".into(),
                    kind: LinkKind::Ethernet,
                    state: LinkState::Activated,
                    managed: true,
                    carrier: true,
                    mtu: Some(1500),
                    addresses: vec!["192.168.10.1/24".into()],
                    gateway: Some("192.168.10.254".into()),
                    ip: IpConfig::Static {
                        cidr: "192.168.10.1/24".into(),
                        gateway: "192.168.10.254".into(),
                        dns: Vec::new(),
                    },
                    connection_uuid: Some("uuid-nic0".into()),
                    ..ObservedLink::default()
                },
                ObservedLink {
                    name: "nic1".into(),
                    kind: LinkKind::Ethernet,
                    state: LinkState::Activated,
                    managed: true,
                    carrier: true,
                    mtu: Some(9000),
                    addresses: vec!["10.10.0.1/24".into()],
                    ip: IpConfig::Static {
                        cidr: "10.10.0.1/24".into(),
                        gateway: String::new(),
                        dns: Vec::new(),
                    },
                    connection_uuid: Some("uuid-nic1".into()),
                    ..ObservedLink::default()
                },
                ObservedLink {
                    name: "nic2".into(),
                    kind: LinkKind::Ethernet,
                    state: LinkState::Disconnected,
                    managed: true,
                    carrier: true,
                    mtu: Some(1500),
                    ..ObservedLink::default()
                },
            ],
        };
        let net_backend = Arc::new(lumen_net::backend::mock::MockBackend::new(observed));
        let network = Arc::new(lumen_net::NetworkService::new(
            net_backend.clone(),
            &test_dir("seat-move-net"),
            60,
        ));
        let backend = Arc::new(MockBackend::appliance());
        let service = ClusterService::new(
            backend.clone() as Arc<dyn ClusterBackend>,
            Arc::new(MockPeers::new()),
            network,
            &test_dir("seat-move"),
            "0.3.0",
        )
        .with_node("alpha-1");

        let update = crate::join::CoreSeatUpdate {
            cluster: "alpha".into(),
            old_interface: Some("nic1".into()),
            core: crate::join::CoreAssignment {
                interface: "nic2".into(),
                address: "10.10.0.1".parse().unwrap(),
                prefix: 24,
                mtu: 9000,
            },
        };
        service.peer_update_core_seat(&update).await.unwrap();

        let state = net_backend.state();
        let nic1 = state.link("nic1").unwrap();
        let nic2 = state.link("nic2").unwrap();
        assert!(
            !nic1.addresses.iter().any(|a| a == "10.10.0.1/24"),
            "{nic1:?}"
        );
        assert!(
            nic2.addresses.iter().any(|a| a == "10.10.0.1/24"),
            "{nic2:?}"
        );
        assert_eq!(nic2.mtu, Some(9000));
        assert_eq!(
            backend.cluster_ports(),
            vec![("nic1".into(), None, false), ("nic2".into(), None, true)]
        );

        // A retry — the coordinator's "ask again" after a failure elsewhere
        // in the walk — finds the box already right and stages nothing.
        let before = net_backend.applied().len();
        service.peer_update_core_seat(&update).await.unwrap();
        assert_eq!(net_backend.applied().len(), before);
    }

    // --- fencing ------------------------------------------------------------

    /// A membership record that knows cluster "alpha" whole — the shape a
    /// completed create leaves behind.
    fn clustered_membership() -> EnvironmentMembership {
        let mut membership =
            membership_of(&[("alpha-1", Some("alpha")), ("alpha-2", Some("alpha"))]);
        let (definition, networks, _) = create_request(&["alpha-1", "alpha-2"]).build().unwrap();
        membership
            .clusters
            .push(ClusterRecord::new(definition, networks));
        membership
    }

    fn fence_harness(backend: Arc<MockBackend>) -> Arc<ClusterService> {
        Arc::new(
            ClusterService::new(
                backend as Arc<dyn ClusterBackend>,
                Arc::new(MockPeers::new()),
                network(),
                &test_dir("fence"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_environment(&clustered_membership()),
        )
    }

    /// The power path exists for a node whose own operating system cannot be
    /// asked, so its guards are about *who* and *whether there is a device* —
    /// never about the target being healthy, which is the state it is for.
    #[tokio::test]
    async fn powering_a_member_needs_the_acknowledgement_and_never_this_node() {
        let backend = Arc::new(MockBackend::environment());
        let service = fence_harness(backend.clone());

        // Unacknowledged: refused before anything is asked of the device.
        let err = service
            .power_member("alpha-2", crate::backend::HardPower::Off, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Acknowledge"), "{err}");
        assert!(backend.powered().is_empty());

        // This node: the answer would go down with the command.
        let err = service
            .power_member("alpha-1", crate::backend::HardPower::Cycle, true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Maintenance"), "{err}");
        assert!(backend.powered().is_empty());

        // A node outside this cluster has no fence device here.
        let err = service
            .power_member("stranger", crate::backend::HardPower::Off, true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a member"), "{err}");

        // Acknowledged, another member, a device present: it goes.
        service
            .power_member("alpha-2", crate::backend::HardPower::Cycle, true)
            .await
            .unwrap();
        assert_eq!(
            backend.powered(),
            vec![("alpha-2".to_string(), crate::backend::HardPower::Cycle)]
        );
    }

    #[tokio::test]
    async fn a_fence_test_needs_the_acknowledgement_and_never_tests_its_own_direction() {
        let backend = Arc::new(MockBackend::environment().with_untested_fencing("alpha"));
        let service = fence_harness(backend.clone());

        let refused = service
            .test_fence("alpha", "alpha-2", false)
            .await
            .unwrap_err();
        assert!(matches!(refused, ClusterError::Invalid(_)));

        // The test powers its target off, and this node is the target.
        let own = service
            .test_fence("alpha", "alpha-1", true)
            .await
            .unwrap_err();
        assert!(own.to_string().contains("another member"), "{own}");
        assert!(backend.fenced().is_empty(), "nothing may have been fenced");
    }

    #[tokio::test]
    async fn a_passed_fence_test_is_recorded_and_clears_that_directions_warning() {
        let backend = Arc::new(MockBackend::environment().with_untested_fencing("alpha"));
        let service = fence_harness(backend.clone());

        let view = service.test_fence("alpha", "alpha-2", true).await.unwrap();
        assert!(view.passed && view.error.is_none());
        assert_eq!(backend.fenced(), vec!["alpha-2"]);

        // The record carries the answer, so the view shows this direction
        // proven and the other still waiting.
        let cluster = service.cluster("alpha").await.unwrap();
        assert_eq!(cluster.fence.untested, 1, "{:?}", cluster.fence);
        let tested = cluster.nodes.iter().find(|n| n.node == "alpha-2").unwrap();
        assert!(tested.fence.as_ref().unwrap().last_test.unwrap().passed);
    }

    /// A failed test is an answer, not an error: recorded, shown, and it
    /// keeps the cluster degraded even though the device's monitor passes.
    #[tokio::test]
    async fn a_failed_fence_test_is_an_answer_that_degrades_the_cluster() {
        let backend = Arc::new(MockBackend::environment().with_untested_fencing("alpha"));
        let service = fence_harness(backend.clone());

        backend.fail_fence("the BMC refused the power command");
        let view = service.test_fence("alpha", "alpha-2", true).await.unwrap();
        assert!(!view.passed);
        assert!(
            view.error.as_deref().unwrap_or("").contains("BMC"),
            "{view:?}"
        );

        let cluster = service.cluster("alpha").await.unwrap();
        let device = cluster
            .nodes
            .iter()
            .find(|n| n.node == "alpha-2")
            .and_then(|n| n.fence.clone())
            .unwrap();
        assert!(!device.last_test.unwrap().passed);
        assert_eq!(cluster.health, ClusterHealth::Degraded);
    }

    #[tokio::test]
    async fn a_fence_test_is_for_a_healthy_cluster_only() {
        let backend = Arc::new(
            MockBackend::environment()
                .with_untested_fencing("alpha")
                .with_partition("alpha", "alpha-2"),
        );
        let service = fence_harness(backend.clone());
        let refused = service
            .test_fence("alpha", "alpha-2", true)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("healthy cluster"), "{refused}");
        assert!(backend.fenced().is_empty());
    }

    #[tokio::test]
    async fn break_glass_confirms_only_an_unclean_peer_and_recovery_follows() {
        // A healthy peer is not confirmable — there is nothing to vouch for.
        let healthy = fence_harness(Arc::new(MockBackend::environment()));
        let refused = healthy
            .confirm_node_dead("alpha", "alpha-2", true)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("not waiting"), "{refused}");

        // The state break-glass exists for: lost, and fencing could not
        // prove it dead.
        let backend = Arc::new(
            MockBackend::environment()
                .with_partition("alpha", "alpha-2")
                .with_fence_failure("alpha", "alpha-2"),
        );
        let service = fence_harness(backend.clone());
        assert_eq!(
            service.cluster("alpha").await.unwrap().health,
            ClusterHealth::Critical
        );

        let unacked = service
            .confirm_node_dead("alpha", "alpha-2", false)
            .await
            .unwrap_err();
        assert!(matches!(unacked, ClusterError::Invalid(_)));

        // This node cannot be the dead one — it is answering.
        let own = service
            .confirm_node_dead("alpha", "alpha-1", true)
            .await
            .unwrap_err();
        assert!(own.to_string().contains("not the dead one"), "{own}");

        service
            .confirm_node_dead("alpha", "alpha-2", true)
            .await
            .unwrap();
        assert_eq!(backend.confirmed_dead(), vec!["alpha-2"]);
        // Vouched for: no longer critical — degraded, because a member is
        // still down.
        assert_eq!(
            service.cluster("alpha").await.unwrap().health,
            ClusterHealth::Degraded
        );
    }

    #[tokio::test]
    async fn definitions_replicate_to_co_members_and_withdraw_everywhere() {
        let peers = Arc::new(MockPeers::new());
        let membership = membership_of(&[
            ("alpha-1", Some("alpha")),
            ("alpha-2", Some("alpha")),
            ("beta-1", Some("beta")),
            ("spare-1", None),
        ]);
        let network = network();
        let service = Arc::new(
            ClusterService::new(
                Arc::new(MockBackend::environment()) as Arc<dyn ClusterBackend>,
                peers.clone(),
                network,
                &test_dir("definitions"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_environment(&membership),
        );

        service
            .replicate_definition(101, "<domain>web01</domain>")
            .await
            .unwrap();
        // The local copy exists with this node as home, and exactly the
        // cluster's other members — not beta-1, not the spare — were pushed.
        let stored = service.stored_definitions().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].vmid, 101);
        assert_eq!(stored[0].node, "alpha-1", "home travels with it");
        assert_eq!(stored[0].xml, "<domain>web01</domain>");
        let pushed = peers.definitions();
        assert_eq!(pushed.len(), 1, "{pushed:?}");
        assert_eq!(pushed[0].0, "alpha-2");
        assert_eq!(pushed[0].1.vmid, 101);
        assert_eq!(pushed[0].1.node, "alpha-1");

        // The peer side stores what it is handed, home included.
        service
            .peer_store_definition(&crate::store::StoredDefinition {
                vmid: 102,
                node: "alpha-2".into(),
                xml: "<domain>db01</domain>".into(),
            })
            .unwrap();
        assert_eq!(service.stored_definitions().unwrap().len(), 2);

        service.withdraw_definition(101).await.unwrap();
        let stored = service.stored_definitions().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].vmid, 102);
        assert_eq!(
            peers.dropped_definitions(),
            vec![("alpha-2".to_string(), 101)]
        );
        // Withdrawing what is already gone is the goal state, not an error.
        service.withdraw_definition(101).await.unwrap();
    }

    // --- scale-out ----------------------------------------------------------

    fn spare_member() -> crate::validate::MemberCreate {
        crate::validate::MemberCreate {
            node: "spare-1".into(),
            core_interface: "nic1".into(),
            core_address: "10.10.0.3".into(),
            management_interface: "nic0".into(),
            management_address: "192.168.10.3".into(),
            bmc_address: "10.20.0.3".into(),
            bmc_username: "ADMIN".into(),
            bmc_password: "fence-pw".into(),
        }
    }

    fn scale_out_membership() -> EnvironmentMembership {
        let mut membership = membership_of(&[
            ("alpha-1", Some("alpha")),
            ("alpha-2", Some("alpha")),
            ("spare-1", None),
        ]);
        let (definition, networks, _) = create_request(&["alpha-1", "alpha-2"]).build().unwrap();
        membership
            .clusters
            .push(ClusterRecord::new(definition, networks));
        membership
    }

    #[tokio::test]
    async fn a_node_add_grows_the_cluster_and_flips_the_regime() {
        let backend = Arc::new(MockBackend::environment());
        // The grown cluster as corosync will see it once the newcomer is in
        // — pre-seeded, because the mock peers here deliberately do not
        // rewrite the backend (that would shrink alpha to the one prepared
        // node).
        backend.register_cluster(crate::backend::mock::formed_cluster(
            "alpha",
            &["alpha-1", "alpha-2", "spare-1"],
        ));
        let peers =
            Arc::new(MockPeers::new().with_healthy_node("spare-1", "0.3.0", &["nic0", "nic1"]));
        let service = Arc::new(
            ClusterService::new(
                backend.clone() as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir("scale-out"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_form_poll(Duration::from_millis(5))
            .with_environment(&scale_out_membership()),
        );

        let plan = service
            .prepare_add_node("alpha", spare_member())
            .await
            .unwrap();
        service.execute_add_node(plan).await.unwrap();

        // The newcomer was prepared with the *grown* configuration — no
        // two_node, three members — and every running member reloaded it.
        let prepared = peers.prepared();
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].0, "spare-1");
        assert!(!prepared[0].1.corosync_conf.contains("two_node"));
        assert!(prepared[0].1.corosync_conf.contains("spare-1"));
        let reconfigured = peers.reconfigured();
        assert_eq!(reconfigured.len(), 2, "{reconfigured:?}");
        assert!(reconfigured
            .iter()
            .all(|(_, p)| !p.corosync_conf.contains("two_node")));

        // The regime flipped everywhere it shows: delays flattened, the
        // minority-stops policy set, the newcomer's device created.
        assert!(backend
            .delay_updates()
            .iter()
            .any(|(device, delay)| device == "fence-alpha-1" && *delay == 0));
        assert!(backend
            .properties_set()
            .contains(&("no-quorum-policy".into(), "stop".into())));
        let created = backend.fence_devices_created();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0.target, "spare-1");
        assert_eq!(created[0].0.delay_base_secs, 0);

        // The record grew, last: three members, no preferred node, the
        // newcomer assigned.
        let record = service.environment_record().unwrap().unwrap();
        let alpha = record.cluster_record("alpha").unwrap();
        assert_eq!(alpha.definition.nodes.len(), 3);
        assert_eq!(alpha.definition.preferred_node, None);
        assert_eq!(
            record.node("spare-1").unwrap().cluster.as_deref(),
            Some("alpha")
        );
        let progress = service.create_progress().unwrap();
        assert_eq!(progress.phase, WorkflowPhase::Complete, "{progress:?}");
    }

    #[tokio::test]
    async fn a_failed_node_add_unwinds_and_restores_the_old_configuration() {
        let backend = Arc::new(MockBackend::environment());
        let peers = Arc::new(
            MockPeers::new()
                .with_healthy_node("spare-1", "0.3.0", &["nic0", "nic1"])
                .fail_start_on("spare-1"),
        );
        let service = Arc::new(
            ClusterService::new(
                backend.clone() as Arc<dyn ClusterBackend>,
                peers.clone(),
                network(),
                &test_dir("scale-out-unwind"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_form_poll(Duration::from_millis(5))
            .with_environment(&scale_out_membership()),
        );

        let plan = service
            .prepare_add_node("alpha", spare_member())
            .await
            .unwrap();
        let err = service.execute_add_node(plan).await.unwrap_err();
        assert!(err.to_string().contains("spare-1"), "{err}");

        // The newcomer was torn down, and the members that had taken the
        // grown configuration got the old one back — two_node included.
        let torn = peers.torn_down();
        assert_eq!(torn.len(), 1);
        assert_eq!(torn[0].0, "spare-1");
        let reconfigured = peers.reconfigured();
        assert_eq!(reconfigured.len(), 4, "grown twice, restored twice");
        assert!(reconfigured[2..]
            .iter()
            .all(|(_, p)| p.corosync_conf.contains("two_node")));

        // And the record never grew.
        let record = service.environment_record().unwrap().unwrap();
        assert_eq!(
            record
                .cluster_record("alpha")
                .unwrap()
                .definition
                .nodes
                .len(),
            2
        );
        assert!(record.node("spare-1").unwrap().cluster.is_none());
        assert_eq!(
            service.create_progress().unwrap().phase,
            WorkflowPhase::Failed
        );
    }

    #[tokio::test]
    async fn gossip_receive_reconciles_but_never_adopts_a_stranger() {
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let service = service_with(
            MockBackend::appliance(),
            MockPeers::new(),
            Some(&membership),
        );

        let mut newer = membership.clone();
        newer.version = 9;
        newer.nodes[1].cluster = Some("alpha".into());
        let merged = service.receive_membership(newer.clone()).await.unwrap();
        assert_eq!(merged.version, 9);

        let mut stranger = membership.clone();
        stranger.id = "someone-else".into();
        stranger.version = 99;
        let kept = service.receive_membership(stranger).await.unwrap();
        assert_eq!(kept.version, 9, "a stranger's record is never adopted");
    }

    /// The record's row for a node is written at join and never touched
    /// again, so a package update plus restart left every console showing
    /// the version the node joined with. Gossip now stamps the running
    /// version first — and only when it differs, because the counter makes
    /// every peer re-read the record.
    #[tokio::test]
    async fn gossip_stamps_the_running_version_into_the_record() {
        // Joined as 0.3.0 (what membership_of writes); running 0.4.0 — the
        // shape a dnf update plus restart leaves behind.
        let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
        let service = Arc::new(
            ClusterService::new(
                Arc::new(MockBackend::appliance()),
                Arc::new(MockPeers::new()),
                network(),
                &test_dir("version-stamp"),
                "0.4.0",
            )
            .with_node("alpha-1")
            .with_environment(&membership),
        );
        let before = service.environment_record().unwrap().unwrap().version;

        service.gossip_once().await;

        let record = service.environment_record().unwrap().unwrap();
        assert_eq!(
            record.node("alpha-1").unwrap().controlplane_version,
            "0.4.0"
        );
        assert_eq!(
            record.node("alpha-2").unwrap().controlplane_version,
            "0.3.0",
            "a peer's row is its own to stamp"
        );
        assert_eq!(record.version, before + 1);

        // Already true: the next pass must not spend the counter on nothing.
        service.gossip_once().await;
        assert_eq!(
            service.environment_record().unwrap().unwrap().version,
            before + 1
        );
    }

    // --- maintenance --------------------------------------------------------

    #[tokio::test]
    async fn maintenance_writes_the_record_and_reaches_pacemaker_both_ways() {
        let backend = Arc::new(MockBackend::environment());
        let service = Arc::new(
            ClusterService::new(
                backend.clone(),
                Arc::new(MockPeers::new()),
                network(),
                &test_dir("maint"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_environment(&environment_membership()),
        );

        let view = service
            .set_maintenance("alpha-1", true, "root@pam")
            .await
            .unwrap();
        assert_eq!(view.cluster, "alpha");
        assert_eq!(view.maintenance.as_ref().unwrap().by, "root@pam");
        assert!(
            view.quorum_safe,
            "the two-node regime carries one survivor on purpose"
        );
        assert_eq!(backend.standby_calls(), vec![("alpha-1".into(), true)]);
        assert!(service.maintenance_of("alpha-1").unwrap().is_some());

        // The record is what every other node's HA manager reads, so it has
        // to be in the gossiped document, not just in memory.
        let record = service.environment_record().unwrap().unwrap();
        assert!(record.node("alpha-1").unwrap().in_maintenance());
        assert!(!record.node("alpha-2").unwrap().in_maintenance());

        // And the view an operator reads shows it on the member row.
        let cluster = service.cluster("alpha").await.unwrap();
        let node = cluster.nodes.iter().find(|n| n.node == "alpha-1").unwrap();
        assert!(node.maintenance.is_some());
        assert!(node.standby, "standby is what Pacemaker was told");

        let view = service
            .set_maintenance("alpha-1", false, "root@pam")
            .await
            .unwrap();
        assert!(view.maintenance.is_none());
        assert_eq!(
            backend.standby_calls(),
            vec![("alpha-1".into(), true), ("alpha-1".into(), false)]
        );
        assert!(service.maintenance_of("alpha-1").unwrap().is_none());
    }

    /// The record must never claim a node is out of service when Pacemaker
    /// never agreed — that pair disagreeing is worse than either end state.
    #[tokio::test]
    async fn a_standby_that_fails_unwinds_the_record() {
        let backend = Arc::new(MockBackend::environment());
        let service = Arc::new(
            ClusterService::new(
                backend.clone(),
                Arc::new(MockPeers::new()),
                network(),
                &test_dir("maint-unwind"),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_environment(&environment_membership()),
        );
        let before = service.environment_record().unwrap().unwrap().version;

        backend.fail_standby("pcs is not answering");
        let err = service
            .set_maintenance("alpha-1", true, "root@pam")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not answering"), "{err}");
        assert!(
            service.maintenance_of("alpha-1").unwrap().is_none(),
            "the flag is gone again"
        );
        // The unwind bumps the counter rather than restoring it: a peer that
        // already took the flagged record has to see something newer, or it
        // would keep believing this node is out of service.
        assert_eq!(
            service.environment_record().unwrap().unwrap().version,
            before + 2
        );
    }

    #[tokio::test]
    async fn maintenance_refuses_what_it_should() {
        let service = service_with(
            MockBackend::environment(),
            MockPeers::new(),
            Some(&environment_membership()),
        );

        // Another node's maintenance is run from that node's console.
        let err = service
            .set_maintenance("alpha-2", true, "root@pam")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("alpha-2"), "{err}");
        assert!(err.to_string().contains("console"), "{err}");

        let err = service
            .set_maintenance("ghost", true, "root@pam")
            .await
            .unwrap_err();
        assert!(matches!(err, ClusterError::NotFound(_)), "{err}");

        // A cluster mid-fence is not a cluster to take another node out of.
        let service = service_with(
            MockBackend::environment().with_partition("alpha", "alpha-2"),
            MockPeers::new(),
            Some(&environment_membership()),
        );
        let err = service
            .set_maintenance("alpha-1", true, "root@pam")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("fenced"), "{err}");

        // Leaving service is guarded; returning to it is not — a node coming
        // back must never be blocked by the state it is coming back from.
        assert!(service
            .set_maintenance("alpha-1", false, "root@pam")
            .await
            .is_ok());
    }

    /// A standalone node has nowhere to drain to and nothing that would
    /// restart its machines, so the answer is a sentence, not a flag.
    #[tokio::test]
    async fn a_node_in_no_cluster_is_told_why_maintenance_is_meaningless() {
        let service = service_with(
            MockBackend::appliance(),
            MockPeers::new(),
            Some(&membership_of(&[("alpha-1", None)])),
        );
        let err = service
            .set_maintenance("alpha-1", true, "root@pam")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not in a cluster"), "{err}");
    }

    #[test]
    fn quorum_safety_counts_the_vote_that_is_about_to_go_away() {
        let three = |votes: u32| QuorumState {
            quorate: true,
            votes,
            expected_votes: 3,
            two_node: false,
            wait_for_all: false,
        };
        assert!(quorum_survives_loss(&three(3)), "3 of 3 can spare one");
        assert!(!quorum_survives_loss(&three(2)), "2 of 3 cannot");

        // The two-node regime exists so one survivor carries on.
        assert!(quorum_survives_loss(&QuorumState {
            quorate: true,
            votes: 2,
            expected_votes: 2,
            two_node: true,
            wait_for_all: true,
        }));

        assert!(quorum_survives_loss(&QuorumState {
            quorate: true,
            votes: 5,
            expected_votes: 5,
            two_node: false,
            wait_for_all: false,
        }));
    }
}
