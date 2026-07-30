//! The replicated-storage domain's one entry point.
//!
//! The control plane's handlers deserialize, call one method here, and
//! serialize the answer. The reads present every cluster's volumes; the
//! writes are the volume workflows — create, destroy, grow — each of which
//! touches every member through the [`VolumePeers`] channel and commits the
//! record through `lumen-cluster` **last**, the same finishes-or-never-
//! happened rule as a cluster create.
//!
//! Composition, not knowledge: the backing zvols go through `lumen-zfs`, the
//! record and the replication policy come from `lumen-cluster`, and this
//! service is the only place the three meet.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use lumen_cluster::topology::ClusterTopology;
use lumen_cluster::{
    Acknowledgements, ClusterRecord, ClusterService, EnvironmentMembership, EnvironmentNode,
    Regime, ValidationCode, ValidationError, VolumeRecord, VolumeSeat,
};
use lumen_zfs::StorageService;

use crate::backend::DrbdBackend;
use crate::error::{DrbdError, Result};
use crate::model::{
    allocate_minor, allocate_port, backing_size, device_path, resource_name, valid_volume_name,
    zvol_path, MAX_REPLICAS, MIN_REPLICAS, VOLBLOCKSIZE,
};
use crate::peers::{
    VolumePeers, VolumePrepare, VolumeResizeBacking, VolumeSnapshot, VolumeTeardown,
};
use crate::render::{generate_secret, resource_file, RenderSeat};
use crate::state::ResourceStatus;

/// GET /api/storage/replicated. Grouped by cluster — the repo's grouped-by-
/// node shape extended the same way the environment answer is: a volume is
/// cluster-scoped, so the cluster is the group.
#[derive(Debug, Clone, Serialize)]
pub struct ReplicatedVolumesResponse {
    pub clusters: Vec<ClusterVolumes>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClusterVolumes {
    pub cluster: String,
    pub volumes: Vec<VolumeView>,
}

/// The replication pill, derived here so the console and the API can never
/// disagree about what a volume's state reads as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeHealth {
    /// Every replica current, every connection up.
    UpToDate,
    /// A resync is in flight; `sync_percent` and `sync_rate_kib_s` ride
    /// along.
    Syncing,
    /// Serving, but a peer is disconnected or behind — the volume runs on
    /// fewer replicas than it was built with.
    Degraded,
    /// The local replica is outdated: readable history, not current data.
    Outdated,
    /// I/O suspended because the volume lost its own quorum — the
    /// three-replica integrity stop.
    SuspendedNoQuorum,
    /// I/O suspended by the fencing handler while a lost peer is outdated
    /// through Pacemaker — the two-node integrity stop.
    Suspended,
    /// A connection is StandAlone: replication has stopped on purpose and
    /// will not resume by itself.
    StandAlone,
    /// The record knows the volume; DRBD on this node does not have it up.
    Down,
    /// This node holds no replica, so it cannot see the resource — the state
    /// is known on the volume's members.
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeView {
    pub name: String,
    pub cluster: String,
    /// The DRBD resource name, `<cluster>-<name>`.
    pub resource: String,
    /// The stable block device, identical on every member.
    pub device: String,
    pub size_bytes: u64,
    pub zvol_bytes: u64,
    pub port: u16,
    pub minor: u32,
    pub health: VolumeHealth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_rate_kib_s: Option<u64>,
    pub replicas: Vec<ReplicaView>,
    /// Why the state could not be read, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplicaView {
    pub node: String,
    pub pool: String,
    /// This is the node answering the request.
    pub local: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
}

/// POST /api/storage/replicated, exactly as the console sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeCreate {
    pub cluster: String,
    pub name: String,
    pub size_bytes: u64,
    /// The replica seats. Empty means "place it for me": the least-utilized
    /// members, each on the pool its existing replicas already use.
    #[serde(default)]
    pub members: Vec<VolumeMemberCreate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMemberCreate {
    pub node: String,
    pub pool: String,
}

pub struct DrbdService {
    backend: Arc<dyn DrbdBackend>,
    peers: Arc<dyn VolumePeers>,
    cluster: Arc<ClusterService>,
    storage: Arc<StorageService>,
    /// Resync counters from the previous read, for deriving a rate: resource
    /// name → (when, KiB moved).
    rates: Mutex<HashMap<String, (Instant, u64)>>,
    /// Serializes writes: two operators creating volumes at once must not
    /// race to one port.
    gate: Mutex<()>,
}

impl DrbdService {
    pub fn new(
        backend: Arc<dyn DrbdBackend>,
        peers: Arc<dyn VolumePeers>,
        cluster: Arc<ClusterService>,
        storage: Arc<StorageService>,
    ) -> Self {
        DrbdService {
            backend,
            peers,
            cluster,
            storage,
            rates: Mutex::new(HashMap::new()),
            gate: Mutex::new(()),
        }
    }

    fn node(&self) -> &str {
        self.cluster.node()
    }

    fn membership(&self) -> Result<EnvironmentMembership> {
        self.cluster
            .environment_record()
            .map_err(DrbdError::from)?
            .ok_or_else(|| {
                DrbdError::Conflict(
                    "This node has not joined an environment, and replicated volumes exist only \
                     inside one."
                        .to_string(),
                )
            })
    }

    // --- reads --------------------------------------------------------------

    /// Every cluster's replicated volumes, definitions joined with whatever
    /// this node's DRBD can see of them.
    pub async fn volumes(&self) -> Result<ReplicatedVolumesResponse> {
        let Some(membership) = self.cluster.environment_record().map_err(DrbdError::from)? else {
            return Ok(ReplicatedVolumesResponse {
                clusters: Vec::new(),
            });
        };

        // A DRBD that cannot be asked must not blank the page: the volumes
        // are still listed from the record, each carrying the reason.
        let (statuses, status_error) = match self.backend.status().await {
            Ok(statuses) => (statuses, None),
            Err(err) => (Vec::new(), Some(err.to_string())),
        };

        let mut clusters = Vec::new();
        for name in membership.cluster_names() {
            let Some(record) = membership.cluster_record(&name) else {
                continue;
            };
            if record.volumes.is_empty() {
                continue;
            }
            let mut volumes = Vec::new();
            for volume in &record.volumes {
                volumes.push(
                    self.view_of(&name, volume, &statuses, status_error.as_deref())
                        .await,
                );
            }
            clusters.push(ClusterVolumes {
                cluster: name,
                volumes,
            });
        }
        Ok(ReplicatedVolumesResponse { clusters })
    }

