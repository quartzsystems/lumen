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
use crate::security::PeerSession;
use crate::AppState;
use lumen_cluster::{
    EnvironmentMembership, JoinGrant, JoinRequest, PreflightReport, PreparePayload, TeardownPayload,
};
use lumen_drbd::{VolumePrepare, VolumeResizeBacking, VolumeTeardown};

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
