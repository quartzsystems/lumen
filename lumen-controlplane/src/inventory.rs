//! What each node in the environment has, gathered from all of them.
//!
//! Every node-shaped response in this API is already a list grouped by node —
//! `/api/nodes`, `/api/network/interfaces`, `/api/storage/pools` — and every
//! one of those lists has exactly one entry, because each control plane can
//! only see itself. This module is the other half: one call that asks every
//! member the same three questions and returns the answers side by side.
//!
//! It is deliberately a *separate* endpoint rather than a widening of those
//! three. Those routes mean "this appliance" unless a request *names* another
//! node — never by default — and the console's edit and delete actions on a
//! pool are written against that meaning. Reading is the part that wants the
//! whole environment; writing still belongs to the node that owns the thing,
//! and where a write may now be asked for from another member's console
//! ([`NetworkVerb`], `wipe_disk`), it still runs on the owner, behind the
//! owner's own guards.
//!
//! One unreachable member degrades one row. The console renders six
//! independent readings and must not blank five of them because a sixth node
//! is rebooting, so a member that could not be asked comes back carrying the
//! reason rather than being dropped from the list.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use lumen_cluster::{ClusterError, EnvironmentNode};
use lumen_net::service::{BondPatch, BridgePatch, LinkView, NicPatch, VlanPatch};
use lumen_virt::service::NodeView;
use lumen_zfs::service::PoolView;
use lumen_zfs::BlockDevice;

use crate::AppState;

/// One networking act, named — the closed set a console's network write can
/// ask another member to perform, the same shape as `lumen_pool::PoolVerb`
/// and for the same reason: a peer can request one of these and nothing
/// else, and every variant is deserialized whole before anything runs.
///
/// The verbs mirror the operator-facing `/api/network` routes one for one,
/// staged lifecycle included: a change staged on a member stays staged
/// *there*, its apply opens a checkpoint *there*, and a confirm that never
/// arrives — including because the change severed the path it travelled —
/// is rolled back by that member on its own. That auto-revert is what makes
/// forwarding a write no more dangerous than typing it on the member's own
/// console.
///
/// `Apply` carries the disconnect acknowledgement, unlike `wipe_disk`,
/// which deliberately hardcodes its consent. The difference: the wipe route
/// exists only for already-acknowledged requests, while `may_disconnect`
/// is a validator input that is legitimately false — carrying it is what
/// keeps the target's validator able to refuse a management-address move
/// nobody acknowledged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "verb", content = "with", rename_all = "snake_case")]
pub enum NetworkVerb {
    /// The staged delta, validation results, and any open checkpoint.
    Pending,
    Discard,
    CreateBridge(lumen_net::Bridge),
    CreateBond(lumen_net::Bond),
    CreateVlan(lumen_net::Vlan),
    UpdateBridge {
        name: String,
        patch: BridgePatch,
    },
    UpdateBond {
        name: String,
        patch: BondPatch,
    },
    UpdateVlan {
        name: String,
        patch: VlanPatch,
    },
    UpdateNic {
        name: String,
        patch: NicPatch,
    },
    DeleteLink {
        name: String,
        kind: lumen_net::LinkKind,
    },
    Apply {
        may_disconnect: bool,
    },
    Confirm,
    Rollback,
    Extend {
        seconds: u32,
    },
    ManagementBridge,
    /// The nicN pins: names that have lost their hardware, and adapters
    /// nothing has claimed.
    Pins,
    /// Give an orphaned nicN to the adapter that replaced its card.
    Adopt {
        slot: u32,
        mac: String,
    },
}