    async fn view_of(
        &self,
        cluster: &str,
        volume: &VolumeRecord,
        statuses: &[ResourceStatus],
        status_error: Option<&str>,
    ) -> VolumeView {
        let resource = resource_name(cluster, &volume.name);
        let local_replica = volume.seat(self.node()).is_some();
        let status = statuses.iter().find(|s| s.name == resource);

        let (health, sync_percent, sync_rate, error) = match (status, local_replica, status_error) {
            (_, _, Some(reason)) => (VolumeHealth::Unknown, None, None, Some(reason.to_string())),
            (None, false, None) => (
                VolumeHealth::Unknown,
                None,
                None,
                Some(format!(
                    "This node holds no replica of \"{}\" — its state is known on {}.",
                    volume.name,
                    volume
                        .replicas
                        .iter()
                        .map(|s| s.node.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            ),
            (None, true, None) => (VolumeHealth::Down, None, None, None),
            (Some(status), _, None) => {
                let (health, percent) = derive_health(status);
                let rate = if health == VolumeHealth::Syncing {
                    self.derive_rate(&resource, status).await
                } else {
                    self.rates.lock().await.remove(&resource);
                    None
                };
                (health, percent, rate, None)
            }
        };

        let replicas = volume
            .replicas
            .iter()
            .map(|seat| replica_view_of(seat, self.node(), status))
            .collect();

        VolumeView {
            name: volume.name.clone(),
            cluster: cluster.to_string(),
            resource,
            device: device_path(volume.minor),
            size_bytes: volume.size_bytes,
            zvol_bytes: volume.zvol_bytes,
            port: volume.port,
            minor: volume.minor,
            health,
            sync_percent,
            sync_rate_kib_s: sync_rate,
            replicas,
            error,
        }
    }

    /// KiB/s from the delta of the resync counters between two reads. The
    /// first read of a sync answers `None`; the console's next poll has a
    /// number.
    async fn derive_rate(&self, resource: &str, status: &ResourceStatus) -> Option<u64> {
        let moved: u64 = status
            .connections
            .iter()
            .filter_map(|p| p.device())
            .map(|d| d.received + d.sent)
            .sum();
        let now = Instant::now();
        let mut rates = self.rates.lock().await;
        let previous = rates.insert(resource.to_string(), (now, moved));
        let (then, before) = previous?;
        let secs = now.duration_since(then).as_secs_f64();
        if secs < 0.05 || moved <= before {
            return None;
        }
        Some(((moved - before) as f64 / secs) as u64)
    }

    // --- workflows ----------------------------------------------------------

    /// Create a replicated volume: validate everything, size the backing
    /// devices byte-exactly, prepare every member whole, skip the initial
    /// sync, and record it — last. Any failure before the record unwinds the
    /// touched members, and the record never knew the volume existed.
    pub async fn create_volume(&self, request: VolumeCreate) -> Result<VolumeView> {
        let _guard = self.gate.lock().await;
        let membership = self.membership()?;
        let Some(record) = membership.cluster_record(&request.cluster) else {
            return Err(DrbdError::NotFound(format!(
                "There is no cluster called \"{}\" in this environment.",
                request.cluster
            )));
        };

        let members = self.placed_members(&request, record)?;
        let volume = self.validated_record(&request, record, &members)?;
        let resource = resource_name(&request.cluster, &volume.name);

        // Rendering inputs: each seat's Core address from the cluster's
        // network definition, and the regime's replication policy from the
        // one topology engine.
        let mut seats = Vec::new();
        for seat in &volume.replicas {
            let Some(core) = record
                .networks
                .core
                .members
                .iter()
                .find(|m| m.node == seat.node)
            else {
                return Err(DrbdError::Conflict(format!(
                    "\"{}\" has no seat on the cluster's Core network, and replication rides \
                     nothing else.",
                    seat.node
                )));
            };
            seats.push(RenderSeat {
                node: seat.node.clone(),
                pool: seat.pool.clone(),
                core_address: core.address,
            });
        }
        let policy = ClusterTopology::new(record.definition.clone())
            .replication_policy(volume.replicas.len());
        let rendered = resource_file(
            &resource,
            &request.cluster,
            &volume,
            &seats,
            &policy,
            &generate_secret(),
        );

        let nodes: Vec<EnvironmentNode> = volume
            .replicas
            .iter()
            .filter_map(|seat| membership.node(&seat.node).cloned())
            .collect();
        if nodes.len() != volume.replicas.len() {
            return Err(DrbdError::Conflict(
                "Not every member is in the environment record.".to_string(),
            ));
        }

        // Prepare each member whole; on failure, unwind exactly the touched
        // ones, in reverse — best effort per node, the cluster-create rule.
        let mut touched = 0usize;
        let mut failure: Option<DrbdError> = None;
        for (node, seat) in nodes.iter().zip(&volume.replicas) {
            let payload = VolumePrepare {
                cluster: request.cluster.clone(),
                resource: resource.clone(),
                zvol: zvol_path(&seat.pool, &request.cluster, &volume.name),
                zvol_bytes: volume.zvol_bytes,
                volblocksize: VOLBLOCKSIZE,
                minor: volume.minor,
                peers: volume.replicas.len() - 1,
                resource_file: rendered.clone(),
            };
            if let Err(err) = self.peers.prepare(node, &payload).await {
                failure = Some(DrbdError::Conflict(format!(
                    "Preparing {} failed: {err}",
                    node.name
                )));
                break;
            }
            touched += 1;
        }

        if failure.is_none() {
            // Both zvols are fresh zeros: declare them in sync instead of
            // copying the zeros across the Core network.
            if let Err(err) = self.peers.prime(&nodes[0], &resource).await {
                failure = Some(DrbdError::Conflict(format!(
                    "The volume came up but could not be declared in sync: {err}"
                )));
            }
        }

        if let Some(err) = failure {
            for (node, seat) in nodes.iter().zip(&volume.replicas).take(touched).rev() {
                let payload = VolumeTeardown {
                    resource: resource.clone(),
                    zvol: zvol_path(&seat.pool, &request.cluster, &volume.name),
                };
                if let Err(unwind_err) = self.peers.teardown(node, &payload).await {
                    tracing::warn!(
                        node = %node.name,
                        "the unwind could not clean up: {unwind_err}"
                    );
                }
            }
            return Err(err);
        }

        // Record, last.
        self.cluster
            .commit_volume(&request.cluster, volume.clone())
            .await?;
        tracing::info!(cluster = %request.cluster, volume = %volume.name, "replicated volume created");

        let statuses = self.backend.status().await.unwrap_or_default();
        Ok(self
            .view_of(&request.cluster, &volume, &statuses, None)
            .await)
    }

    /// Destroy a replicated volume: every replica torn down — resource down,
    /// file removed, zvol destroyed — then the record forgets it. Refused
    /// unless every member cleans up, the destroy-a-cluster rule.
    pub async fn destroy_volume(
        &self,
        cluster: &str,
        name: &str,
        ack: Acknowledgements,
    ) -> Result<()> {
        let _guard = self.gate.lock().await;
        if !ack.may_lose_data {
            return Err(DrbdError::invalid(ValidationError::new(
                ValidationCode::UnacknowledgedDestructiveOperation,
                Some("i_understand_this_may_lose_data"),
                "Destroying a replicated volume destroys every replica of its data. \
                 Acknowledge that to proceed.",
            )));
        }
        let membership = self.membership()?;
        let volume = self.recorded_volume(&membership, cluster, name)?.clone();
        let resource = resource_name(cluster, name);

        let mut failures = Vec::new();
        for seat in &volume.replicas {
            let Some(node) = membership.node(&seat.node).cloned() else {
                failures.push(format!("{} is not in the environment record", seat.node));
                continue;
            };
            let payload = VolumeTeardown {
                resource: resource.clone(),
                zvol: zvol_path(&seat.pool, cluster, name),
            };
            if let Err(err) = self.peers.teardown(&node, &payload).await {
                failures.push(format!("{}: {err}", node.name));
            }
        }
        if !failures.is_empty() {
            return Err(DrbdError::Conflict(format!(
                "Not every replica could be cleaned up, so the volume is still recorded. {}",
                failures.join(" ")
            )));
        }

        self.cluster.forget_volume(cluster, name).await?;
        tracing::info!(cluster, volume = name, "replicated volume destroyed");
        Ok(())
    }

    /// Grow a replicated volume: every backing zvol first, then the resource
    /// once, then the record. Grow only — the refusal to shrink lives at
    /// every layer down to `zfs set volsize`.
    pub async fn resize_volume(&self, cluster: &str, name: &str, size_bytes: u64) -> Result<()> {
        let _guard = self.gate.lock().await;
        let membership = self.membership()?;
        let mut volume = self.recorded_volume(&membership, cluster, name)?.clone();
        if size_bytes <= volume.size_bytes {
            return Err(DrbdError::Conflict(format!(
                "A replicated volume only grows: \"{name}\" is already {} bytes.",
                volume.size_bytes
            )));
        }
        let resource = resource_name(cluster, name);
        let zvol_bytes = backing_size(size_bytes, volume.replicas.len());
        if zvol_bytes <= volume.zvol_bytes {
            return Err(DrbdError::Conflict(
                "Grow by at least one block — that growth rounds to the size the volume \
                 already has."
                    .to_string(),
            ));
        }

        let nodes: Vec<(EnvironmentNode, &VolumeSeat)> = volume
            .replicas
            .iter()
            .filter_map(|seat| membership.node(&seat.node).cloned().map(|n| (n, seat)))
            .collect();
        if nodes.len() != volume.replicas.len() {
            return Err(DrbdError::Conflict(
                "Not every member is in the environment record.".to_string(),
            ));
        }

        // Every backing device grows before the resource does. A failure
        // partway leaves some zvols larger than the resource uses — spare
        // room, not damage — and the record unchanged, so the operator
        // simply retries.
        for (node, seat) in &nodes {
            let payload = VolumeResizeBacking {
                resource: resource.clone(),
                zvol: zvol_path(&seat.pool, cluster, name),
                zvol_bytes,
            };
            if let Err(err) = self.peers.resize_backing(node, &payload).await {
                return Err(DrbdError::Conflict(format!(
                    "Growing the backing device on {} failed: {err} — nothing was resized at \
                     the volume level, so the volume is unchanged.",
                    node.name
                )));
            }
        }
        self.peers.grow(&nodes[0].0, &resource).await?;

        volume.size_bytes = size_bytes;
        volume.zvol_bytes = zvol_bytes;
        self.cluster.commit_volume(cluster, volume).await?;
        tracing::info!(
            cluster,
            volume = name,
            size_bytes,
            "replicated volume grown"
        );
        Ok(())
    }

    // --- peer-side operations -----------------------------------------------

    /// Carry a replica: the backing zvol, the resource file, the metadata,
    /// and up — whole or not at all, so the coordinator's unwind only ever
    /// sees whole members.
    pub async fn peer_prepare(&self, payload: &VolumePrepare) -> Result<()> {
        let (pool, name) = split_zvol(&payload.zvol)?;
        self.storage
            .create_volume(pool, name, payload.zvol_bytes, Some(payload.volblocksize))
            .await?;
        self.backend
            .write_resource(&payload.resource, &payload.resource_file)
            .await?;
        self.backend
            .create_metadata(&payload.resource, payload.peers)
            .await?;
        self.backend.up(&payload.resource).await
    }

    pub async fn peer_prime(&self, resource: &str) -> Result<()> {
        self.backend.skip_initial_sync(resource).await
    }

    /// Put the member back exactly as it was: resource down, file gone, zvol
    /// destroyed. Best-effort all the way through — one stuck step must not
    /// leave the others undone — with the first failure reported.
    pub async fn peer_teardown(&self, payload: &VolumeTeardown) -> Result<()> {
        let mut first_error: Option<DrbdError> = None;
        let mut note = |result: Result<()>| {
            if let Err(err) = result {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        };
        note(self.backend.down(&payload.resource).await);
        note(self.backend.remove_resource_file(&payload.resource).await);
        note(
            self.storage
                .destroy_volume(&payload.zvol)
                .await
                .map_err(DrbdError::from),
        );
        match first_error {
            None => Ok(()),
            Some(err) => Err(err),
        }
    }

    pub async fn peer_resize_backing(&self, payload: &VolumeResizeBacking) -> Result<()> {
        self.storage
            .resize_volume(&payload.zvol, payload.zvol_bytes)
            .await
            .map_err(DrbdError::from)
    }

    pub async fn peer_grow(&self, resource: &str) -> Result<()> {
        self.backend.resize(resource).await
    }

    pub async fn peer_two_primaries(&self, resource: &str, allow: bool) -> Result<()> {
        self.backend.set_two_primaries(resource, allow).await
    }

    pub async fn peer_snapshot_backing(&self, payload: &VolumeSnapshot) -> Result<()> {
        self.storage
            .snapshot_volume(&payload.zvol, &payload.snapshot)
            .await
            .map_err(DrbdError::from)
    }

    pub async fn peer_rollback_backing(&self, payload: &VolumeSnapshot) -> Result<()> {
        self.storage
            .rollback_volume(&payload.zvol, &payload.snapshot)
            .await
            .map_err(DrbdError::from)
    }

    pub async fn peer_drop_snapshot(&self, payload: &VolumeSnapshot) -> Result<()> {
        self.storage
            .destroy_snapshot(&payload.zvol, &payload.snapshot)
            .await
            .map_err(DrbdError::from)
    }

    pub async fn peer_down(&self, resource: &str) -> Result<()> {
        self.backend.down(resource).await
    }

    pub async fn peer_up(&self, resource: &str) -> Result<()> {
        self.backend.up(resource).await
    }

    pub async fn peer_invalidate_remote(&self, resource: &str) -> Result<()> {
        self.backend.invalidate_remote(resource).await
    }

    pub async fn peer_reconnect(&self, resource: &str, discard: bool) -> Result<()> {
        self.backend.reconnect(resource, discard).await
    }

    // --- snapshots and recovery ---------------------------------------------

    /// Snapshot a volume on every replica. Each member's snapshot is
    /// crash-consistent at its own instant — with protocol C those instants
    /// are a heartbeat apart — and a rollback later picks *one* of them and
    /// resyncs the rest from it, which is why per-member snapshots are the
    /// right shape and a coordinated freeze would be machinery without a
    /// customer.
    pub async fn snapshot_volume(&self, cluster: &str, name: &str, snapshot: &str) -> Result<()> {
        let _guard = self.gate.lock().await;
        if !valid_volume_name(snapshot) {
            return Err(DrbdError::invalid(ValidationError::new(
                ValidationCode::InvalidVolumeName,
                Some("snapshot"),
                format!(
                    "\"{snapshot}\" is not a usable snapshot name — lowercase letters, digits, \
                     and hyphens, starting with a letter."
                ),
            )));
        }
        let membership = self.membership()?;
        let volume = self.recorded_volume(&membership, cluster, name)?.clone();

        let mut taken: Vec<(EnvironmentNode, VolumeSnapshot)> = Vec::new();
        for seat in &volume.replicas {
            let Some(node) = membership.node(&seat.node).cloned() else {
                continue;
            };
            let payload = VolumeSnapshot {
                zvol: zvol_path(&seat.pool, cluster, name),
                snapshot: snapshot.to_string(),
            };
            if let Err(err) = self.peers.snapshot_backing(&node, &payload).await {
                // A snapshot on some replicas and not others is a trap for
                // the rollback that would use it: unwind what was taken.
                for (node, payload) in taken.iter().rev() {
                    if let Err(unwind_err) = self.peers.drop_snapshot(node, payload).await {
                        tracing::warn!(%node.name, "the snapshot unwind failed: {unwind_err}");
                    }
                }
                return Err(DrbdError::Conflict(format!(
                    "The snapshot on {} failed, so it was not taken anywhere: {err}",
                    node.name
                )));
            }
            taken.push((node, payload));
        }
        tracing::info!(cluster, volume = name, snapshot, "volume snapshotted");
        Ok(())
    }

    /// The snapshots of this node's own replica. Honest about vantage, like
    /// every read here: a node without a replica is told where to ask.
    pub async fn volume_snapshots(
        &self,
        cluster: &str,
        name: &str,
    ) -> Result<Vec<lumen_zfs::SnapshotInfo>> {
        let membership = self.membership()?;
        let volume = self.recorded_volume(&membership, cluster, name)?;
        let Some(seat) = volume.seat(self.node()) else {
            return Err(DrbdError::Conflict(format!(
                "This node holds no replica of \"{name}\" — its snapshots are known on {}.",
                volume
                    .replicas
                    .iter()
                    .map(|s| s.node.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        self.storage
            .volume_snapshots(&zvol_path(&seat.pool, cluster, name))
            .await
            .map_err(DrbdError::from)
    }

    /// Drop a snapshot from every replica. All must let go, or the answer
    /// says who did not.
    pub async fn delete_snapshot(&self, cluster: &str, name: &str, snapshot: &str) -> Result<()> {
        let _guard = self.gate.lock().await;
        let membership = self.membership()?;
        let volume = self.recorded_volume(&membership, cluster, name)?.clone();
        let mut failures = Vec::new();
        for seat in &volume.replicas {
            let Some(node) = membership.node(&seat.node).cloned() else {
                continue;
            };
            let payload = VolumeSnapshot {
                zvol: zvol_path(&seat.pool, cluster, name),
                snapshot: snapshot.to_string(),
            };
            if let Err(err) = self.peers.drop_snapshot(&node, &payload).await {
                failures.push(format!("{}: {err}", node.name));
            }
        }
        if !failures.is_empty() {
            return Err(DrbdError::Conflict(format!(
                "Not every replica dropped the snapshot. {}",
                failures.join(" ")
            )));
        }
        Ok(())
    }

    /// Roll a volume back to a snapshot: one transactional workflow, never
    /// raw primitives. The machine using the volume must be off — DRBD's
    /// role says whether anything holds the device open — then: resource
    /// down on every member, `zfs rollback` on the chosen source, resource
    /// up everywhere, and the source declared the one truth
    /// (`invalidate-remote`), so the peers resync from it. The other
    /// members' own snapshots are left in place; the resync overwrites their
    /// live data either way.
    pub async fn rollback_volume(
        &self,
        cluster: &str,
        name: &str,
        snapshot: &str,
        source: &str,
        ack: Acknowledgements,
    ) -> Result<()> {
        let _guard = self.gate.lock().await;
        if !ack.may_lose_data {
            return Err(DrbdError::invalid(ValidationError::new(
                ValidationCode::UnacknowledgedDestructiveOperation,
                Some("i_understand_this_may_lose_data"),
                "Rolling back discards everything written since the snapshot, on every \
                 replica. Acknowledge that to proceed.",
            )));
        }
        let membership = self.membership()?;
        let volume = self.recorded_volume(&membership, cluster, name)?.clone();
        let Some(source_seat) = volume.seat(source).cloned() else {
            return Err(DrbdError::NotFound(format!(
                "\"{source}\" holds no replica of \"{name}\"."
            )));
        };
        let resource = resource_name(cluster, name);

        // The device must be closed everywhere. This node's DRBD can answer
        // for the whole resource — its own role and every peer's — so the
        // check requires running from a member holding a replica.
        if volume.seat(self.node()).is_none() {
            return Err(DrbdError::Conflict(format!(
                "A rollback runs from a member holding a replica of \"{name}\" — open that \
                 node's console."
            )));
        }
        let statuses = self.backend.status().await?;
        if let Some(status) = statuses.iter().find(|s| s.name == resource) {
            let open_somewhere = status.role == "Primary"
                || status.connections.iter().any(|p| p.peer_role == "Primary");
            if open_somewhere {
                return Err(DrbdError::Conflict(format!(
                    "\"{name}\" is open somewhere — the machine using it must be off before a \
                     rollback."
                )));
            }
        }

        let nodes: Vec<(EnvironmentNode, VolumeSeat)> = volume
            .replicas
            .iter()
            .filter_map(|seat| {
                membership
                    .node(&seat.node)
                    .cloned()
                    .map(|n| (n, seat.clone()))
            })
            .collect();

        // Down everywhere. A member that will not go down stops the whole
        // workflow before anything destructive has happened.
        let mut downed: Vec<&EnvironmentNode> = Vec::new();
        let mut failure: Option<DrbdError> = None;
        for (node, _) in &nodes {
            match self.peers.down_resource(node, &resource).await {
                Ok(()) => downed.push(node),
                Err(err) => {
                    failure = Some(DrbdError::Conflict(format!(
                        "Taking the volume down on {} failed, so nothing was rolled back: {err}",
                        node.name
                    )));
                    break;
                }
            }
        }

        // The destructive step, on exactly one member.
        if failure.is_none() {
            let source_node = membership
                .node(&source_seat.node)
                .cloned()
                .expect("the seat came from the record");
            let payload = VolumeSnapshot {
                zvol: zvol_path(&source_seat.pool, cluster, name),
                snapshot: snapshot.to_string(),
            };
            if let Err(err) = self.peers.rollback_backing(&source_node, &payload).await {
                failure = Some(DrbdError::Conflict(format!(
                    "The rollback on {source} failed: {err}"
                )));
            }
        }

        // Up everywhere that went down — after a successful rollback and
        // after a failed one alike; a volume left down is a second outage.
        let mut up_failures = Vec::new();
        for node in downed.iter().rev() {
            if let Err(err) = self.peers.up_resource(node, &resource).await {
                up_failures.push(format!("{}: {err}", node.name));
            }
        }
        if let Some(err) = failure {
            return Err(err);
        }
        if !up_failures.is_empty() {
            return Err(DrbdError::Conflict(format!(
                "The rollback ran, but the volume did not come back up everywhere — bring it \
                 up by hand before using it. {}",
                up_failures.join(" ")
            )));
        }

        // The source is the truth now; everyone else resyncs from it.
        let source_node = membership
            .node(&source_seat.node)
            .cloned()
            .expect("the seat came from the record");
        self.peers
            .invalidate_remote(&source_node, &resource)
            .await?;
        tracing::info!(
            cluster,
            volume = name,
            snapshot,
            source,
            "volume rolled back"
        );
        Ok(())
    }

    /// Re-render every volume of a cluster with its *current* replication
    /// policy and apply it live — the 2→3 scale-out's closing move. A
    /// two-node cluster's volumes carried `fencing resource-and-stonith`;
    /// at three, majority quorum decides and the mechanisms fall away —
    /// decided, as always, by the topology engine and nowhere else. The
    /// shared-secret never travels: the file goes out with a placeholder
    /// and each member substitutes its own copy before writing.
    pub async fn refresh_policies(&self, cluster: &str) -> Result<()> {
        let _guard = self.gate.lock().await;
        let membership = self.membership()?;
        let Some(record) = membership.cluster_record(cluster) else {
            return Err(DrbdError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        let topology = ClusterTopology::new(record.definition.clone());
        let mut failures = Vec::new();
        for volume in &record.volumes {
            let resource = resource_name(cluster, &volume.name);
            let mut seats = Vec::new();
            for seat in &volume.replicas {
                let Some(core) = record
                    .networks
                    .core
                    .members
                    .iter()
                    .find(|m| m.node == seat.node)
                else {
                    continue;
                };
                seats.push(RenderSeat {
                    node: seat.node.clone(),
                    pool: seat.pool.clone(),
                    core_address: core.address,
                });
            }
            let policy = topology.replication_policy(volume.replicas.len());
            let rendered = resource_file(
                &resource,
                cluster,
                volume,
                &seats,
                &policy,
                crate::peers::SECRET_PLACEHOLDER,
            );
            for seat in &volume.replicas {
                let Some(node) = membership.node(&seat.node).cloned() else {
                    continue;
                };
                let payload = crate::peers::VolumeApply {
                    resource: resource.clone(),
                    resource_file: rendered.clone(),
                };
                if let Err(err) = self.peers.apply_policy(&node, &payload).await {
                    failures.push(format!("{} on {}: {err}", volume.name, node.name));
                }
            }
        }
        if !failures.is_empty() {
            return Err(DrbdError::Conflict(format!(
                "Not every volume took the new replication policy — re-run this after fixing: \
                 {}",
                failures.join(" ")
            )));
        }
        tracing::info!(cluster, "volume replication policies refreshed");
        Ok(())
    }

    /// A peer handed this node a re-rendered file: substitute this node's
    /// own secret for the placeholder, write, and adjust — live.
    pub async fn peer_apply_policy(&self, payload: &crate::peers::VolumeApply) -> Result<()> {
        let existing = self.backend.read_resource(&payload.resource).await?;
        let Some(secret) = extract_secret(&existing) else {
            return Err(DrbdError::Conflict(format!(
                "The existing file for {} carries no shared-secret to keep.",
                payload.resource
            )));
        };
        let rendered = payload
            .resource_file
            .replace(crate::peers::SECRET_PLACEHOLDER, &secret);
        self.backend
            .write_resource(&payload.resource, &rendered)
            .await?;
        self.backend.adjust(&payload.resource).await
    }

    /// Resolve a split brain: the operator names the victim, whose divergent
    /// writes are discarded, and every side reconnects. Offered by the
    /// console only while a connection is StandAlone, and guarded twice —
    /// the acknowledgement here, the victim's name typed in the dialog.
    pub async fn resolve_split_brain(
        &self,
        cluster: &str,
        name: &str,
        victim: &str,
        ack: Acknowledgements,
    ) -> Result<()> {
        let _guard = self.gate.lock().await;
        if !ack.may_lose_data {
            return Err(DrbdError::invalid(ValidationError::new(
                ValidationCode::UnacknowledgedDestructiveOperation,
                Some("i_understand_this_may_lose_data"),
                "Resolving a split brain discards everything the victim wrote on its own. \
                 Acknowledge that to proceed.",
            )));
        }
        let membership = self.membership()?;
        let volume = self.recorded_volume(&membership, cluster, name)?.clone();
        if volume.seat(victim).is_none() {
            return Err(DrbdError::NotFound(format!(
                "\"{victim}\" holds no replica of \"{name}\"."
            )));
        }
        let resource = resource_name(cluster, name);

        // The victim first, discarding; then the survivors, keeping. Order
        // matters: a survivor reconnecting first would meet the victim's
        // divergent generation and refuse again.
        let victim_node = membership.node(victim).cloned().ok_or_else(|| {
            DrbdError::Conflict(format!("{victim} is not in the environment record"))
        })?;
        self.peers.reconnect(&victim_node, &resource, true).await?;
        for seat in &volume.replicas {
            if seat.node == victim {
                continue;
            }
            let Some(node) = membership.node(&seat.node).cloned() else {
                continue;
            };
            if let Err(err) = self.peers.reconnect(&node, &resource, false).await {
                tracing::warn!(node = %seat.node, "the survivor did not reconnect: {err}");
            }
        }
        tracing::warn!(cluster, volume = name, victim, "split brain resolved");
        Ok(())
    }

    // --- the compute domain's door ------------------------------------------

    /// The cluster this node is a member of — where a machine on this node
    /// keeps its replicated disks.
    pub(crate) fn home_cluster(&self) -> Result<(String, String)> {
        let membership = self.membership()?;
        let node = self.node().to_string();
        let cluster = membership
            .node(&node)
            .and_then(|n| n.cluster.clone())
            .ok_or_else(|| {
                DrbdError::Conflict(format!(
                    "\"{node}\" is not a member of a cluster, and a replicated disk needs one."
                ))
            })?;
        Ok((cluster, node))
    }

    pub(crate) fn membership_or_none(&self) -> Result<Option<EnvironmentMembership>> {
        self.cluster.environment_record().map_err(DrbdError::from)
    }

    /// Placement for a machine's disk on `node`: the node itself — its
    /// device must exist where the machine runs — plus the least-utilized
    /// other member, each on the pool its existing replicas already use.
    pub(crate) fn place_for_node(
        &self,
        cluster: &str,
        node: &str,
    ) -> Result<Vec<VolumeMemberCreate>> {
        let membership = self.membership()?;
        let Some(record) = membership.cluster_record(cluster) else {
            return Err(DrbdError::NotFound(format!(
                "There is no cluster called \"{cluster}\" in this environment."
            )));
        };
        let pool_of = |n: &str| {
            record
                .volumes
                .iter()
                .find_map(|v| v.seat(n).map(|s| s.pool.clone()))
        };
        let seat_for = |n: &str| -> Result<VolumeMemberCreate> {
            let Some(pool) = pool_of(n) else {
                return Err(DrbdError::Conflict(format!(
                    "\"{n}\" has never held a replica, so there is no pool to infer — choose \
                     the members and pools explicitly."
                )));
            };
            Ok(VolumeMemberCreate {
                node: n.to_string(),
                pool,
            })
        };

        let mut members = vec![seat_for(node)?];
        let mut others: Vec<(String, usize)> = record
            .definition
            .node_names()
            .into_iter()
            .filter(|n| n != node)
            .map(|n| {
                let seats = record
                    .volumes
                    .iter()
                    .filter(|v| v.seat(&n).is_some())
                    .count();
                (n, seats)
            })
            .collect();
        others.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        let Some((other, _)) = others.into_iter().next() else {
            return Err(DrbdError::Conflict(format!(
                "\"{cluster}\" has no other member to hold the second replica."
            )));
        };
        members.push(seat_for(&other)?);
        Ok(members)
    }

    /// Open or close the two-primaries window on every member of one
    /// volume. The order is not transactional on purpose: the caller — the
    /// migration guard — closes on every path out, so a half-opened window
    /// is closed the same way a fully opened one is.
    pub(crate) async fn adjust_two_primaries(
        &self,
        cluster: &str,
        name: &str,
        allow: bool,
    ) -> Result<()> {
        let membership = self.membership()?;
        let volume = self.recorded_volume(&membership, cluster, name)?.clone();
        let resource = resource_name(cluster, name);
        for seat in &volume.replicas {
            let Some(node) = membership.node(&seat.node).cloned() else {
                return Err(DrbdError::Conflict(format!(
                    "{} is not in the environment record",
                    seat.node
                )));
            };
            self.peers.two_primaries(&node, &resource, allow).await?;
        }
        Ok(())
    }

    // --- validation ---------------------------------------------------------

    /// The replica seats, chosen or defaulted. Default placement is the
    /// least-utilized members — fewest existing replica seats — each on the
    /// pool its existing seats already use; a node that has never held a
    /// replica has no pool to infer, and the refusal says to choose one.
    fn placed_members(
        &self,
        request: &VolumeCreate,
        record: &ClusterRecord,
    ) -> Result<Vec<VolumeMemberCreate>> {
        if !request.members.is_empty() {
            return Ok(request.members.clone());
        }
        let mut load: Vec<(String, usize)> = record
            .definition
            .node_names()
            .into_iter()
            .map(|node| {
                let seats = record
                    .volumes
                    .iter()
                    .filter(|v| v.seat(&node).is_some())
                    .count();
                (node, seats)
            })
            .collect();
        load.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        let mut members = Vec::new();
        for (node, _) in load.into_iter().take(MIN_REPLICAS) {
            let Some(pool) = record
                .volumes
                .iter()
                .find_map(|v| v.seat(&node).map(|s| s.pool.clone()))
            else {
                return Err(DrbdError::Conflict(format!(
                    "\"{node}\" has never held a replica, so there is no pool to infer — choose \
                     the members and pools explicitly."
                )));
            };
            members.push(VolumeMemberCreate { node, pool });
        }
        Ok(members)
    }

    /// Every rule over the request, all problems collected, and the record
    /// the volume will be — allocation included — if they all pass.
    fn validated_record(
        &self,
        request: &VolumeCreate,
        record: &ClusterRecord,
        members: &[VolumeMemberCreate],
    ) -> Result<VolumeRecord> {
        let mut errors = Vec::new();

        if !valid_volume_name(&request.name) {
            errors.push(ValidationError::new(
                ValidationCode::InvalidVolumeName,
                Some("name"),
                format!(
                    "\"{}\" is not a usable volume name — lowercase letters, digits, and \
                     hyphens, starting with a letter, at most 48 characters.",
                    request.name
                ),
            ));
        }
        if record.volume(&request.name).is_some() {
            errors.push(ValidationError::new(
                ValidationCode::DuplicateVolumeName,
                Some("name"),
                format!(
                    "\"{}\" already has a volume called \"{}\".",
                    record.definition.name, request.name
                ),
            ));
        }
        if request.size_bytes == 0 {
            errors.push(ValidationError::new(
                ValidationCode::InvalidVolumeSize,
                Some("size_bytes"),
                "A volume needs a size greater than zero.",
            ));
        }

        let names: Vec<String> = members.iter().map(|m| m.node.clone()).collect();
        let mut deduped = names.clone();
        deduped.sort();
        deduped.dedup();
        if deduped.len() != names.len() {
            errors.push(ValidationError::new(
                ValidationCode::DuplicateNode,
                Some("members"),
                "A node carries at most one replica of a volume.",
            ));
        }
        errors.extend(lumen_cluster::validate::validate_volume_members(
            &names,
            &record.definition.node_names(),
        ));
        match record.definition.regime() {
            Regime::TwoNode if names.len() != 2 => {
                errors.push(ValidationError::new(
                    ValidationCode::TooManyVolumeMembers,
                    Some("members"),
                    "A two-node cluster replicates every volume to both nodes — there is \
                     nowhere else for a replica to live.",
                ));
            }
            Regime::Quorum if names.len() > MAX_REPLICAS => {
                errors.push(ValidationError::new(
                    ValidationCode::TooManyVolumeMembers,
                    Some("members"),
                    format!("A volume has at most {MAX_REPLICAS} replicas."),
                ));
            }
            _ => {}
        }

        if !errors.is_empty() {
            return Err(DrbdError::Invalid(errors));
        }

        let Some(port) = allocate_port(&record.volumes) else {
            return Err(DrbdError::Conflict(format!(
                "\"{}\" has used its whole replication port range ({} volumes). Destroy one \
                 first.",
                record.definition.name,
                record.volumes.len()
            )));
        };

        Ok(VolumeRecord {
            name: request.name.clone(),
            size_bytes: request.size_bytes,
            zvol_bytes: backing_size(request.size_bytes, members.len()),
            port,
            minor: allocate_minor(&record.volumes),
            replicas: members
                .iter()
                .map(|m| VolumeSeat {
                    node: m.node.clone(),
                    pool: m.pool.clone(),
                })
                .collect(),
        })
    }

    fn recorded_volume<'a>(
        &self,
        membership: &'a EnvironmentMembership,
        cluster: &str,
        name: &str,
    ) -> Result<&'a VolumeRecord> {
        membership
            .cluster_record(cluster)
            .ok_or_else(|| {
                DrbdError::NotFound(format!(
                    "There is no cluster called \"{cluster}\" in this environment."
                ))
            })?
            .volume(name)
            .ok_or_else(|| {
                DrbdError::NotFound(format!("\"{cluster}\" has no volume called \"{name}\"."))
            })
    }
}

/// The shared-secret out of an existing resource file — kept on the node,
/// substituted into re-rendered files, never sent anywhere.
fn extract_secret(resource_file: &str) -> Option<String> {
    let start = resource_file.find("shared-secret \"")? + "shared-secret \"".len();
    let rest = &resource_file[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// `tank/lumen/alpha-v0` → (`tank`, `alpha-v0`): the pieces
/// `StorageService::create_volume` takes.
fn split_zvol(zvol: &str) -> Result<(&str, &str)> {
    let mut parts = zvol.splitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(pool), Some("lumen"), Some(name)) if !pool.is_empty() && !name.is_empty() => {
            Ok((pool, name))
        }
        _ => Err(DrbdError::Conflict(format!(
            "\"{zvol}\" is not a path inside a pool's lumen namespace."
        ))),
    }
}

fn derive_health(status: &ResourceStatus) -> (VolumeHealth, Option<f64>) {
    if status.suspended_quorum {
        return (VolumeHealth::SuspendedNoQuorum, None);
    }
    if status.suspended_fencing || status.suspended {
        return (VolumeHealth::Suspended, None);
    }
    let syncing: Vec<f64> = status
        .connections
        .iter()
        .filter_map(|p| p.device())
        .filter(|d| d.syncing())
        .map(|d| d.percent_in_sync)
        .collect();
    if !syncing.is_empty() {
        let percent = syncing.iter().fold(f64::INFINITY, |a, &b| a.min(b));
        return (VolumeHealth::Syncing, Some(percent));
    }
    if status
        .connections
        .iter()
        .any(|p| p.connection_state == "StandAlone")
    {
        return (VolumeHealth::StandAlone, None);
    }
    if status.disk_state() == Some("Outdated") {
        return (VolumeHealth::Outdated, None);
    }
    let peers_healthy = status.connections.iter().all(|p| {
        p.connection_state == "Connected"
            && p.device().is_some_and(|d| d.peer_disk_state == "UpToDate")
    });
    if status.disk_state() == Some("UpToDate") && peers_healthy {
        (VolumeHealth::UpToDate, None)
    } else {
        (VolumeHealth::Degraded, None)
    }
}

fn replica_view_of(
    seat: &VolumeSeat,
    local_node: &str,
    status: Option<&ResourceStatus>,
) -> ReplicaView {
    let local = seat.node == local_node;
    let (disk_state, role, connection) = match status {
        None => (None, None, None),
        Some(status) if local => (
            status.disk_state().map(str::to_string),
            Some(status.role.clone()),
            None,
        ),
        Some(status) => match status.peer(&seat.node) {
            None => (None, None, None),
            Some(peer) => (
                peer.device().map(|d| d.peer_disk_state.clone()),
                Some(peer.peer_role.clone()),
                Some(peer.connection_state.clone()),
            ),
        },
    };
    ReplicaView {
        node: seat.node.clone(),
        pool: seat.pool.clone(),
        local,
        disk_state,
        role,
        connection,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use lumen_cluster::backend::mock::{membership_of, MockBackend as ClusterMockBackend};
    use lumen_cluster::networks::{
        AddressedMember, ClusterNetworks, CoreNetwork, ManagementNetwork,
    };
    use lumen_cluster::{BmcConfig, ClusterDefinition, MemberNode};

    use crate::backend::mock::{syncing_resource, MockBackend};
    use crate::peers::MockVolumePeers;

    fn test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lumen-drbd-service-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seat(node: &str, last_octet: u8) -> (MemberNode, AddressedMember) {
        (
            MemberNode {
                name: node.into(),
                ring0: std::net::Ipv4Addr::new(10, 10, 0, last_octet),
                ring1: std::net::Ipv4Addr::new(192, 168, 10, last_octet),
                bmc: BmcConfig {
                    address: format!("10.20.0.{last_octet}"),
                    username: "ADMIN".into(),
                },
            },
            AddressedMember {
                node: node.into(),
                interface: "nic1".into(),
                address: std::net::Ipv4Addr::new(10, 10, 0, last_octet),
            },
        )
    }

    /// A membership whose two-node cluster "alpha" is recorded whole —
    /// definition, networks, no volumes yet — with the coordinator alpha-1
    /// a member and one spare outside.
    fn alpha_membership() -> EnvironmentMembership {
        let mut membership = membership_of(&[
            ("alpha-1", Some("alpha")),
            ("alpha-2", Some("alpha")),
            ("spare-1", None),
        ]);
        let (node1, core1) = seat("alpha-1", 1);
        let (node2, core2) = seat("alpha-2", 2);
        let definition = ClusterDefinition {
            name: "alpha".into(),
            nodes: vec![node1, node2],
            preferred_node: Some("alpha-1".into()),
        };
        let networks = ClusterNetworks {
            core: CoreNetwork {
                subnet: "10.10.0.0/24".parse().unwrap(),
                mtu: 9000,
                members: vec![core1, core2],
            },
            management: ManagementNetwork {
                subnet: "192.168.10.0/24".parse().unwrap(),
                vip: None,
                members: Vec::new(),
            },
            external: Vec::new(),
        };
        membership
            .clusters
            .push(ClusterRecord::new(definition, networks));
        membership
    }

    fn service_with(
        backend: Arc<MockBackend>,
        peers: Arc<MockVolumePeers>,
        membership: &EnvironmentMembership,
        tag: &str,
    ) -> DrbdService {
        let network = Arc::new(lumen_net::NetworkService::new(
            Arc::new(lumen_net::backend::mock::MockBackend::appliance()),
            &test_dir(&format!("{tag}-net")),
            60,
        ));
        let cluster = Arc::new(
            ClusterService::new(
                Arc::new(ClusterMockBackend::environment()),
                Arc::new(lumen_cluster::MockPeers::new()),
                network,
                &test_dir(tag),
                "0.3.0",
            )
            .with_node("alpha-1")
            .with_environment(membership),
        );
        let storage = Arc::new(StorageService::new(Arc::new(
            lumen_zfs::backend::mock::MockBackend::appliance(),
        )));
        DrbdService::new(backend, peers, cluster, storage)
    }

    fn create_request(name: &str, size: u64) -> VolumeCreate {
        VolumeCreate {
            cluster: "alpha".into(),
            name: name.into(),
            size_bytes: size,
            members: vec![
                VolumeMemberCreate {
                    node: "alpha-1".into(),
                    pool: "boot".into(),
                },
                VolumeMemberCreate {
                    node: "alpha-2".into(),
                    pool: "boot".into(),
                },
            ],
        }
    }

    #[tokio::test]
    async fn a_create_prepares_every_member_primes_once_and_records_last() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new().with_backend(backend.clone()));
        let service = service_with(backend, peers.clone(), &alpha_membership(), "create-ok");

        let view = service
            .create_volume(create_request("vm-101-disk-0", 10 << 30))
            .await
            .unwrap();
        assert_eq!(view.resource, "alpha-vm-101-disk-0");
        assert_eq!(view.device, "/dev/drbd1");
        assert_eq!(view.port, crate::model::DRBD_PORT_MIN);
        assert_eq!(view.health, VolumeHealth::UpToDate, "{view:?}");

        // Every member prepared with the identical byte-exact backing size,
        // and the rendered file carries the two-node regime.
        let prepared = peers.prepared();
        assert_eq!(prepared.len(), 2);
        let sizes: Vec<u64> = prepared.iter().map(|(_, p)| p.zvol_bytes).collect();
        assert_eq!(sizes[0], sizes[1]);
        assert_eq!(sizes[0], backing_size(10 << 30, 2));
        assert!(prepared[0]
            .1
            .resource_file
            .contains("fencing resource-and-stonith"));
        assert!(prepared[0].1.resource_file.contains("shared-secret"));
        // The initial sync is skipped exactly once.
        assert_eq!(peers.primed().len(), 1);

        // The record carries the volume, and the view groups it by cluster.
        let response = service.volumes().await.unwrap();
        assert_eq!(response.clusters.len(), 1);
        assert_eq!(response.clusters[0].cluster, "alpha");
        assert_eq!(response.clusters[0].volumes[0].name, "vm-101-disk-0");
    }

    #[tokio::test]
    async fn a_failed_create_unwinds_the_touched_members_and_records_nothing() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(
            MockVolumePeers::new()
                .with_backend(backend.clone())
                .fail_prepare_on("alpha-2"),
        );
        let service = service_with(backend, peers.clone(), &alpha_membership(), "create-unwind");

        let err = service
            .create_volume(create_request("vm-101-disk-0", 10 << 30))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("alpha-2"), "{err}");

        // Only the member that was touched is unwound, and nothing was
        // recorded — the volume never happened.
        let torn = peers.torn_down();
        assert_eq!(torn.len(), 1);
        assert_eq!(torn[0].0, "alpha-1");
        assert!(service.volumes().await.unwrap().clusters.is_empty());
    }

    #[tokio::test]
    async fn a_malformed_create_carries_every_problem() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new());
        let service = service_with(
            backend,
            peers.clone(),
            &alpha_membership(),
            "create-invalid",
        );

        let mut request = create_request("Bad Name", 0);
        request.members[1].node = "spare-1".into();
        let err = service.create_volume(request).await.unwrap_err();
        let DrbdError::Invalid(errors) = err else {
            panic!("expected a validation answer, got {err}");
        };
        let codes: Vec<&str> = errors.iter().map(|e| e.code.as_str()).collect();
        assert!(codes.contains(&"invalid_volume_name"), "{codes:?}");
        assert!(codes.contains(&"invalid_volume_size"), "{codes:?}");
        assert!(
            codes.contains(&"volume_member_outside_cluster"),
            "{codes:?}"
        );
        assert!(peers.prepared().is_empty(), "nothing may have been touched");
    }

    #[tokio::test]
    async fn default_placement_is_least_utilized_with_inferred_pools() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new().with_backend(backend.clone()));
        let service = service_with(backend, peers.clone(), &alpha_membership(), "placement");

        // Nothing to infer from yet: the refusal says to choose.
        let mut bare = create_request("vm-1-disk-0", 1 << 30);
        bare.members.clear();
        let err = service.create_volume(bare.clone()).await.unwrap_err();
        assert!(err.to_string().contains("choose"), "{err}");

        // After one explicit volume, placement can be inferred.
        service
            .create_volume(create_request("vm-1-disk-0", 1 << 30))
            .await
            .unwrap();
        bare.name = "vm-2-disk-0".into();
        let view = service.create_volume(bare).await.unwrap();
        let mut nodes: Vec<&str> = view.replicas.iter().map(|r| r.node.as_str()).collect();
        nodes.sort();
        assert_eq!(nodes, vec!["alpha-1", "alpha-2"]);
        assert!(view.replicas.iter().all(|r| r.pool == "boot"));
        // Allocation moved on: second volume, second port, second minor.
        assert_eq!(view.port, crate::model::DRBD_PORT_MIN + 1);
        assert_eq!(view.minor, 2);
    }

    #[tokio::test]
    async fn destroying_needs_the_acknowledgement_and_every_replica() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new().with_backend(backend.clone()));
        let service = service_with(
            backend.clone(),
            peers.clone(),
            &alpha_membership(),
            "destroy",
        );
        service
            .create_volume(create_request("vm-101-disk-0", 1 << 30))
            .await
            .unwrap();

        let refused = service
            .destroy_volume("alpha", "vm-101-disk-0", Acknowledgements::default())
            .await
            .unwrap_err();
        assert!(matches!(refused, DrbdError::Invalid(_)));

        service
            .destroy_volume(
                "alpha",
                "vm-101-disk-0",
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(peers.torn_down().len(), 2);
        assert!(service.volumes().await.unwrap().clusters.is_empty());
    }

    #[tokio::test]
    async fn a_destroy_that_loses_a_member_leaves_the_volume_recorded() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(
            MockVolumePeers::new()
                .with_backend(backend.clone())
                .fail_teardown_on("alpha-2"),
        );
        let service = service_with(backend, peers.clone(), &alpha_membership(), "destroy-stuck");
        service
            .create_volume(create_request("vm-101-disk-0", 1 << 30))
            .await
            .unwrap();

        let err = service
            .destroy_volume(
                "alpha",
                "vm-101-disk-0",
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("still recorded"), "{err}");
        assert_eq!(
            service.volumes().await.unwrap().clusters[0].volumes.len(),
            1
        );
    }

