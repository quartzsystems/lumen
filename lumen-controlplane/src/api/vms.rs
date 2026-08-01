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
    CdromCreate, DiskCreate, NicCreate, VmCreate, VmDeleteResponse, VmMigrateResponse, VmPatch,
    VmUpdateResponse, VmView, VmsResponse,
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

/// The session's principal, `user@realm` — who a task is recorded against.
fn principal(session: &Session) -> String {
    format!("{}@{}", session.0.sub, session.0.realm)
}

/// Put one outcome in the task log before the handler answers. Refusals are
/// recorded too: who tried and was told no is as much a part of the machine's
/// history as what happened.
fn record<T>(
    state: &AppState,
    session: &Session,
    vmid: u32,
    action: &str,
    detail: impl Into<String>,
    result: &Result<T, lumen_virt::VirtError>,
) {
    state.tasks.record(
        vmid,
        action,
        detail.into(),
        principal(session),
        result.as_ref().err().map(|err| err.to_string()),
    );
}

/// Which fields a patch actually sets, for the task log's one sentence.
fn changed_fields(patch: &VmPatch) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if patch.name.is_some() {
        fields.push("name");
    }
    if patch.description.is_some() {
        fields.push("description");
    }
    if patch.vcpus.is_some() {
        fields.push("processors");
    }
    if patch.memory_mib.is_some() {
        fields.push("memory");
    }
    if patch.cpu_model.is_some() {
        fields.push("processor model");
    }
    if patch.topology.is_some() {
        fields.push("processor layout");
    }
    if patch.machine.is_some() {
        fields.push("machine type");
    }
    if patch.firmware.is_some() {
        fields.push("firmware");
    }
    if patch.video.is_some() {
        fields.push("graphics");
    }
    if patch.ha.is_some() {
        fields.push("high availability");
    }
    if patch.boot_order.is_some() {
        fields.push("boot order");
    }
    if patch.start_on_boot.is_some() {
        fields.push("start on boot");
    }
    if patch.guest_agent.is_some() {
        fields.push("guest agent");
    }
    if patch.tags.is_some() {
        fields.push("tags");
    }
    fields
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

/// Keep the cluster's copy of the machine's definition current — the HA
/// manager's restart inventory, replicated to co-members at define time
/// because libvirt on a dead node cannot be asked for it. Best-effort by
/// design: a peer that is down misses this push and is caught up by the
/// next define, and failing the operator's action over HA prep would make
/// the preparation more important than the machine.
pub(crate) async fn sync_definition(state: &AppState, vmid: u32) {
    match state.virt.definition(vmid).await {
        Ok(xml) => {
            if let Err(err) = state.cluster.replicate_definition(vmid, &xml).await {
                tracing::warn!(vmid, "the definition did not replicate: {err}");
            }
        }
        Err(err) => {
            tracing::warn!(vmid, "could not read the definition to replicate: {err}")
        }
    }
}

/// POST /api/vms — define the machine and the volumes its disks live on.
pub async fn create(
    session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    let request: VmCreate = required_body(raw)?;
    let requested = request.vmid;
    let detail = format!(
        "Define {}{}",
        request.name,
        if request.start { " and start it" } else { "" }
    );
    let result = state.virt.create(request).await;
    // A rejected create without a requested identifier belongs to no machine,
    // so there is nothing to file it under.
    if let Some(vmid) = result.as_ref().map(|vm| vm.vmid).ok().or(requested) {
        record(&state, &session, vmid, "create", detail, &result);
    }
    if let Ok(vm) = &result {
        sync_definition(&state, vm.vmid).await;
    }
    Ok(Json(result?))
}

/// PATCH /api/vms/{vmid} — change the configuration, and report which changes
/// the running machine took and which are waiting for a restart.
pub async fn update(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    let patch: VmPatch = body(raw)?;
    let changed = changed_fields(&patch);
    let detail = if changed.is_empty() {
        "Change nothing".to_string()
    } else {
        format!("Change {}", changed.join(", "))
    };
    let result = state.virt.update(vmid, patch).await;
    record(&state, &session, vmid, "update", detail, &result);
    if result.is_ok() {
        sync_definition(&state, vmid).await;
    }
    Ok(Json(result?))
}

