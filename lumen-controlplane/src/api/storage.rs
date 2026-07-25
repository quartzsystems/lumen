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

use axum::body::Body as AxumBody;
use axum::extract::{Path, State};
use axum::Json;
use futures_util::StreamExt;

use lumen_zfs::service::{IsosResponse, PoolsResponse, VolumesResponse};
use lumen_zfs::IsoStoreView;

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

/// GET /api/storage/iso — the media libraries and everything in them.
pub async fn isos(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<IsosResponse>, ApiError> {
    Ok(Json(state.storage.isos().await?))
}

/// POST /api/storage/iso/{pool} — make a pool's media library.
pub async fn create_iso_store(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(pool): Path<String>,
) -> Result<Json<IsoStoreView>, ApiError> {
    Ok(Json(state.storage.create_iso_store(&pool).await?))
}

/// PUT /api/storage/iso/{pool}/{name} — store an uploaded image.
///
/// The body is the file itself, streamed: an installation image is measured in
/// gigabytes, so it goes to disk a chunk at a time and is never buffered whole
/// in this process. That is also why it is not multipart — a form encoding
/// would put a parser between the socket and the file for no benefit, since
/// there is exactly one field and its name is already in the path.
///
/// A dropped connection leaves a partial file that is not visible as media and
/// that the next attempt replaces; see `lumen_zfs::iso`.
pub async fn upload_iso(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((pool, name)): Path<(String, String)>,
    body: AxumBody,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut upload = state.storage.begin_iso_upload(&pool, &name).await?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(err) => {
                upload.abort().await;
                return Err(ApiError::BadRequest(format!("The upload stopped: {err}")));
            }
        };
        if let Err(err) = upload.write(&chunk).await {
            upload.abort().await;
            return Err(err.into());
        }
    }
    let written = upload.finish().await?;
    Ok(Json(serde_json::json!({
        "storage": pool,
        "name": name,
        "size": written,
    })))
}

/// DELETE /api/storage/iso/{pool}/{name} — remove one image.
pub async fn delete_iso(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((pool, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.storage.delete_iso(&pool, &name).await?;
    Ok(Json(serde_json::json!({ "storage": pool, "name": name })))
}