    #[tokio::test]
    async fn a_resize_grows_every_backing_then_the_resource_then_the_record() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new().with_backend(backend.clone()));
        let service = service_with(backend, peers.clone(), &alpha_membership(), "resize");
        service
            .create_volume(create_request("vm-101-disk-0", 1 << 30))
            .await
            .unwrap();

        // Shrinking is refused before anything is touched.
        let err = service
            .resize_volume("alpha", "vm-101-disk-0", 1 << 29)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only grows"), "{err}");

        service
            .resize_volume("alpha", "vm-101-disk-0", 4 << 30)
            .await
            .unwrap();
        let resized = peers.resized();
        assert_eq!(resized.len(), 2);
        let expected = backing_size(4 << 30, 2);
        assert!(resized.iter().all(|(_, p)| p.zvol_bytes == expected));
        assert_eq!(peers.grown().len(), 1, "the resource grows exactly once");

        let response = service.volumes().await.unwrap();
        assert_eq!(response.clusters[0].volumes[0].size_bytes, 4 << 30);
    }

    #[tokio::test]
    async fn a_node_with_no_replica_says_where_the_state_is_known() {
        // A second cluster this node is not part of, with a recorded volume.
        let mut membership = alpha_membership();
        membership.nodes.push(lumen_cluster::EnvironmentNode {
            name: "beta-1".into(),
            address: "192.168.20.1:8443".into(),
            controlplane_version: "0.3.0".into(),
            cluster: Some("beta".into()),
            maintenance: None,
        });
        membership.nodes.push(lumen_cluster::EnvironmentNode {
            name: "beta-2".into(),
            address: "192.168.20.2:8443".into(),
            controlplane_version: "0.3.0".into(),
            cluster: Some("beta".into()),
            maintenance: None,
        });
        let (b1, core1) = seat("beta-1", 1);
        let (b2, core2) = seat("beta-2", 2);
        let mut record = ClusterRecord::new(
            ClusterDefinition {
                name: "beta".into(),
                nodes: vec![b1, b2],
                preferred_node: Some("beta-1".into()),
            },
            ClusterNetworks {
                core: CoreNetwork {
                    subnet: "10.20.0.0/24".parse().unwrap(),
                    mtu: 9000,
                    members: vec![core1, core2],
                },
                management: ManagementNetwork {
                    subnet: "192.168.20.0/24".parse().unwrap(),
                    vip: None,
                    members: Vec::new(),
                },
                external: Vec::new(),
            },
        );
        record.volumes.push(VolumeRecord {
            name: "vm-7-disk-0".into(),
            size_bytes: 1 << 30,
            zvol_bytes: backing_size(1 << 30, 2),
            port: 7788,
            minor: 1,
            replicas: vec![
                VolumeSeat {
                    node: "beta-1".into(),
                    pool: "tank".into(),
                },
                VolumeSeat {
                    node: "beta-2".into(),
                    pool: "tank".into(),
                },
            ],
        });
        membership.clusters.push(record);

        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new());
        let service = service_with(backend, peers, &membership, "no-replica");

        let response = service.volumes().await.unwrap();
        let beta = response
            .clusters
            .iter()
            .find(|c| c.cluster == "beta")
            .unwrap();
        let volume = &beta.volumes[0];
        assert_eq!(volume.health, VolumeHealth::Unknown);
        assert!(
            volume.error.as_deref().unwrap_or("").contains("beta-1"),
            "{volume:?}"
        );
        assert!(volume.replicas.iter().all(|r| r.disk_state.is_none()));
    }

    /// The peer side, which is what actually touches a node: the zvol is
    /// created thick in the pool's lumen namespace, the resource file
    /// written, the metadata sized for the peer count, the resource up —
    /// and teardown reverses all of it.
    #[tokio::test]
    async fn a_peer_prepares_whole_and_tears_down_whole() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new());
        let service = service_with(backend.clone(), peers, &alpha_membership(), "peer-side");

        let payload = VolumePrepare {
            cluster: "alpha".into(),
            resource: "alpha-vm-101-disk-0".into(),
            zvol: "boot/lumen/alpha-vm-101-disk-0".into(),
            zvol_bytes: 1 << 30,
            volblocksize: VOLBLOCKSIZE,
            minor: 1,
            peers: 1,
            resource_file: "resource …".into(),
        };
        service.peer_prepare(&payload).await.unwrap();
        assert_eq!(backend.written().len(), 1);
        assert_eq!(
            backend.metadata_created(),
            vec![("alpha-vm-101-disk-0".to_string(), 1)]
        );
        assert_eq!(backend.brought_up(), vec!["alpha-vm-101-disk-0"]);
        // The zvol exists where the payload said, at the exact size.
        let volumes = service.storage.volumes("boot").await.unwrap();
        let zvol = volumes
            .volumes
            .iter()
            .find(|v| v.name == "boot/lumen/alpha-vm-101-disk-0")
            .expect("the backing zvol was created");
        assert_eq!(zvol.volsize, Some(1 << 30));

        service
            .peer_teardown(&VolumeTeardown {
                resource: "alpha-vm-101-disk-0".into(),
                zvol: "boot/lumen/alpha-vm-101-disk-0".into(),
            })
            .await
            .unwrap();
        assert_eq!(backend.taken_down(), vec!["alpha-vm-101-disk-0"]);
        assert_eq!(backend.removed(), vec!["alpha-vm-101-disk-0"]);
        let volumes = service.storage.volumes("boot").await.unwrap();
        assert!(!volumes
            .volumes
            .iter()
            .any(|v| v.name == "boot/lumen/alpha-vm-101-disk-0"));

        // A zvol outside the lumen namespace is refused before anything runs.
        let err = service
            .peer_prepare(&VolumePrepare {
                zvol: "boot/data".into(),
                ..payload
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("lumen namespace"), "{err}");
    }

    /// The compute domain's door: placement always includes this node,
    /// devices resolve back to their volumes, and the two-primaries window
    /// reaches every member in both directions.
    #[tokio::test]
    async fn the_compute_door_places_on_this_node_and_works_the_window() {
        use crate::vm::{MigrationWindow, VmDiskRequest, VmVolumes};
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new().with_backend(backend.clone()));
        let service = service_with(backend, peers.clone(), &alpha_membership(), "vm-door");

        // Nothing to infer from yet: default placement says to choose.
        let bare = VmDiskRequest {
            name: "vm-1-disk-0".into(),
            size_bytes: 1 << 30,
            members: Vec::new(),
        };
        let err = service.create_disk(&bare).await.unwrap_err();
        assert!(err.to_string().contains("choose"), "{err}");

        // Explicit members must include the machine's own node.
        let elsewhere = VmDiskRequest {
            members: vec![VolumeMemberCreate {
                node: "alpha-2".into(),
                pool: "boot".into(),
            }],
            ..bare.clone()
        };
        let err = service.create_disk(&elsewhere).await.unwrap_err();
        assert!(err.to_string().contains("must hold a replica"), "{err}");

        let disk = service
            .create_disk(&VmDiskRequest {
                members: vec![
                    VolumeMemberCreate {
                        node: "alpha-1".into(),
                        pool: "boot".into(),
                    },
                    VolumeMemberCreate {
                        node: "alpha-2".into(),
                        pool: "boot".into(),
                    },
                ],
                ..bare.clone()
            })
            .await
            .unwrap();
        assert_eq!(disk.device, "/dev/drbd1");
        assert_eq!(disk.members, vec!["alpha-1", "alpha-2"]);

        // A second disk can be placed automatically now, on this node.
        let second = service
            .create_disk(&VmDiskRequest {
                name: "vm-1-disk-1".into(),
                size_bytes: 1 << 30,
                members: Vec::new(),
            })
            .await
            .unwrap();
        assert!(second.members.contains(&"alpha-1".to_string()));

        // Devices resolve back; a zvol path is simply not ours.
        let found = service.disk_of(&disk.device).await.unwrap().unwrap();
        assert_eq!(found.name, "vm-1-disk-0");
        assert!(service
            .disk_of("/dev/zvol/boot/lumen/vm-1-disk-0")
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            service
                .common_members(&[disk.device.clone(), second.device.clone()])
                .await
                .unwrap(),
            vec!["alpha-1", "alpha-2"]
        );

        // The window opens on every member and closes on every member —
        // and a completed migration closes it exactly as an abandoned one
        // would, because DRBD has only the one switch.
        service
            .migration_window(
                &disk.device,
                MigrationWindow::Open {
                    destination: "alpha-2".to_string(),
                },
            )
            .await
            .unwrap();
        service
            .migration_window(&disk.device, MigrationWindow::Accepted)
            .await
            .unwrap();
        let adjustments = peers.two_primaries();
        assert_eq!(adjustments.len(), 4, "{adjustments:?}");
        assert!(adjustments[..2].iter().all(|(_, _, allow)| *allow));
        assert!(adjustments[2..].iter().all(|(_, _, allow)| !*allow));

        // Destroy through the door needs no second acknowledgement and
        // forgets the record.
        service.destroy_disk(&second.device).await.unwrap();
        assert!(service.disk_of(&second.device).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_snapshot_lands_on_every_replica_or_on_none() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new().with_backend(backend.clone()));
        let service = service_with(backend, peers.clone(), &alpha_membership(), "snap");
        service
            .create_volume(create_request("vm-101-disk-0", 1 << 30))
            .await
            .unwrap();

        service
            .snapshot_volume("alpha", "vm-101-disk-0", "before-upgrade")
            .await
            .unwrap();
        let taken = peers.snapshots();
        assert_eq!(taken.len(), 2, "{taken:?}");
        assert!(taken
            .iter()
            .all(|(_, s)| s.snapshot == "before-upgrade"
                && s.zvol == "boot/lumen/alpha-vm-101-disk-0"));

        // A bad name is a validation answer, not a half-taken snapshot.
        let err = service
            .snapshot_volume("alpha", "vm-101-disk-0", "Bad Name")
            .await
            .unwrap_err();
        assert!(matches!(err, DrbdError::Invalid(_)));
    }

    #[tokio::test]
    async fn a_failed_snapshot_unwinds_the_ones_already_taken() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(
            MockVolumePeers::new()
                .with_backend(backend.clone())
                .fail_snapshot_on("alpha-2"),
        );
        let service = service_with(backend, peers.clone(), &alpha_membership(), "snap-unwind");
        service
            .create_volume(create_request("vm-101-disk-0", 1 << 30))
            .await
            .unwrap();

        let err = service
            .snapshot_volume("alpha", "vm-101-disk-0", "before")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not taken anywhere"), "{err}");
        // The one that succeeded was dropped again.
        assert_eq!(peers.dropped_snapshots().len(), 1);
        assert_eq!(peers.dropped_snapshots()[0].0, "alpha-1");
    }

    /// The rollback workflow, in its exact order: down everywhere, roll back
    /// the chosen source, up everywhere, and the source declared the truth.
    #[tokio::test]
    async fn a_rollback_is_one_transaction_with_the_source_as_the_truth() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new().with_backend(backend.clone()));
        let service = service_with(
            backend.clone(),
            peers.clone(),
            &alpha_membership(),
            "rollback",
        );
        service
            .create_volume(create_request("vm-101-disk-0", 1 << 30))
            .await
            .unwrap();

        // Refused without the acknowledgement.
        let refused = service
            .rollback_volume(
                "alpha",
                "vm-101-disk-0",
                "before",
                "alpha-2",
                Acknowledgements::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(refused, DrbdError::Invalid(_)));

        service
            .rollback_volume(
                "alpha",
                "vm-101-disk-0",
                "before",
                "alpha-2",
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap();

        assert_eq!(peers.downs().len(), 2, "down everywhere first");
        let rollbacks = peers.rollbacks();
        assert_eq!(rollbacks.len(), 1, "the destructive step runs once");
        assert_eq!(rollbacks[0].0, "alpha-2");
        assert_eq!(peers.ups().len(), 2, "up everywhere after");
        let invalidated = peers.invalidated();
        assert_eq!(
            invalidated,
            vec![("alpha-2".to_string(), "alpha-vm-101-disk-0".to_string())]
        );
    }

    #[tokio::test]
    async fn a_rollback_is_refused_while_the_device_is_open() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new().with_backend(backend.clone()));
        let service = service_with(
            backend.clone(),
            peers.clone(),
            &alpha_membership(),
            "rollback-open",
        );
        service
            .create_volume(create_request("vm-101-disk-0", 1 << 30))
            .await
            .unwrap();

        // Simulate the machine holding the device open: this node Primary.
        let mut open = crate::backend::mock::healthy_resource("alpha-vm-101-disk-0", "alpha-2", 1);
        open.role = "Primary".into();
        backend.register(open);

        let err = service
            .rollback_volume(
                "alpha",
                "vm-101-disk-0",
                "before",
                "alpha-1",
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must be off"), "{err}");
        assert!(peers.downs().is_empty(), "nothing was touched");
    }

    /// The 2→3 policy flip: every volume re-rendered with the quorum
    /// regime's policy — mechanisms gone for a two-replica volume — with a
    /// placeholder where the secret goes, and the peer side substituting
    /// its own before writing.
    #[tokio::test]
    async fn the_policy_flip_rerenders_without_the_secret_ever_leaving() {
        // The membership a completed scale-out leaves: three members in the
        // definition, the old two-replica volume still recorded.
        let mut membership = alpha_membership();
        let (node3, core3) = seat("alpha-3", 3);
        membership.nodes.push(lumen_cluster::EnvironmentNode {
            name: "alpha-3".into(),
            address: "192.168.10.3:8443".into(),
            controlplane_version: "0.3.0".into(),
            cluster: Some("alpha".into()),
            maintenance: None,
        });
        let record = membership
            .clusters
            .iter_mut()
            .find(|r| r.definition.name == "alpha")
            .unwrap();
        record.definition.nodes.push(node3);
        record.definition.preferred_node = None;
        record.networks.core.members.push(core3);
        record.volumes.push(VolumeRecord {
            name: "vm-101-disk-0".into(),
            size_bytes: 1 << 30,
            zvol_bytes: backing_size(1 << 30, 2),
            port: 7788,
            minor: 1,
            replicas: vec![
                VolumeSeat {
                    node: "alpha-1".into(),
                    pool: "boot".into(),
                },
                VolumeSeat {
                    node: "alpha-2".into(),
                    pool: "boot".into(),
                },
            ],
        });

        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new());
        let service = service_with(backend.clone(), peers.clone(), &membership, "policy-flip");

        service.refresh_policies("alpha").await.unwrap();
        let applied = peers.applied();
        assert_eq!(applied.len(), 2, "one push per replica");
        let file = &applied[0].1.resource_file;
        assert!(
            file.contains("@@LUMEN-SECRET@@"),
            "the secret never travels"
        );
        assert!(
            !file.contains("fencing resource-and-stonith"),
            "the two-node mechanism fell away"
        );
        assert!(
            !file.contains("quorum majority"),
            "two replicas at three nodes carry neither"
        );

        // The peer side keeps its own secret: placeholder in, local secret
        // out, adjusted live.
        service
            .peer_apply_policy(&crate::peers::VolumeApply {
                resource: "alpha-vm-101-disk-0".into(),
                resource_file: file.clone(),
            })
            .await
            .unwrap();
        let written = backend.written();
        let last = &written.last().unwrap().1;
        assert!(last.contains("mock-secret"), "{last}");
        assert!(!last.contains("@@LUMEN-SECRET@@"));
        assert_eq!(backend.adjusted(), vec!["alpha-vm-101-disk-0"]);
    }

    #[tokio::test]
    async fn a_split_brain_resolution_discards_the_victim_first() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new().with_backend(backend.clone()));
        let service = service_with(backend, peers.clone(), &alpha_membership(), "split-brain");
        service
            .create_volume(create_request("vm-101-disk-0", 1 << 30))
            .await
            .unwrap();

        let refused = service
            .resolve_split_brain(
                "alpha",
                "vm-101-disk-0",
                "alpha-2",
                Acknowledgements::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(refused, DrbdError::Invalid(_)));

        service
            .resolve_split_brain(
                "alpha",
                "vm-101-disk-0",
                "alpha-2",
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap();
        let reconnects = peers.reconnects();
        assert_eq!(reconnects.len(), 2, "{reconnects:?}");
        assert_eq!(
            (reconnects[0].0.as_str(), reconnects[0].2),
            ("alpha-2", true),
            "the victim reconnects first, discarding"
        );
        assert_eq!(
            (reconnects[1].0.as_str(), reconnects[1].2),
            ("alpha-1", false)
        );

        // A stranger cannot be named the victim.
        let err = service
            .resolve_split_brain(
                "alpha",
                "vm-101-disk-0",
                "ghost",
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("holds no replica"), "{err}");
    }

    #[tokio::test]
    async fn a_resync_reports_its_percent_and_grows_a_rate_between_reads() {
        let backend = Arc::new(MockBackend::appliance());
        let peers = Arc::new(MockVolumePeers::new().with_backend(backend.clone()));
        let service = service_with(backend.clone(), peers, &alpha_membership(), "sync-rate");
        service
            .create_volume(create_request("vm-101-disk-0", 1 << 30))
            .await
            .unwrap();

        backend.register(syncing_resource(
            "alpha-vm-101-disk-0",
            "alpha-2",
            42.7,
            1_000_000,
        ));
        let first = service.volumes().await.unwrap();
        let volume = &first.clusters[0].volumes[0];
        assert_eq!(volume.health, VolumeHealth::Syncing);
        assert!((volume.sync_percent.unwrap() - 42.7).abs() < 0.001);
        assert!(volume.sync_rate_kib_s.is_none(), "no rate from one read");

        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        backend.register(syncing_resource(
            "alpha-vm-101-disk-0",
            "alpha-2",
            55.0,
            2_000_000,
        ));
        let second = service.volumes().await.unwrap();
        let volume = &second.clusters[0].volumes[0];
        let rate = volume.sync_rate_kib_s.expect("a second read has a rate");
        assert!(rate > 0, "{rate}");
    }
}