/// DELETE /api/vms/{vmid} — remove the definition, and the volumes only if
/// asked.
pub async fn delete(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmDeleteResponse>, ApiError> {
    let request: DeleteRequest = body(raw)?;
    let detail = if request.purge_disks {
        "Remove the machine and destroy its volumes"
    } else {
        "Remove the machine"
    };
    let result = state
        .virt
        .delete(vmid, request.purge_disks, request.ack())
        .await;
    record(&state, &session, vmid, "delete", detail, &result);
    if result.is_ok() {
        // A stored definition for a machine that no longer exists is a
        // machine waiting to be wrongly resurrected.
        if let Err(err) = state.cluster.withdraw_definition(vmid).await {
            tracing::warn!(vmid, "the definition was not withdrawn: {err}");
        }
    }
    Ok(Json(result?))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrateRequest {
    /// The member to move to — one of the machine's disks' replica nodes.
    pub target: String,
}

/// POST /api/vms/{vmid}/migrate — live-migrate the machine to another member
/// of its disks' replica set. The transfer rides the cluster's Core network
/// — the dedicated storage link, whose address the membership record already
/// knows — and the two-primaries window around it is the service's guard.
pub async fn migrate(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmMigrateResponse>, ApiError> {
    let request: MigrateRequest = required_body(raw)?;
    let destination = core_uri_of(&state, &request.target)?;
    let detail = format!("Migrate to {}", request.target);
    let result = state
        .virt
        .migrate(vmid, &request.target, &destination)
        .await;
    record(&state, &session, vmid, "migrate", detail, &result);
    Ok(Json(result?))
}

/// The migration destination URI: the target's seat on its cluster's Core
/// network. Management carries the console; the machine's memory rides the
/// same dedicated link its disks already replicate over. Shared with the
/// maintenance drain, which moves machines the same way one at a time.
fn core_uri_of(state: &AppState, target: &str) -> Result<String, ApiError> {
    crate::maintenance::core_uri_of(state, target).map_err(ApiError::Conflict)
}

pub async fn start(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    node_only(raw)?;
    let result = state.virt.start(vmid).await;
    record(
        &state,
        &session,
        vmid,
        "start",
        "Start the machine",
        &result,
    );
    Ok(Json(result?))
}

/// POST /api/vms/{vmid}/shutdown — ask the guest to shut down and let it
/// decide when.
pub async fn shutdown(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    node_only(raw)?;
    let result = state.virt.shutdown(vmid).await;
    record(
        &state,
        &session,
        vmid,
        "shutdown",
        "Ask the guest to shut down",
        &result,
    );
    Ok(Json(result?))
}

/// POST /api/vms/{vmid}/stop — the equivalent of pulling the power, so it
/// needs the acknowledgement.
pub async fn stop(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    let request: RiskyRequest = body(raw)?;
    let result = state.virt.stop(vmid, request.ack()).await;
    record(
        &state,
        &session,
        vmid,
        "stop",
        "Stop the machine without asking the guest",
        &result,
    );
    Ok(Json(result?))
}

pub async fn reboot(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    node_only(raw)?;
    let result = state.virt.reboot(vmid).await;
    record(
        &state,
        &session,
        vmid,
        "reboot",
        "Ask the guest to reboot",
        &result,
    );
    Ok(Json(result?))
}

/// POST /api/vms/{vmid}/reset — the reset button, so it needs the
/// acknowledgement too.
pub async fn reset(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmView>, ApiError> {
    let request: RiskyRequest = body(raw)?;
    let result = state.virt.reset(vmid, request.ack()).await;
    record(
        &state,
        &session,
        vmid,
        "reset",
        "Reset the machine",
        &result,
    );
    Ok(Json(result?))
}

/// POST /api/vms/{vmid}/disks — create the volume and attach it.
pub async fn attach_disk(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    let request: DiskCreate = required_body(raw)?;
    let detail = if request.replicated {
        format!("Add a replicated {} GiB disk", request.size_gib)
    } else {
        format!("Add a {} GiB disk from {}", request.size_gib, request.pool)
    };
    let result = state.virt.attach_disk(vmid, request).await;
    record(&state, &session, vmid, "attach-disk", detail, &result);
    if result.is_ok() {
        sync_definition(&state, vmid).await;
    }
    Ok(Json(result?))
}

/// DELETE /api/vms/{vmid}/disks/{id} — detach it, and destroy the volume only
/// if asked.
pub async fn detach_disk(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path((vmid, id)): Path<(u32, String)>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    let request: DeleteRequest = body(raw)?;
    let detail = format!(
        "Detach disk {id}{}",
        if request.purge_disks {
            " and destroy its volume"
        } else {
            ""
        }
    );
    let result = state
        .virt
        .detach_disk(vmid, &id, request.purge_disks, request.ack())
        .await;
    record(&state, &session, vmid, "detach-disk", detail, &result);
    if result.is_ok() {
        sync_definition(&state, vmid).await;
    }
    Ok(Json(result?))
}

pub async fn attach_nic(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    let request: NicCreate = required_body(raw)?;
    let detail = format!("Add a network adapter on {}", request.bridge);
    let result = state.virt.attach_nic(vmid, request).await;
    record(&state, &session, vmid, "attach-nic", detail, &result);
    if result.is_ok() {
        sync_definition(&state, vmid).await;
    }
    Ok(Json(result?))
}

pub async fn detach_nic(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path((vmid, id)): Path<(u32, String)>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    node_only(raw)?;
    let detail = format!("Detach network adapter {id}");
    let result = state.virt.detach_nic(vmid, &id).await;
    record(&state, &session, vmid, "detach-nic", detail, &result);
    if result.is_ok() {
        sync_definition(&state, vmid).await;
    }
    Ok(Json(result?))
}

/// What a cdrom request is about, for the task log's one sentence.
fn media_words(request: &CdromCreate) -> String {
    match request.image.as_deref() {
        Some(image) => format!("with {image}"),
        None => "empty".to_string(),
    }
}

/// POST /api/vms/{vmid}/cdroms — add an optical drive, empty or loaded. An
/// absent body is an empty drive, which is a real thing to ask for.
pub async fn attach_cdrom(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    let request: CdromCreate = body(raw)?;
    let detail = format!("Add a CD/DVD drive, {}", media_words(&request));
    let result = state.virt.attach_cdrom(vmid, request).await;
    record(&state, &session, vmid, "attach-cdrom", detail, &result);
    if result.is_ok() {
        sync_definition(&state, vmid).await;
    }
    Ok(Json(result?))
}

/// PUT /api/vms/{vmid}/cdroms/{id} — what is in the tray. An absent body or
/// an empty one ejects.
pub async fn set_cdrom_media(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path((vmid, id)): Path<(u32, String)>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    let request: CdromCreate = body(raw)?;
    let detail = match request.image.as_deref() {
        Some(image) => format!("Put {image} in drive {id}"),
        None => format!("Eject drive {id}"),
    };
    let result = state.virt.set_cdrom_media(vmid, &id, request).await;
    record(&state, &session, vmid, "cdrom-media", detail, &result);
    if result.is_ok() {
        sync_definition(&state, vmid).await;
    }
    Ok(Json(result?))
}

/// DELETE /api/vms/{vmid}/cdroms/{id} — remove the drive. The image in its
/// tray belongs to the media library and is untouched.
pub async fn detach_cdrom(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path((vmid, id)): Path<(u32, String)>,
    raw: Body,
) -> Result<Json<VmUpdateResponse>, ApiError> {
    node_only(raw)?;
    let detail = format!("Remove CD/DVD drive {id}");
    let result = state.virt.detach_cdrom(vmid, &id).await;
    record(&state, &session, vmid, "detach-cdrom", detail, &result);
    if result.is_ok() {
        sync_definition(&state, vmid).await;
    }
    Ok(Json(result?))
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
    session: Session,
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
    let detail = format!("Copy a file into the guest at {}", target.path);
    let result = state.virt.push_file(vmid, &target.path, &contents).await;
    record(&state, &session, vmid, "push-file", detail, &result);
    Ok(Json(result?))
}

/// GET /api/vms/{vmid}/tasks — what has been done to this machine, newest
/// first. History is kept by identifier: a machine nothing has touched answers
/// with an empty list rather than a 404, and so does one that does not exist —
/// the log is not the place that knows which machines do.
pub async fn tasks(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(
        serde_json::json!({ "tasks": state.tasks.for_vm(vmid) }),
    ))
}

/// How many entries `/api/tasks` answers with when the caller does not say,
/// and the most it will answer with when they do. The default is a dashboard
/// panel's worth; the cap is there so a mistyped query cannot ask for the
/// whole log on every poll.
const DEFAULT_TASK_LIMIT: usize = 50;
const MAX_TASK_LIMIT: usize = 500;

/// GET /api/tasks — how many entries to answer with.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWindow {
    limit: Option<usize>,
}

/// GET /api/tasks — everything that has been done on this node, newest first.
///
/// The same log [`tasks`] reads, unfiltered: the dashboard shows activity
/// across every machine, and asking per machine would mean one request per
/// machine on every poll to rebuild an ordering the log already has.
pub async fn recent_tasks(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Query(window): Query<TaskWindow>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let limit = window
        .limit
        .unwrap_or(DEFAULT_TASK_LIMIT)
        .min(MAX_TASK_LIMIT);
    Ok(Json(
        serde_json::json!({ "tasks": state.tasks.recent(limit) }),
    ))
}
