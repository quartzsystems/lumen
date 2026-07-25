//! Virtual machine endpoints.
//!
//! Thin by design: deserialize, call one `lumen_virt::VirtService` method,
//! serialize. There is no hypervisor here, no domain document, no volume
//! handling, and no validation — that is the whole point of the component
//! split (see docs/compute.md).
//!
//! Every route takes the [`Session`] extractor, so an unauthenticated request
//! is a 401 before any handler body runs.

use std::sync::Arc;

use axum::body::Body as AxumBody;
use axum::extract::{Path, Query, State};
use axum::Json;

use lumen_virt::service::{
    DiskCreate, NicCreate, VmCreate, VmDeleteResponse, VmPatch, VmUpdateResponse, VmView,
    VmsResponse,
};
use lumen_virt::{Acknowledgements, CpuModels, OsCatalog, PushedFile, MAX_GUEST_FILE_BYTES};

use crate::api::request::{body, node_only, required_body, Body};
use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;

/// The acknowledgement the validator demands before it will take a running
/// machine down without warning it, or remove a disk's contents.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RiskyRequest {
    #[serde(default)]
    i_understand_this_may_lose_data: bool,
}

impl RiskyRequest {
    fn ack(&self) -> Acknowledgements {
        Acknowledgements {
            may_lose_data: self.i_understand_this_may_lose_data,
        }
    }
}

/// DELETE /api/vms/{vmid} and DELETE /api/vms/{vmid}/disks/{id}.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteRequest {
    /// Off by default: removing a machine must not destroy its data unless the
    /// caller asked for that in so many words.
    #[serde(default)]
    purge_disks: bool,
    #[serde(default)]
    i_understand_this_may_lose_data: bool,
}

impl DeleteRequest {
    fn ack(&self) -> Acknowledgements {
        Acknowledgements {
            may_lose_data: self.i_understand_this_may_lose_data,
        }
    }
}

/// GET /api/vms — every machine, grouped by node.
pub async fn list(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<VmsResponse>, ApiError> {
    Ok(Json(state.virt.list().await?))
}

/// GET /api/vms/next-id — the identifier a machine created now would get.
///
/// Advisory, not a reservation: two operators opening the dialog at the same
/// moment see the same number, and whichever creates second is allocated the
/// next one by the service. Offering it is what lets the console show the
/// identifier before anything is created, the way the rest of the form shows
/// its defaults.
pub async fn next_id(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(
        serde_json::json!({ "vmid": state.virt.next_vmid().await? }),
    ))
}

/// GET /api/vms/cpu-models — the processor models this node can run.
pub async fn cpu_models(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<CpuModels>, ApiError> {
    Ok(Json(state.virt.cpu_models().await?))
}

/// GET /api/vms/os-catalog — the guest operating systems this node knows.
pub async fn os_catalog(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<OsCatalog>, ApiError> {
    Ok(Json(state.virt.os_catalog().await?))
}

/// GET /api/vms/{vmid} — one machine, in full.
pub async fn get(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
) -> Result<Json<VmView>, ApiError> {
    Ok(Json(state.virt.get(vmid).await?))
}

/// POST /api/vms — define the machine and the volumes its disks live on.
pub async fn create(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    let request: VmCreate = required_body(raw)?;
    Ok(Json(state.virt.create(request).await?))
}

/// PATCH /api/vms/{vmid} — change the configuration, and report which changes
/// the running machine took and which are waiting for a restart.
pub async fn update(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    let patch: VmPatch = body(raw)?;
    Ok(Json(state.virt.update(vmid, patch).await?))
}

/// DELETE /api/vms/{vmid} — remove the definition, and the volumes only if
/// asked.
pub async fn delete(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmDeleteResponse>, ApiError> {
    let request: DeleteRequest = body(raw)?;
    Ok(Json(
        state
            .virt
            .delete(vmid, request.purge_disks, request.ack())
            .await?,
    ))
}

pub async fn start(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.virt.start(vmid).await?))
}

/// POST /api/vms/{vmid}/shutdown — ask the guest to shut down and let it
/// decide when.
pub async fn shutdown(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.virt.shutdown(vmid).await?))
}

/// POST /api/vms/{vmid}/stop — the equivalent of pulling the power, so it
/// needs the acknowledgement.
pub async fn stop(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    let request: RiskyRequest = body(raw)?;
    Ok(Json(state.virt.stop(vmid, request.ack()).await?))
}

pub async fn reboot(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.virt.reboot(vmid).await?))
}

/// POST /api/vms/{vmid}/reset — the reset button, so it needs the
/// acknowledgement too.
pub async fn reset(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    let request: RiskyRequest = body(raw)?;
    Ok(Json(state.virt.reset(vmid, request.ack()).await?))
}

/// POST /api/vms/{vmid}/disks — create the volume and attach it.
pub async fn attach_disk(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    let request: DiskCreate = required_body(raw)?;
    Ok(Json(state.virt.attach_disk(vmid, request).await?))
}

/// DELETE /api/vms/{vmid}/disks/{id} — detach it, and destroy the volume only
/// if asked.
pub async fn detach_disk(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((vmid, id)): Path<(u32, String)>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    let request: DeleteRequest = body(raw)?;
    Ok(Json(
        state
            .virt
            .detach_disk(vmid, &id, request.purge_disks, request.ack())
            .await?,
    ))
}

pub async fn attach_nic(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    let request: NicCreate = required_body(raw)?;
    Ok(Json(state.virt.attach_nic(vmid, request).await?))
}

pub async fn detach_nic(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((vmid, id)): Path<(u32, String)>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.virt.detach_nic(vmid, &id).await?))
}

/// Where a pushed file is going, inside the guest.
#[derive(Debug, serde::Deserialize)]
pub struct PushTarget {
    path: String,
}

/// PUT /api/vms/{vmid}/files?path=/root/thing — copy a file into a guest.
///
/// The body is the file itself, exactly as `upload_iso` takes an installation
/// image, and for the same reason: there is one field, its name is already in
/// the query, and a form encoding would put a parser between the socket and the
/// bytes for no benefit.
///
/// Unlike that one it is **buffered**, not streamed, and the route caps it at
/// [`MAX_GUEST_FILE_BYTES`]. The guest agent takes a file as a series of
/// base64-encoded messages rather than as a stream, so there is nothing to
/// stream into — see `lumen_virt::VirtService::push_file` for why that makes
/// this the wrong road for anything large, and what the right one is.
pub async fn push_file(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    Query(target): Query<PushTarget>,
    body: AxumBody,
) -> Result<Json<PushedFile>, ApiError> {
    let contents = axum::body::to_bytes(body, MAX_GUEST_FILE_BYTES)
        .await
        .map_err(|err| {
            ApiError::BadRequest(format!(
                "That file could not be read, or is larger than the {} MiB a guest agent will \
                 take: {err}",
                MAX_GUEST_FILE_BYTES / (1024 * 1024)
            ))
        })?;
    Ok(Json(
        state.virt.push_file(vmid, &target.path, &contents).await?,
    ))
}
