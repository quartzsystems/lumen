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
