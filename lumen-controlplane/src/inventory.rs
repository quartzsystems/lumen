//! What each node in the environment has, gathered from all of them.
//!
//! Every node-shaped response in this API is already a list grouped by node —
//! `/api/nodes`, `/api/network/interfaces`, `/api/storage/pools` — and every
//! one of those lists has exactly one entry, because each control plane can
//! only see itself. This module is the other half: one call that asks every
//! member the same three questions and returns the answers side by side.
//!
//! It is deliberately a *separate* endpoint rather than a widening of those
//! three. Those routes mean "this appliance" — the same rule `check_node`
//! states for request bodies — and the console's edit and delete actions on a
//! link or a pool are written against that meaning. Widening them would make
//! every existing write ambiguous about which node it lands on. Reading is the
//! part that wants the whole environment; writing still belongs to the node
//! that owns the thing.
//!
//! One unreachable member degrades one row. The console renders six
//! independent readings and must not blank five of them because a sixth node
//! is rebooting, so a member that could not be asked comes back carrying the
//! reason rather than being dropped from the list.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use lumen_cluster::{ClusterError, EnvironmentNode};
use lumen_net::service::LinkView;
use lumen_virt::service::NodeView;
use lumen_zfs::service::PoolView;
use lumen_zfs::BlockDevice;

use crate::AppState;

/// The one thing this module needs from the peer channel.
///
/// Narrow on purpose. The real channel is an HTTP client with TLS, tickets,
/// and a dozen verbs; what an environment-wide read wants is "ask that node
/// what it has", and a seam that small is one a test can stand in for
/// without building a socket.
#[async_trait]
pub trait InventoryPeers: Send + Sync {
    async fn fetch(&self, node: &EnvironmentNode) -> Result<NodeInventory, ClusterError>;

    /// Build a pool on one member, from disks that member owns.
    ///
    /// The cluster does not own the pool that results — it is that node's
    /// pool, listed and destroyed on its own Storage page, exactly as the
    /// bond and the bridge are that node's links. What the cluster provides
    /// is the one operation an operator would otherwise do by visiting every
    /// console in turn.
    async fn create_pool(
        &self,
        node: &EnvironmentNode,
        request: &lumen_zfs::PoolCreate,
    ) -> Result<(), ClusterError>;

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

    async fn create_pool(
        &self,
        node: &EnvironmentNode,
        _request: &lumen_zfs::PoolCreate,
    ) -> Result<(), ClusterError> {
        Err(ClusterError::Conflict(format!(
            "This control plane has no peer channel, so nothing could be built on \"{}\".",
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

/// Build the same pool on every named member.
///
/// Sequential, not concurrent, and deliberately: this reformats disks. A
/// failure part-way should stop rather than press on to the next member, and
/// the operator should be able to read the outcome as "these worked, this one
/// did not, nothing after it was attempted".
pub async fn create_cluster_pool(
    state: &Arc<AppState>,
    request: &ClusterPoolCreate,
) -> Result<ClusterPoolOutcome, ClusterError> {
    if !request.i_understand_this_erases_the_disks {
        return Err(ClusterError::Conflict(
            "Building a pool reformats every disk it is given. Acknowledge that first.".to_string(),
        ));
    }
    if request.seats.is_empty() {
        return Err(ClusterError::Conflict(
            "No member was given any disks, so there is nothing to build.".to_string(),
        ));
    }

    let nodes = state.cluster.environment_nodes()?;
    let local_name = state.cluster.node().to_string();
    let mut built = Vec::new();

    for seat in &request.seats {
        if seat.disks.is_empty() {
            return Err(ClusterError::Conflict(format!(
                "\"{}\" was given no disks. Leave a member out entirely rather than \
                 building it an empty pool.",
                seat.node
            )));
        }
        let pool = lumen_zfs::PoolCreate {
            name: request.name.clone(),
            vdev: request.vdev,
            disks: seat.disks.clone(),
            ashift: request.ashift,
            compression: request.compression,
            autotrim: true,
        };

        let outcome = if seat.node == local_name {
            // This node builds its own through the storage domain directly,
            // for the same reason the inventory read does: only the caller
            // holds the service.
            state
                .storage
                .create_pool(
                    pool,
                    lumen_zfs::Acknowledgements {
                        may_lose_data: true,
                    },
                )
                .await
                .map(|_| ())
                .map_err(|err| ClusterError::Conflict(err.to_string()))
        } else {
            let Some(member) = nodes.iter().find(|node| node.name == seat.node) else {
                return Err(ClusterError::NotFound(format!(
                    "\"{}\" is not a node in this environment.",
                    seat.node
                )));
            };
            state.peers.create_pool(member, &pool).await
        };

        if let Err(err) = outcome {
            return Err(ClusterError::Conflict(format!(
                "\"{}\" could not build the pool: {err}. {} Nothing after it was attempted.",
                seat.node,
                if built.is_empty() {
                    "No member has one yet.".to_string()
                } else {
                    format!("{} already has one.", built.join(", "))
                }
            )));
        }
        built.push(seat.node.clone());
    }

    Ok(ClusterPoolOutcome {
        name: request.name.clone(),
        built,
    })
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

/// One member's share of a cluster-wide pool build: which of its disks go in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolSeat {
    pub node: String,
    /// Disks as the picker reported them; each member resolves its own to
    /// stable `/dev/disk/by-id` paths before anything is built.
    pub disks: Vec<String>,
}

/// Build one identically named pool across several members at once.
///
/// This is the "pool the drives across the nodes" operation, and it is worth
/// being exact about what it is not: there is no cluster-wide pool. Each
/// member ends up with its own ZFS pool of the same name, on its own disks,
/// listed and destroyed on its own Storage page. What that buys is
/// replicated volumes — a volume placed on two members needs a pool of the
/// same name on both, and doing that by hand across consoles is where the
/// names drift apart.
///
/// A member whose pool cannot be built fails the call. Pools already built
/// elsewhere stay, because they are that node's and are useful on their own;
/// the error names the member so a retry can finish the job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterPoolCreate {
    /// The pool name, identical on every member — what makes a replicated
    /// volume able to name one place and mean all of them.
    pub name: String,
    #[serde(default)]
    pub vdev: lumen_zfs::VdevKind,
    #[serde(default)]
    pub compression: lumen_zfs::Compression,
    #[serde(default)]
    pub ashift: Option<u8>,
    pub seats: Vec<PoolSeat>,
    /// The operator has been told this reformats every disk listed.
    #[serde(default)]
    pub i_understand_this_erases_the_disks: bool,
}

/// What one cluster-wide pool build did, per member.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterPoolOutcome {
    pub name: String,
    pub built: Vec<String>,
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
