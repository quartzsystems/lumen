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

/// POST /api/storage/pool — start the LumenFS pool create. Validation
/// answers now with every problem; the build itself runs behind the 202,
/// on the pending feed.
pub async fn create_lumen_pool(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<
    (
        axum::http::StatusCode,
        Json<crate::pool_workflow::PoolProgress>,
    ),
    ApiError,
> {
    let request: crate::pool_workflow::LumenPoolCreate = required_body(raw)?;
    let progress = crate::pool_workflow::create_pool(&state, request).await?;
    Ok((axum::http::StatusCode::ACCEPTED, Json(progress)))
}

/// DELETE /api/storage/pool — destroy it: every brick wiped, every
/// drop-in removed, every control plane restarted out of it.
/// POST /api/storage/pool/members — grow the pool by one member: the
/// 2→3 scale-out's storage half, its disk choices the operator's own.
pub async fn grow_lumen_pool(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<
    (
        axum::http::StatusCode,
        Json<crate::pool_workflow::PoolProgress>,
    ),
    ApiError,
> {
    let request: crate::pool_workflow::LumenPoolGrow = required_body(raw)?;
    let progress = crate::pool_workflow::grow_pool(&state, request).await?;
    Ok((axum::http::StatusCode::ACCEPTED, Json(progress)))
}

pub async fn destroy_lumen_pool(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<
    (
        axum::http::StatusCode,
        Json<crate::pool_workflow::PoolProgress>,
    ),
    ApiError,
> {
    let request: DestroyPoolRequest = body(raw)?;
    let progress =
        crate::pool_workflow::destroy_pool(&state, request.i_understand_this_may_lose_data).await?;
    Ok((axum::http::StatusCode::ACCEPTED, Json(progress)))
}

/// GET /api/storage/pool/pending — the running (or last finished) pool
/// job. 404 when none has been started; after the coordinator's own
/// restart the feed is gone too, and the observed pool is the answer.
pub async fn lumen_pool_pending(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::pool_workflow::PoolProgress>, ApiError> {
    state
        .pool_job
        .snapshot()
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("No pool job has been started.".to_string()))
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

// --- the LumenFS pool --------------------------------------------------------

/// GET /api/storage/pool — the pooled storage on this node's cluster, as it
/// is right now.
///
/// Three honest shapes in one response. `pool: null, error: null` is the
/// standalone appliance (or a DRBD cluster): the console renders no section
/// at all. `pool: null, error: "..."` is a drop-in that exists but could not
/// be assembled — a broken deployment, shown as its own sentence rather than
/// hidden behind an empty page. And a present pool is the observed view:
/// every member's answer or its silence, every vdisk with its snapshots.
#[derive(Debug, serde::Serialize)]
pub struct PooledStorageResponse {
    pub pool: Option<PooledStorageView>,
    pub error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct PooledStorageView {
    /// The pool is named for its cluster, so this page and the machine
    /// pages call the same thing by the same name.
    pub name: String,
    #[serde(flatten)]
    pub state: lumen_pool::PoolState,
}

pub async fn pooled_storage(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<PooledStorageResponse>, ApiError> {
    Ok(Json(match &state.pool {
        crate::pool::PoolPresence::Absent => PooledStorageResponse {
            pool: None,
            error: None,
        },
        crate::pool::PoolPresence::Broken(why) => PooledStorageResponse {
            pool: None,
            error: Some(why.clone()),
        },
        crate::pool::PoolPresence::Present { service, .. } => PooledStorageResponse {
            pool: Some(PooledStorageView {
                name: service.name().to_string(),
                state: service.state().await,
            }),
            error: None,
        },
    }))
}

/// The routes below address a disk by the compute domain's name for it —
/// `vm-7-disk-3` — because the device path has slashes in it and the name is
/// the same fact in another form. Anything else is refused by name.
fn pooled_device(name: &str) -> Result<String, ApiError> {
    lumen_pool::DiskName::parse(name)
        .and_then(|disk| disk.vdisk())
        .map(lumen_pool::device_path)
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "{name} is not a pooled machine disk (vm-<vmid>-disk-<n>)"
            ))
        })
}

fn pool_service(state: &AppState) -> Result<&Arc<lumen_pool::PoolService>, ApiError> {
    state
        .pool
        .service()
        .ok_or_else(|| ApiError::Conflict("this node carries no LumenFS pool".into()))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolSnapshotRequest {
    /// The snapshot's id. The console passes Unix seconds so ids sort and
    /// display as the moments they are; the engine refuses a duplicate.
    snapshot: u64,
}

/// POST /api/storage/pool/disks/{name}/snapshots — take one. Replicated by
/// the engine: one member is told, both have it.
pub async fn snapshot_pooled_disk(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: PoolSnapshotRequest = required_body(raw)?;
    let device = pooled_device(&name)?;
    pool_service(&state)?
        .snapshot(&device, request.snapshot)
        .await?;
    Ok(Json(serde_json::json!({ "taken": true })))
}

/// DELETE /api/storage/pool/disks/{name}/snapshots/{snapshot} — drop one,
/// releasing whatever it alone was pinning.
pub async fn delete_pooled_snapshot(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((name, snapshot)): Path<(String, u64)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let device = pooled_device(&name)?;
    pool_service(&state)?
        .delete_snapshot(&device, snapshot)
        .await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolDiskDeleteRequest {
    #[serde(default)]
    i_understand_this_may_lose_data: bool,
}

/// DELETE /api/storage/pool/disks/{name} — reap an orphaned pooled
/// volume: one kept when its machine was deleted without a purge, which
/// until now no operator surface could remove (the replicated-volume
/// delete route only ever listed DRBD clusters — a recorded gap from the
/// phase-4 exit test). Refused while any defined machine still
/// references the device: detaching or deleting the machine is the
/// honest way to free a disk it still names.
pub async fn delete_pooled_disk(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: PoolDiskDeleteRequest = required_body(raw)?;
    if !request.i_understand_this_may_lose_data {
        return Err(ApiError::BadRequest(format!(
            "Deleting {name} discards its contents — confirm you understand data may be lost."
        )));
    }
    let device = pooled_device(&name)?;
    let vmid = lumen_pool::DiskName::parse(&name)
        .map(|disk| disk.vmid)
        .ok_or_else(|| ApiError::NotFound(format!("{name} is not a pooled machine disk")))?;
    if let Ok(machine) = state.virt.get(vmid as u32).await {
        if machine.disks.iter().any(|disk| disk.source == device) {
            return Err(ApiError::Conflict(format!(
                "\"{}\" still uses {name}. Detach the disk or delete the machine instead — \
                 this route reaps volumes nothing references.",
                machine.name
            )));
        }
    }
    use lumen_drbd::VmVolumes;
    pool_service(&state)?.destroy_disk(&device).await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolRollbackRequest {
    snapshot: u64,
    #[serde(default)]
    i_understand_this_may_lose_data: bool,
}

/// POST /api/storage/pool/disks/{name}/rollback — put the disk back to a
/// snapshot, discarding everything written since.
///
/// The service refuses while the disk is open anywhere in the pool — a
/// member serving it, or a member that cannot say — and this handler
/// refuses without the acknowledgement, because "discards everything
/// written since" is a sentence an operator must have read.
pub async fn rollback_pooled_disk(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: PoolRollbackRequest = required_body(raw)?;
    if !request.i_understand_this_may_lose_data {
        return Err(ApiError::Conflict(
            "rolling back discards everything written since the snapshot; acknowledge with \
             i_understand_this_may_lose_data"
                .into(),
        ));
    }
    let device = pooled_device(&name)?;
    pool_service(&state)?
        .rollback(&device, request.snapshot)
        .await?;
    Ok(Json(serde_json::json!({ "rolled_back": true })))
}