/// One act on one machine, named — the closed set a console can ask the
/// member that owns a machine to perform.
///
/// The same shape as [`NetworkVerb`] and for the same reasons, but the thing
/// it makes possible is different in kind. A network write is forwarded
/// because the operator may be sitting at the wrong console; a machine verb is
/// forwarded because **the machine is somewhere else**, and an appliance whose
/// console can only start the machines that happen to share a node with it is
/// an appliance that makes an operator learn the layout before they can use
/// it.
///
/// Every guard stays the target's. Whether a machine may start, whether a disk
/// may be detached, whether a running guest may be cut off without warning are
/// all decided by the hypervisor that has it — this carries the instruction
/// and nothing else. The acknowledgements ride along for the reason
/// `NetworkVerb::Apply` carries its own: they are validator inputs that are
/// legitimately false, and dropping them would leave the target unable to
/// refuse the thing nobody agreed to.
///
/// The console's viewer is deliberately **not** here. A VNC stream is a socket
/// on the node holding the machine, not a request/response, and relaying one
/// through this enum would mean a second console protocol. A machine on
/// another member is viewed from that member's console, which the UI says.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "verb", content = "with", rename_all = "snake_case")]
pub enum VmVerb {
    /// One machine, in full — the detail page's read.
    Get {
        vmid: u32,
    },
    /// One machine's history, newest first.
    Tasks {
        vmid: u32,
    },
    Create(Box<lumen_virt::service::VmCreate>),
    Update {
        vmid: u32,
        patch: Box<lumen_virt::service::VmPatch>,
    },
    Delete {
        vmid: u32,
        purge_disks: bool,
        may_lose_data: bool,
    },
    Start {
        vmid: u32,
    },
    Shutdown {
        vmid: u32,
    },
    Stop {
        vmid: u32,
        may_lose_data: bool,
    },
    Reboot {
        vmid: u32,
    },
    Reset {
        vmid: u32,
        may_lose_data: bool,
    },
    /// Live-migrate. Named from the *machine's* node, so a console anywhere
    /// asks the node that has it to hand it over — which is the only node that
    /// can.
    Migrate {
        vmid: u32,
        target: String,
    },
    AttachDisk {
        vmid: u32,
        disk: Box<lumen_virt::service::DiskCreate>,
    },
    DetachDisk {
        vmid: u32,
        id: String,
        purge_disks: bool,
        may_lose_data: bool,
    },
    AttachNic {
        vmid: u32,
        nic: Box<lumen_virt::service::NicCreate>,
    },
    DetachNic {
        vmid: u32,
        id: String,
    },
    AttachCdrom {
        vmid: u32,
        cdrom: Box<lumen_virt::service::CdromCreate>,
    },
    SetCdromMedia {
        vmid: u32,
        id: String,
        media: Box<lumen_virt::service::CdromCreate>,
    },
    DetachCdrom {
        vmid: u32,
        id: String,
    },
}

impl VmVerb {
    /// The machine it is about, when it is about an existing one. `None` for a
    /// create, which is the one verb with no machine to route by.
    pub fn vmid(&self) -> Option<u32> {
        match self {
            VmVerb::Create(_) => None,
            VmVerb::Get { vmid }
            | VmVerb::Tasks { vmid }
            | VmVerb::Update { vmid, .. }
            | VmVerb::Delete { vmid, .. }
            | VmVerb::Start { vmid }
            | VmVerb::Shutdown { vmid }
            | VmVerb::Stop { vmid, .. }
            | VmVerb::Reboot { vmid }
            | VmVerb::Reset { vmid, .. }
            | VmVerb::Migrate { vmid, .. }
            | VmVerb::AttachDisk { vmid, .. }
            | VmVerb::DetachDisk { vmid, .. }
            | VmVerb::AttachNic { vmid, .. }
            | VmVerb::DetachNic { vmid, .. }
            | VmVerb::AttachCdrom { vmid, .. }
            | VmVerb::SetCdromMedia { vmid, .. }
            | VmVerb::DetachCdrom { vmid, .. } => Some(*vmid),
        }
    }

    /// Whether it changes anything. The reads are forwarded on the same
    /// channel — one relay rather than two — but only the writes are worth a
    /// generous deadline.
    pub fn is_slow(&self) -> bool {
        !matches!(self, VmVerb::Get { .. } | VmVerb::Tasks { .. })
    }
}

impl NetworkVerb {
    /// Whether the verb applies a change and waits on NetworkManager —
    /// checkpoints, activations, a live rename — rather than editing a
    /// staged document. The channel gives these the generous deadline.
    pub fn is_slow(&self) -> bool {
        matches!(
            self,
            NetworkVerb::Apply { .. }
                | NetworkVerb::Rollback
                | NetworkVerb::ManagementBridge
                | NetworkVerb::Adopt { .. }
        )
    }
}

/// The one thing this module needs from the peer channel.
///
/// Narrow on purpose. The real channel is an HTTP client with TLS, tickets,
/// and a dozen verbs; what an environment-wide read wants is "ask that node
/// what it has", and a seam that small is one a test can stand in for
/// without building a socket.
#[async_trait]
pub trait InventoryPeers: Send + Sync {
    async fn fetch(&self, node: &EnvironmentNode) -> Result<NodeInventory, ClusterError>;

