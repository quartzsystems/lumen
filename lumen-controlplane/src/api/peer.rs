//! The peer surface: one control plane answering another.
//!
//! Every route here except `/api/peer/join` requires a peer ticket — a JWT
//! signed with the environment-shared secret, carrying `kind: peer` so a
//! browser session can never pass as one (and a peer ticket can never open a
//! console). The join route is the sole exception: its caller does not have
//! the secret yet, and the one-time token in its body is the authentication.
//!
//! Handlers stay thin, exactly as the operator-facing ones do: deserialize,
//! one service call, serialize.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::error::ApiError;
use crate::inventory::NodeInventory;
use crate::security::PeerSession;
use crate::AppState;
use lumen_cluster::{
    EnvironmentMembership, JoinGrant, JoinRequest, PreflightReport, PreparePayload, TeardownPayload,
};
use lumen_drbd::{VolumePrepare, VolumeResizeBacking, VolumeSnapshot, VolumeTeardown};

/// POST /api/peer/join — the issuer's half of an environment join. The
/// token in the body is the authentication; there is no ticket to hold yet.
pub async fn join(
    State(state): State<Arc<AppState>>,
    Json(request): Json<JoinRequest>,
) -> Result<Json<JoinGrant>, ApiError> {
    let secret = state.jwt_secret.read().expect("secret lock").clone();
    Ok(Json(state.cluster.grant_join(&request, &secret).await?))
}

/// POST /api/peer/membership — gossip. The peer pushes its record, we
/// reconcile and answer with the result.
pub async fn membership(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(remote): Json<EnvironmentMembership>,
) -> Result<Json<EnvironmentMembership>, ApiError> {
    Ok(Json(state.cluster.receive_membership(remote).await?))
}

