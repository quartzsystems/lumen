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

use lumen_drbd::{ReplicatedVolumesResponse, VolumeCreate, VolumeView};
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

// --- replicated volumes -----------------------------------------------------

/// GET /api/storage/replicated — every cluster's replicated volumes, grouped
/// by cluster, definitions joined with whatever this node's DRBD can see.
pub async fn replicated_volumes(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ReplicatedVolumesResponse>, ApiError> {
    Ok(Json(state.drbd.volumes().await?))
}

/// POST /api/storage/replicated — create a replicated volume: every member
/// prepared whole, the initial sync skipped, the record written last. The
/// answer is the volume as this node then sees it.
pub async fn create_replicated_volume(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<VolumeView>, ApiError> {
    let request: VolumeCreate = required_body(raw)?;
    Ok(Json(state.drbd.create_volume(request).await?))
}

/// DELETE /api/storage/replicated/{cluster}/{name} — destroy every replica,
/// then forget the volume. `i_understand_this_may_lose_data` required.
pub async fn destroy_replicated_volume(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((cluster, name)): Path<(String, String)>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: DestroyPoolRequest = body(raw)?;
    state
        .drbd
        .destroy_volume(
            &cluster,
            &name,
            lumen_cluster::Acknowledgements {
                may_lose_data: request.i_understand_this_may_lose_data,
            },
        )
        .await?;
    Ok(Json(serde_json::json!({ "destroyed": true })))
}

/// POST /api/storage/replicated/{cluster}/{name}/resize — grow the volume:
/// every backing zvol, then the resource, then the record. Grow only.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResizeVolumeRequest {
    pub size_bytes: u64,
}

pub async fn resize_replicated_volume(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((cluster, name)): Path<(String, String)>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: ResizeVolumeRequest = required_body(raw)?;
    state
        .drbd
        .resize_volume(&cluster, &name, request.size_bytes)
        .await?;
    Ok(Json(serde_json::json!({ "resized": true })))
}

/// GET /api/storage/replicated/{cluster}/{name}/snapshots — this node's own
/// replica's snapshots.
pub async fn volume_snapshots(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((cluster, name)): Path<(String, String)>,
) -> Result<Json<Vec<lumen_zfs::SnapshotInfo>>, ApiError> {
    Ok(Json(state.drbd.volume_snapshots(&cluster, &name).await?))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRequest {
    pub name: String,
}

/// POST /api/storage/replicated/{cluster}/{name}/snapshots — snapshot every
/// replica, or none.
pub async fn snapshot_volume(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((cluster, name)): Path<(String, String)>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: SnapshotRequest = required_body(raw)?;
    state
        .drbd
        .snapshot_volume(&cluster, &name, &request.name)
        .await?;
    Ok(Json(serde_json::json!({ "snapshotted": true })))
}

/// DELETE /api/storage/replicated/{cluster}/{name}/snapshots/{snapshot}.
pub async fn delete_volume_snapshot(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((cluster, name, snapshot)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .drbd
        .delete_snapshot(&cluster, &name, &snapshot)
        .await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackRequest {
    pub snapshot: String,
    /// The member whose snapshot becomes the one truth.
    pub source: String,
    #[serde(default)]
    pub i_understand_this_may_lose_data: bool,
}

/// POST /api/storage/replicated/{cluster}/{name}/rollback — the
/// transactional rollback: machine off, resource down everywhere, one
/// member rolled back, up everywhere, peers resync from the source.
pub async fn rollback_volume(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((cluster, name)): Path<(String, String)>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: RollbackRequest = required_body(raw)?;
    state
        .drbd
        .rollback_volume(
            &cluster,
            &name,
            &request.snapshot,
            &request.source,
            lumen_cluster::Acknowledgements {
                may_lose_data: request.i_understand_this_may_lose_data,
            },
        )
        .await?;
    Ok(Json(serde_json::json!({ "rolled_back": true })))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitBrainRequest {
    /// The member whose divergent writes are discarded.
    pub victim: String,
    #[serde(default)]
    pub i_understand_this_may_lose_data: bool,
}

/// POST /api/storage/replicated/{cluster}/{name}/resolve-split-brain — the
/// guided recovery: victim named, its writes discarded, every side
/// reconnected.
pub async fn resolve_split_brain(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((cluster, name)): Path<(String, String)>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: SplitBrainRequest = required_body(raw)?;
    state
        .drbd
        .resolve_split_brain(
            &cluster,
            &name,
            &request.victim,
            lumen_cluster::Acknowledgements {
                may_lose_data: request.i_understand_this_may_lose_data,
            },
        )
        .await?;
    Ok(Json(serde_json::json!({ "resolved": true })))
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
