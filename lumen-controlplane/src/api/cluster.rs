//! Thin HTTP handlers over `lumen_cluster::ClusterService`.
//!
//! Read-only at this stage: the environment and its clusters, grouped by
//! cluster and then by node. The workflows — environment join, cluster
//! create, fencing — land in later stages and will follow the same
//! deserialize → one service call → serialize discipline.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;

use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;
use lumen_cluster::{ClusterView, EnvironmentResponse};

/// GET /api/environment — the whole environment: every cluster, every node,
/// and the unassigned nodes, in one answer. A node that never joined an
/// environment answers with itself as the one unassigned node.
pub async fn environment(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<EnvironmentResponse>, ApiError> {
    Ok(Json(state.cluster.environment().await?))
}

/// GET /api/environment/clusters/{name} — one cluster, the same view the
/// environment answer carries.
pub async fn cluster(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<ClusterView>, ApiError> {
    Ok(Json(state.cluster.cluster(&name).await?))
}