    /// Clear one disk on one member.
    ///
    /// The exception to this module's own rule that writing belongs to the
    /// node that owns the thing, and it earns it: the operator choosing disks
    /// for a pool is looking at every member's disks in one picker, and the
    /// ones they cannot choose are the ones carrying an old partition table.
    /// Sending them to another console to clear a disk they are already
    /// looking at is the console refusing to finish a job it started.
    ///
    /// Narrower than a pool destroy on purpose. This clears a disk nothing is
    /// using; the member's own guards decide what that means, and a disk
    /// holding a pool or a mount is refused there.
    async fn wipe_disk(
        &self,
        node: &EnvironmentNode,
        disk: &str,
    ) -> Result<BlockDevice, ClusterError>;

    /// What one member has waiting, and what it is installing.
    ///
    /// The read, which never asks that member's repositories — so an
    /// environment-wide page load costs one round trip per member and no
    /// network beyond the cluster, exactly as the node-local read does.
    async fn updates(
        &self,
        node: &EnvironmentNode,
    ) -> Result<crate::cluster_updates::NodeUpdates, ClusterError>;

    /// The same, but that member asks its repositories first.
    async fn check_updates(
        &self,
        node: &EnvironmentNode,
    ) -> Result<crate::cluster_updates::NodeUpdates, ClusterError>;

    /// Start installing on one member, and answer as soon as it is under way.
    ///
    /// Returns that member's own transaction record, whose `started_at` is how
    /// the walk tells the transaction it started from one that was already
    /// running there.
    ///
    /// `by` is the operator's principal, carried so that member's own record
    /// names the person rather than only the node that relayed the request.
    async fn apply_updates(
        &self,
        node: &EnvironmentNode,
        platform: bool,
        by: &str,
    ) -> Result<crate::updates::UpdateProgress, ClusterError>;

    /// Take one member out of service and drain it.
    ///
    /// Answers once the drain is published, not once it has finished — the
    /// machines are still moving. [`Self::drain_progress`] is how it is
    /// watched, and an empty `stranded` list at the end is what means the node
    /// is safe to restart.
    async fn enter_maintenance(
        &self,
        node: &EnvironmentNode,
        by: &str,
    ) -> Result<crate::maintenance::MaintenanceProgress, ClusterError>;

    /// One member's drain, while it has one to report.
    async fn drain_progress(
        &self,
        node: &EnvironmentNode,
    ) -> Result<Option<crate::maintenance::MaintenanceProgress>, ClusterError>;

    /// Put one member back into service.
    async fn exit_maintenance(&self, node: &EnvironmentNode, by: &str) -> Result<(), ClusterError>;

    /// Restart one member now.
    ///
    /// The member applies the quorum guard itself before it goes, so a call
    /// that succeeds is one the member agreed was safe — the coordinator's
    /// belief about the cluster carries no weight over the member's own.
    async fn restart(&self, node: &EnvironmentNode) -> Result<(), ClusterError>;

    /// One member's uptime, its own clock, and anything it is committed to.
    ///
    /// The read behind the environment's Maintenance table, and cheap enough
    /// to be one round trip per member on every page load — logind is asked
    /// what it has scheduled, and nothing leaves the node.
    async fn power_state(
        &self,
        node: &EnvironmentNode,
    ) -> Result<lumen_sys::service::PowerView, ClusterError>;

    /// Commit one member to a restart or a shutdown, now or at a moment.
    ///
    /// The member applies its own quorum guard, exactly as [`Self::restart`]
    /// does. `None` comes back for an immediate one: the node is going down
    /// and has nothing truthful to report about the state it will be in.
    async fn set_power(
        &self,
        node: &EnvironmentNode,
        action: lumen_sys::model::PowerAction,
        at: Option<u64>,
        by: &str,
    ) -> Result<Option<lumen_sys::service::PowerView>, ClusterError>;

    /// Call off what one member has scheduled.
    async fn cancel_power(
        &self,
        node: &EnvironmentNode,
    ) -> Result<lumen_sys::service::PowerView, ClusterError>;

    /// One member's most recent log entries, newest first.
    ///
    /// A window rather than the whole log: the caller is showing a page of
    /// activity across every member, and the far end of one member's history
    /// is not what any of them are looking at.
    async fn tasks(
        &self,
        node: &EnvironmentNode,
        limit: usize,
    ) -> Result<Vec<crate::tasks::TaskRecord>, ClusterError>;

