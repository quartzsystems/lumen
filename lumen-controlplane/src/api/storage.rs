//! Storage endpoints.
//!
//! Read-only at this stage. The one write the storage domain has — the volume
//! a machine's disk lives on — is reached through `/api/vms/{vmid}/disks`,
//! because a volume is created for a machine and never on its own. Pool
//! creation, import, and destroy have no endpoint at all yet; see
//! docs/compute.md for why they wait for a privileged executor.
//!
//! Thin by design, and every route takes the [`Session`] extractor.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;

use lumen_zfs::service::{PoolsResponse, VolumesResponse};

use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;

/// GET /api/storage/pools — pools, grouped by node.
pub async fn pools(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<PoolsResponse>, ApiError> {
    Ok(Json(state.storage.pools().await?))
}

/// GET /api/storage/pools/{pool}/volumes — datasets and volumes under a pool.
pub async fn volumes(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(pool): Path<String>,
) -> Result<Json<VolumesResponse>, ApiError> {
    Ok(Json(state.storage.volumes(&pool).await?))
}