/// POST /api/peer/preflight — could this node join a cluster right now?
pub async fn preflight(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<PreflightReport>, ApiError> {
    Ok(Json(state.cluster.peer_preflight().await?))
}

/// POST /api/peer/node/inventory — what this node has: its processors and
/// memory, its links, and its pools.
///
/// A read, on a surface that is otherwise all writes, because the console's
/// environment-wide tables need every member's answer and only each member
/// can give its own. One call rather than three: the three reads happen on
/// the same node at the same moment, and splitting them would only let the
/// three halves of one row disagree.
pub async fn inventory(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<NodeInventory>, ApiError> {
    Ok(Json(crate::inventory::local(&state).await))
}

/// POST /api/peer/storage/pool — build a pool here, on this node's own
/// disks.
///
/// The acknowledgement is not taken from the wire: the operator's consent was
/// given to the coordinator, which is where it belongs, and a peer route that
/// accepted "yes, erase them" from a body would be a second, quieter way to
/// reformat a node's disks.
pub async fn create_pool(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(request): Json<lumen_zfs::PoolCreate>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .storage
        .create_pool(
            request,
            lumen_zfs::Acknowledgements {
                may_lose_data: true,
            },
        )
        .await?;
    Ok(Json(serde_json::json!({ "built": true })))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WipeDiskRequest {
    /// The disk as this node names it — a kernel name, a `/dev` path, or the
    /// by-id path. The storage domain matches all three.
    pub disk: String,
}

/// POST /api/peer/storage/wipe — clear one disk here, on behalf of an
/// operator working from another member's console.
///
/// The acknowledgement is not taken from the wire, for the same reason
/// `create_pool` does not take it: consent was given to the console the
/// operator is looking at, and a peer route that accepted it from a body
/// would be a second, quieter way to clear a node's disks.
///
/// Every guard that matters is still this node's. A disk holding a pool, a
/// mount, or swap is refused here regardless of what the caller believes,
/// because this node is the only one that can see what is actually on it.
pub async fn wipe_disk(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(request): Json<WipeDiskRequest>,
) -> Result<Json<lumen_zfs::BlockDevice>, ApiError> {
    Ok(Json(
        state
            .storage
            .wipe_disk(
                &request.disk,
                lumen_zfs::Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await?,
    ))
}

/// POST /api/peer/cluster/prepare — realize this node's Core seat and write
/// the cluster configuration.
pub async fn prepare(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<PreparePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_prepare(&payload).await?;
    Ok(Json(serde_json::json!({ "prepared": true })))
}

/// POST /api/peer/network/bond — build a bond here, through this node's own
/// networking domain. The create wizard's Core-redundancy shortcut; the bond
/// that results is an ordinary link, owned and edited by Networking.
pub async fn create_bond(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(bond): Json<lumen_net::Bond>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_create_bond(&bond).await?;
    Ok(Json(serde_json::json!({ "bonded": true })))
}

/// POST /api/peer/network/bridge — build an External network's bridge here,
/// through this node's own networking domain. Like the bond above, what comes
/// out is an ordinary link that outlives the cluster.
pub async fn create_bridge(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(seat): Json<lumen_cluster::join::ExternalSeat>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_create_bridge(&seat).await?;
    Ok(Json(serde_json::json!({ "built": true })))
}

/// POST /api/peer/cluster/start — enable and start the cluster stack.
pub async fn start(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_start().await?;
    Ok(Json(serde_json::json!({ "started": true })))
}

/// POST /api/peer/cluster/teardown — put this node back exactly as it was.
pub async fn teardown(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<TeardownPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_teardown(&payload).await?;
    Ok(Json(serde_json::json!({ "torn_down": true })))
}

/// POST /api/peer/cluster/reconfigure — take a regenerated configuration
/// and reload corosync, live. The scale-out's reach into a running member.
pub async fn reconfigure(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<lumen_cluster::join::ReconfigurePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_reconfigure(&payload).await?;
    Ok(Json(serde_json::json!({ "reconfigured": true })))
}

// --- replicated volumes -----------------------------------------------------

/// The two volume verbs that take only a resource name.
#[derive(Debug, serde::Deserialize)]
pub struct ResourceRef {
    pub resource: String,
}

/// POST /api/peer/volume/prepare — carry a replica: zvol, resource file,
/// metadata, up. Whole or not at all.
pub async fn prepare_volume(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<VolumePrepare>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_prepare(&payload).await?;
    Ok(Json(serde_json::json!({ "prepared": true })))
}

/// POST /api/peer/volume/prime — skip the initial sync of a fresh volume.
pub async fn prime_volume(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ResourceRef>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_prime(&payload.resource).await?;
    Ok(Json(serde_json::json!({ "primed": true })))
}

/// POST /api/peer/volume/teardown — resource down, file gone, zvol
/// destroyed.
pub async fn teardown_volume(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<VolumeTeardown>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_teardown(&payload).await?;
    Ok(Json(serde_json::json!({ "torn_down": true })))
}

/// POST /api/peer/volume/resize-backing — grow this member's backing zvol.
pub async fn resize_volume_backing(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<VolumeResizeBacking>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_resize_backing(&payload).await?;
    Ok(Json(serde_json::json!({ "resized": true })))
}

/// POST /api/peer/volume/grow — let the resource take its grown backing.
pub async fn grow_volume(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ResourceRef>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_grow(&payload.resource).await?;
    Ok(Json(serde_json::json!({ "grown": true })))
}

#[derive(Debug, serde::Deserialize)]
pub struct TwoPrimariesRef {
    pub resource: String,
    pub allow: bool,
}

/// POST /api/peer/volume/two-primaries — the live-migration guard's reach
/// into this replica: open or close the window here.
pub async fn volume_two_primaries(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<TwoPrimariesRef>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .drbd
        .peer_two_primaries(&payload.resource, payload.allow)
        .await?;
    Ok(Json(serde_json::json!({ "adjusted": true })))
}

/// POST /api/peer/volume/snapshot — snapshot this member's backing zvol.
pub async fn snapshot_volume_backing(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<VolumeSnapshot>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_snapshot_backing(&payload).await?;
    Ok(Json(serde_json::json!({ "snapshotted": true })))
}

/// POST /api/peer/volume/rollback-backing — roll this member's zvol back.
pub async fn rollback_volume_backing(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<VolumeSnapshot>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_rollback_backing(&payload).await?;
    Ok(Json(serde_json::json!({ "rolled_back": true })))
}

/// POST /api/peer/volume/drop-snapshot — drop this member's snapshot.
pub async fn drop_volume_snapshot(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<VolumeSnapshot>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_drop_snapshot(&payload).await?;
    Ok(Json(serde_json::json!({ "dropped": true })))
}

/// POST /api/peer/volume/down — take the resource down here.
pub async fn down_volume(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ResourceRef>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_down(&payload.resource).await?;
    Ok(Json(serde_json::json!({ "down": true })))
}

/// POST /api/peer/volume/up — bring it back up.
pub async fn up_volume(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ResourceRef>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_up(&payload.resource).await?;
    Ok(Json(serde_json::json!({ "up": true })))
}

/// POST /api/peer/volume/invalidate-remote — this member is the truth;
/// everyone else resyncs from it.
pub async fn invalidate_remote_volume(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ResourceRef>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_invalidate_remote(&payload.resource).await?;
    Ok(Json(serde_json::json!({ "invalidated": true })))
}

#[derive(Debug, serde::Deserialize)]
pub struct ReconnectRef {
    pub resource: String,
    #[serde(default)]
    pub discard: bool,
}

/// POST /api/peer/volume/reconnect — reconnect, discarding this member's
/// own writes when it is the split-brain victim.
pub async fn reconnect_volume(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<ReconnectRef>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .drbd
        .peer_reconnect(&payload.resource, payload.discard)
        .await?;
    Ok(Json(serde_json::json!({ "reconnected": true })))
}

/// POST /api/peer/volume/apply-policy — substitute this node's own secret
/// into the re-rendered file, write it, and adjust the resource live.
pub async fn apply_volume_policy(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<lumen_drbd::VolumeApply>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.drbd.peer_apply_policy(&payload).await?;
    Ok(Json(serde_json::json!({ "applied": true })))
}

// --- replicated machine definitions -----------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct DefinitionRef {
    pub vmid: u32,
}

/// POST /api/peer/definition/store — keep a machine's definition, home node
/// included, so an HA restart can define it here after its node dies.
pub async fn store_definition(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<lumen_cluster::StoredDefinition>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_store_definition(&payload)?;
    Ok(Json(serde_json::json!({ "stored": true })))
}

/// POST /api/peer/definition/drop — forget one.
pub async fn drop_definition(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<DefinitionRef>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_drop_definition(payload.vmid)?;
    Ok(Json(serde_json::json!({ "dropped": true })))
}