    /// The machines one member has, exactly as that member serialized them.
    ///
    /// Its own answer about its own hypervisor — no member can speak for
    /// another's domains, which is why the environment-wide list is a fan-out
    /// and not a lookup in a record somebody keeps. Relayed untouched, like
    /// the verb answers below: the console's view of a machine is that
    /// member's view of it, not this node's re-telling.
    async fn vms(&self, node: &EnvironmentNode) -> Result<serde_json::Value, ClusterError>;

    /// Run one machine verb on the member that owns the machine — the
    /// compute half of the federation's proxied write, answering with exactly
    /// the JSON that member's own console route would have returned.
    ///
    /// `by` is the operator's principal, carried so that member's own log
    /// names the person rather than only the node that relayed the request.
    async fn vm_verb(
        &self,
        node: &EnvironmentNode,
        verb: &VmVerb,
        by: &str,
    ) -> Result<serde_json::Value, ClusterError>;

    /// Run one networking verb on one member — the federation's proxied
    /// write, and the answer is exactly the JSON that member's own console
    /// route would have returned, relayed untouched.
    ///
    /// The write still belongs to the node that owns the link: it lands in
    /// that member's own networking domain, behind that member's own
    /// validation, checkpoint, and confirm window. What this changes is only
    /// where the operator may be sitting when they ask.
    async fn network(
        &self,
        node: &EnvironmentNode,
        verb: &NetworkVerb,
    ) -> Result<serde_json::Value, ClusterError>;

    /// Cut or cycle `target`'s power through `via`'s fence device.
    ///
    /// The path a node takes to its own BMC: commanding it directly would
    /// kill the answer mid-flight, so the request rides another member's
    /// fence path instead. Every guard is `via`'s — it refuses a target it
    /// holds no fence device for the same way the local route does.
    async fn power(
        &self,
        via: &EnvironmentNode,
        target: &str,
        action: lumen_cluster::HardPower,
    ) -> Result<(), ClusterError> {
        let _ = (target, action);
        Err(ClusterError::Conflict(format!(
            "There is no way to reach \"{}\" from here.",
            via.name
        )))
    }
}

/// A control plane with no peer channel behind it: it can answer for itself
/// and says so plainly about everyone else.
///
/// This is what the test harnesses hold, and it is not a stub — "there is no
/// way to reach that node from here" is a true answer, and the environment
/// view renders it the same way it renders a member that is switched off.
pub struct NoPeers;

#[async_trait]
impl InventoryPeers for NoPeers {
    async fn fetch(&self, node: &EnvironmentNode) -> Result<NodeInventory, ClusterError> {
        Err(ClusterError::Conflict(format!(
            "This control plane has no peer channel, so \"{}\" could not be asked.",
            node.name
        )))
    }

    async fn wipe_disk(
        &self,
        node: &EnvironmentNode,
        _disk: &str,
    ) -> Result<BlockDevice, ClusterError> {
        Err(ClusterError::Conflict(format!(
            "This control plane has no peer channel, so nothing on \"{}\" could be cleared.",
            node.name
        )))
    }

    async fn updates(
        &self,
        node: &EnvironmentNode,
    ) -> Result<crate::cluster_updates::NodeUpdates, ClusterError> {
        Err(ClusterError::Conflict(format!(
            "This control plane has no peer channel, so \"{}\" could not be asked what it has \
             waiting.",
            node.name
        )))
    }

    async fn check_updates(
        &self,
        node: &EnvironmentNode,
    ) -> Result<crate::cluster_updates::NodeUpdates, ClusterError> {
        self.updates(node).await
    }

    async fn apply_updates(
        &self,
        node: &EnvironmentNode,
        _platform: bool,
        _by: &str,
    ) -> Result<crate::updates::UpdateProgress, ClusterError> {
        Err(ClusterError::Conflict(format!(
            "This control plane has no peer channel, so nothing could be installed on \"{}\".",
            node.name
        )))
    }

    async fn enter_maintenance(
        &self,
        node: &EnvironmentNode,
        _by: &str,
    ) -> Result<crate::maintenance::MaintenanceProgress, ClusterError> {
        Err(no_channel(node, "taken out of service"))
    }

    async fn drain_progress(
        &self,
        node: &EnvironmentNode,
    ) -> Result<Option<crate::maintenance::MaintenanceProgress>, ClusterError> {
        Err(no_channel(node, "asked about its drain"))
    }

