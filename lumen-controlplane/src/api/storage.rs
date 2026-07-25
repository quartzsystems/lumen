//! Storage endpoints.
//!
//! The volume a machine's disk lives on is reached through
//! `/api/vms/{vmid}/disks` rather than from here, because a volume is created
//! *for a machine* and never on its own. Pools are the opposite: they are a
//! decision about the node's own disks, so they are created and destroyed from
//! here.
//!
//! Thin by design, and every route takes the [`Session`] extractor.

use std::sync::Arc;

use axum::body::Body as AxumBody;
use axum::extract::{Path, State};
use axum::Json;
use futures_util::StreamExt;

use lumen_zfs::service::{DevicesResponse, IsosResponse, PoolView, PoolsResponse, VolumesResponse};
use lumen_zfs::{Acknowledgements, IsoStoreView, PoolCreate};

use crate::api::request::{body, required_body, Body};
use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;

/// DELETE /api/storage/pools/{pool}.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DestroyPoolRequest {
    #[serde(default)]
    i_understand_this_may_lose_data: bool,
}

/// POST /api/storage/pools — the acknowledgement rides alongside the pool.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePoolRequest {
    #[serde(flatten)]
    pool: PoolCreate,
    /// Needed only when a chosen disk already has something on it. The
    /// validator says which, and says what is on it.
    #[serde(default)]
    i_understand_this_may_lose_data: bool,
}

/// GET /api/storage/pools — pools, grouped by node.
pub async fn pools(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<PoolsResponse>, ApiError> {
    Ok(Json(state.storage.pools().await?))
}

/// GET /api/storage/devices — every disk the node has, and what is on each.
///
/// The answer the create dialog fills its picker from. It is a separate route
/// from the pool listing because it is a different question — "what could a
/// pool be built on" rather than "what pools are there" — and because reading
/// `/sys/block` is work no page that only wants the pool table should pay for.
pub async fn devices(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<DevicesResponse>, ApiError> {
    Ok(Json(state.storage.block_devices().await?))
}

/// POST /api/storage/pools — build one.
///
/// The most destructive request this API accepts: it reformats every disk it is
/// given. Every check happens before anything runs, so a rejected request
/// leaves the node's disks exactly as they were.
pub async fn create_pool(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<PoolView>, ApiError> {
    let request: CreatePoolRequest = required_body(raw)?;
    Ok(Json(
        state
            .storage
            .create_pool(
                request.pool,
                Acknowledgements {
                    may_lose_data: request.i_understand_this_may_lose_data,
                },
            )
            .await?,
    ))
}

/// DELETE /api/storage/pools/{pool} — destroy one, and everything on it.
pub async fn destroy_pool(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(pool): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: DestroyPoolRequest = body(raw)?;
    state
        .storage
        .destroy_pool(
            &pool,
            Acknowledgements {
                may_lose_data: request.i_understand_this_may_lose_data,
            },
        )
        .await?;
    Ok(Json(serde_json::json!({ "pool": pool })))
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