    async fn exit_maintenance(
        &self,
        node: &EnvironmentNode,
        _by: &str,
    ) -> Result<(), ClusterError> {
        Err(no_channel(node, "put back into service"))
    }

    async fn restart(&self, node: &EnvironmentNode) -> Result<(), ClusterError> {
        Err(no_channel(node, "restarted"))
    }

    async fn power_state(
        &self,
        node: &EnvironmentNode,
    ) -> Result<lumen_sys::service::PowerView, ClusterError> {
        Err(no_channel(node, "asked about its power"))
    }

    async fn set_power(
        &self,
        node: &EnvironmentNode,
        _action: lumen_sys::model::PowerAction,
        _at: Option<u64>,
        _by: &str,
    ) -> Result<Option<lumen_sys::service::PowerView>, ClusterError> {
        Err(no_channel(node, "restarted or shut down"))
    }

    async fn cancel_power(
        &self,
        node: &EnvironmentNode,
    ) -> Result<lumen_sys::service::PowerView, ClusterError> {
        Err(no_channel(node, "told to call it off"))
    }

    async fn tasks(
        &self,
        node: &EnvironmentNode,
        _limit: usize,
    ) -> Result<Vec<crate::tasks::TaskRecord>, ClusterError> {
        Err(no_channel(node, "asked for its log"))
    }

    async fn vms(&self, node: &EnvironmentNode) -> Result<serde_json::Value, ClusterError> {
        Err(no_channel(node, "asked for its machines"))
    }

    async fn vm_verb(
        &self,
        node: &EnvironmentNode,
        _verb: &VmVerb,
        _by: &str,
    ) -> Result<serde_json::Value, ClusterError> {
        Err(no_channel(node, "asked to act on one of its machines"))
    }

    async fn network(
        &self,
        node: &EnvironmentNode,
        _verb: &NetworkVerb,
    ) -> Result<serde_json::Value, ClusterError> {
        Err(no_channel(node, "asked to change its network"))
    }
}

fn no_channel(node: &EnvironmentNode, what: &str) -> ClusterError {
    ClusterError::Conflict(format!(
        "This control plane has no peer channel, so \"{}\" could not be {what}.",
        node.name
    ))
}

/// One node's answer: what it is, what it is plugged into, and what it can
/// store. The three shapes the console already knows, from one round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInventory {
    pub node: String,
    /// Absent when the hypervisor could not be asked — a node whose libvirt
    /// is down still has links and pools worth showing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<NodeView>,
    #[serde(default)]
    pub interfaces: Vec<LinkView>,
    #[serde(default)]
    pub pools: Vec<PoolView>,
    /// The disks this node has, pool candidates and claimed ones alike.
    ///
    /// Carried with the rest because the question "which drives across these
    /// nodes should be pooled together?" cannot be asked one node at a time —
    /// the operator is choosing across the cluster, and a picker that made a
    /// round trip per node would show them the members at different moments.
    #[serde(default)]
    pub devices: Vec<BlockDevice>,
    /// This node's own corosync links, one per ring.
    ///
    /// Here for the same reason the disks are: the question is asked across
    /// the environment and cannot be answered one node at a time.
    /// `corosync-cfgtool -s` speaks only for the node it runs on, so a
    /// cluster view assembled from one member knows one member's link health
    /// — which is what makes a healthy two-node cluster read as "Connected ·
    /// 1 unknown". Gathering every member's answer at one moment is what
    /// resolves the unknowns.
    #[serde(default)]
    pub rings: Vec<lumen_cluster::RingLink>,
}

/// One member as the environment view carries it: its inventory, or why it
/// has none.
///
/// `reachable: false` with an `error` is a node that is in the environment
/// and could not be asked. That is a different fact from a node with no
/// pools, and the console must be able to tell them apart — one is a warning
/// on a row, the other is an empty cell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberInventory {
    pub node: String,
    /// Whether this is the node serving the request, which is the only one
    /// whose links and pools the console may offer to edit.
    pub local: bool,
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory: Option<NodeInventory>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryResponse {
    pub members: Vec<MemberInventory>,
}

/// Clear one disk on one member of the environment.
///
/// Routed here rather than left to `/api/storage/devices/{disk}/wipe` because
/// the operator is acting from a table that spans every member, and the node
/// is part of what they picked. The local node still goes through its own
/// storage domain — the same short-circuit the inventory read uses, for the
/// same reason: only this process holds that service.
///
/// The acknowledgement is checked once, here, and the member is then told to
/// clear the disk. It deliberately does not travel: consent was given to the
/// console the operator is looking at, and a peer route that accepted "yes,
/// erase it" from a body would be a second, quieter way to clear a node's
/// disks.
pub async fn wipe_node_disk(
    state: &Arc<AppState>,
    node: &str,
    disk: &str,
    acknowledged: bool,
) -> Result<BlockDevice, ClusterError> {
    if !acknowledged {
        return Err(ClusterError::Conflict(format!(
            "Clearing \"{disk}\" removes its partition table and every signature on it. \
             Whatever was on that disk becomes unreachable, and there is no undo. Acknowledge \
             that first."
        )));
    }

    if node == state.cluster.node() {
        return state
            .storage
            .wipe_disk(
                disk,
                lumen_zfs::Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .map_err(|err| ClusterError::Conflict(err.to_string()));
    }

    let nodes = state.cluster.environment_nodes()?;
    let Some(member) = nodes.iter().find(|candidate| candidate.name == node) else {
        return Err(ClusterError::NotFound(format!(
            "\"{node}\" is not a node in this environment."
        )));
    };
    state.peers.wipe_disk(member, disk).await
}

/// Every member's inventory, asked for concurrently.
///
/// Concurrently because the wall clock an operator waits on should be the
/// slowest member, not the sum of all of them, and a member that has gone
/// away costs the full call deadline before it says so.
///
/// A node with no environment yet still answers — with itself, alone. The
/// console renders the same table either way.
pub async fn environment(state: &Arc<AppState>) -> InventoryResponse {
    let local_name = state.cluster.node().to_string();
    let nodes = state.cluster.environment_nodes().unwrap_or_default();

    if nodes.is_empty() {
        return InventoryResponse {
            members: vec![MemberInventory {
                node: local_name,
                local: true,
                reachable: true,
                error: None,
                inventory: Some(local(state).await),
            }],
        };
    }

    let calls = nodes.iter().map(|node| {
        let state = state.clone();
        let local_name = local_name.clone();
        async move {
            // This node answers from its own services; only a peer is worth a
            // socket. The peer channel short-circuits nothing here because
            // only the caller holds the state the answer is built from.
            if node.name == local_name {
                return MemberInventory {
                    node: node.name.clone(),
                    local: true,
                    reachable: true,
                    error: None,
                    inventory: Some(local(&state).await),
                };
            }
            match state.peers.fetch(node).await {
                Ok(inventory) => MemberInventory {
                    node: node.name.clone(),
                    local: false,
                    reachable: true,
                    error: None,
                    inventory: Some(inventory),
                },
                Err(err) => MemberInventory {
                    node: node.name.clone(),
                    local: false,
                    reachable: false,
                    error: Some(err.to_string()),
                    inventory: None,
                },
            }
        }
    });

    InventoryResponse {
        members: futures_util::future::join_all(calls).await,
    }
}

/// This node's own inventory, assembled from the three domains that own the
/// three answers.
///
/// Each read is allowed to fail on its own. A node with a wedged libvirt
/// still reports its links, and a node whose pools cannot be listed still
/// reports its processors — the alternative is one broken subsystem hiding
/// everything else the operator came to look at.
pub async fn local(state: &Arc<AppState>) -> NodeInventory {
    let node = state.cluster.node().to_string();

    let capacity = match state.virt.nodes().await {
        Ok(response) => response.nodes.into_iter().next(),
        Err(_) => None,
    };
    let interfaces = match state.network.interfaces().await {
        Ok(response) => response
            .nodes
            .into_iter()
            .flat_map(|entry| entry.interfaces)
            .collect(),
        Err(_) => Vec::new(),
    };
    let pools = match state.storage.pools().await {
        Ok(response) => response
            .nodes
            .into_iter()
            .flat_map(|entry| entry.pools)
            .collect(),
        Err(_) => Vec::new(),
    };
    let devices = match state.storage.block_devices().await {
        Ok(response) => response.devices,
        Err(_) => Vec::new(),
    };

    // Never fails: a node that is not in a cluster, or whose stack cannot be
    // asked, has no rings, and an empty list is the honest answer.
    let rings = state.cluster.local_rings().await;

    NodeInventory {
        node,
        capacity,
        interfaces,
        pools,
        devices,
        rings,
    }
}
