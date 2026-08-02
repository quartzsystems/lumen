//! Virtual machine endpoints.
//!
//! Thin by design: deserialize, call one `lumen_virt::VirtService` method,
//! serialize. There is no hypervisor here, no domain document, no volume
//! handling, and no validation — that is the whole point of the component
//! split (see docs/compute.md).
//!
//! Every route takes the [`Session`] extractor, so an unauthenticated request
//! is a 401 before any handler body runs.
//!
//! ## A machine is wherever it is
//!
//! These routes are addressed by machine, not by node, and they mean the same
//! thing from every console in the environment. A request for a machine this
//! node does not have is resolved to the member that does and relayed there —
//! the same forwarding `api/network.rs` does for links, through the same kind
//! of closed verb enum ([`crate::inventory::VmVerb`]), for a sharper reason: a
//! link is on the node an operator wants to configure, but a *machine* is
//! wherever it last migrated to, and an appliance whose console can only start
//! the machines that happen to share a node with it makes an operator learn
//! the layout before they can use it.
//!
//! Every guard stays with the hypervisor that has the machine. What crosses
//! the wire is the instruction and the operator's name; whether the machine
//! may start, whether the disk may go, whether the guest may be cut off are
//! all still decided by the node that would have to do it.
//!
//! The one thing not forwarded is the console viewer. A VNC stream is a socket
//! on the node holding the machine rather than a request and an answer, and
//! relaying one would mean a second console protocol — see `api/console.rs`.

use std::sync::Arc;

use axum::body::Body as AxumBody;
use axum::extract::{Path, Query, State};
use axum::Json;

use lumen_virt::service::{CdromCreate, DiskCreate, NicCreate, VmCreate, VmPatch};
use lumen_virt::{Acknowledgements, CpuModels, OsCatalog, PushedFile, MAX_GUEST_FILE_BYTES};

use crate::api::request::{routed_body, routed_node_only, routed_required_body, Body};
use crate::error::ApiError;
use crate::inventory::VmVerb;
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

/// The session's principal, `user@realm` — who a task is recorded against.
fn principal(session: &Session) -> String {
    format!("{}@{}", session.0.sub, session.0.realm)
}

/// Put one outcome in the task log before the handler answers. Refusals are
/// recorded too: who tried and was told no is as much a part of the machine's
/// history as what happened.
///
/// `by` rather than the session, because a relayed verb has no session on the
/// node that runs it — it has the operator's principal, carried across the
/// wire so the machine's history names the person and not only the console
/// that relayed them.
fn record<T>(
    state: &AppState,
    by: &str,
    vmid: u32,
    action: &str,
    detail: impl Into<String>,
    result: &Result<T, lumen_virt::VirtError>,
) {
    state.tasks.record(
        vmid,
        action,
        detail.into(),
        by.to_string(),
        result.as_ref().err().map(|err| err.to_string()),
    );
}

/// Everything a verb answers with, as JSON.
///
/// Untyped on purpose, and the same choice `api/network.rs` made: a relayed
/// answer is the *target's* answer, serialized by the node that did the work
/// and passed through here untouched. Typing it again on this side would only
/// create a second place for the shape to be wrong.
fn out<T: serde::Serialize>(value: T) -> Result<serde_json::Value, ApiError> {
    serde_json::to_value(value).map_err(|err| ApiError::Internal(anyhow::anyhow!("{err}")))
}

/// Run one verb against this node's own machines.
///
/// The one definition of what each verb does: the operator-facing handlers
/// below and the peer relay in `api/peer.rs` both end here, which is what
/// makes a forwarded act indistinguishable from a local one — same guards,
/// same log entry, same answer.
pub async fn run_verb(
    state: &Arc<AppState>,
    by: &str,
    verb: VmVerb,
) -> Result<serde_json::Value, ApiError> {
    match verb {
        VmVerb::Get { vmid } => out(state.virt.get(vmid).await?),
        VmVerb::Tasks { vmid } => Ok(serde_json::json!({ "tasks": state.tasks.for_vm(vmid) })),

        VmVerb::Create(request) => {
            let request = *request;
            let requested = request.vmid;
            let detail = format!(
                "Define {}{}",
                request.name,
                if request.start { " and start it" } else { "" }
            );
            let result = state.virt.create(request).await;
            // A rejected create without a requested identifier belongs to no
            // machine, so there is nothing to file it under.
            if let Some(vmid) = result.as_ref().map(|vm| vm.vmid).ok().or(requested) {
                record(state, by, vmid, "create", detail, &result);
            }
            if let Ok(vm) = &result {
                sync_definition(state, vm.vmid).await;
            }
            out(result?)
        }

        VmVerb::Update { vmid, patch } => {
            let patch = *patch;
            let changed = changed_fields(&patch);
            let detail = if changed.is_empty() {
                "Change nothing".to_string()
            } else {
                format!("Change {}", changed.join(", "))
            };
            let result = state.virt.update(vmid, patch).await;
            record(state, by, vmid, "update", detail, &result);
            if result.is_ok() {
                sync_definition(state, vmid).await;
            }
            out(result?)
        }

        VmVerb::Delete {
            vmid,
            purge_disks,
            may_lose_data,
        } => {
            let detail = if purge_disks {
                "Remove the machine and destroy its volumes"
            } else {
                "Remove the machine"
            };
            let result = state
                .virt
                .delete(vmid, purge_disks, Acknowledgements { may_lose_data })
                .await;
            record(state, by, vmid, "delete", detail, &result);
            if result.is_ok() {
                // A stored definition for a machine that no longer exists is a
                // machine waiting to be wrongly resurrected.
                if let Err(err) = state.cluster.withdraw_definition(vmid).await {
                    tracing::warn!(vmid, "the definition was not withdrawn: {err}");
                }
            }
            out(result?)
        }

        VmVerb::Start { vmid } => {
            let result = state.virt.start(vmid).await;
            record(state, by, vmid, "start", "Start the machine", &result);
            out(result?)
        }

        VmVerb::Shutdown { vmid } => {
            let result = state.virt.shutdown(vmid).await;
            record(
                state,
                by,
                vmid,
                "shutdown",
                "Ask the guest to shut down",
                &result,
            );
            out(result?)
        }

        VmVerb::Stop {
            vmid,
            may_lose_data,
        } => {
            let result = state
                .virt
                .stop(vmid, Acknowledgements { may_lose_data })
                .await;
            record(
                state,
                by,
                vmid,
                "stop",
                "Stop the machine without asking the guest",
                &result,
            );
            out(result?)
        }

        VmVerb::Reboot { vmid } => {
            let result = state.virt.reboot(vmid).await;
            record(
                state,
                by,
                vmid,
                "reboot",
                "Ask the guest to reboot",
                &result,
            );
            out(result?)
        }

        VmVerb::Reset {
            vmid,
            may_lose_data,
        } => {
            let result = state
                .virt
                .reset(vmid, Acknowledgements { may_lose_data })
                .await;
            record(state, by, vmid, "reset", "Reset the machine", &result);
            out(result?)
        }

        VmVerb::Migrate { vmid, target } => {
            let destination = core_uri_of(state, &target)?;
            let detail = format!("Migrate to {target}");
            let result = state.virt.migrate(vmid, &target, &destination).await;
            record(state, by, vmid, "migrate", detail, &result);
            out(result?)
        }

        VmVerb::AttachDisk { vmid, disk } => {
            let disk = *disk;
            let detail = if disk.replicated {
                format!("Add a replicated {} GiB disk", disk.size_gib)
            } else {
                format!("Add a {} GiB disk from {}", disk.size_gib, disk.pool)
            };
            let result = state.virt.attach_disk(vmid, disk).await;
            record(state, by, vmid, "attach-disk", detail, &result);
            if result.is_ok() {
                sync_definition(state, vmid).await;
            }
            out(result?)
        }

        VmVerb::DetachDisk {
            vmid,
            id,
            purge_disks,
            may_lose_data,
        } => {
            let detail = format!(
                "Detach disk {id}{}",
                if purge_disks {
                    " and destroy its volume"
                } else {
                    ""
                }
            );
            let result = state
                .virt
                .detach_disk(vmid, &id, purge_disks, Acknowledgements { may_lose_data })
                .await;
            record(state, by, vmid, "detach-disk", detail, &result);
            if result.is_ok() {
                sync_definition(state, vmid).await;
            }
            out(result?)
        }

        VmVerb::AttachNic { vmid, nic } => {
            let nic = *nic;
            let detail = format!("Add a network adapter on {}", nic.bridge);
            let result = state.virt.attach_nic(vmid, nic).await;
            record(state, by, vmid, "attach-nic", detail, &result);
            if result.is_ok() {
                sync_definition(state, vmid).await;
            }
            out(result?)
        }

        VmVerb::DetachNic { vmid, id } => {
            let detail = format!("Detach network adapter {id}");
            let result = state.virt.detach_nic(vmid, &id).await;
            record(state, by, vmid, "detach-nic", detail, &result);
            if result.is_ok() {
                sync_definition(state, vmid).await;
            }
            out(result?)
        }

        VmVerb::AttachCdrom { vmid, cdrom } => {
            let cdrom = *cdrom;
            let detail = format!("Add a CD/DVD drive, {}", media_words(&cdrom));
            let result = state.virt.attach_cdrom(vmid, cdrom).await;
            record(state, by, vmid, "attach-cdrom", detail, &result);
            if result.is_ok() {
                sync_definition(state, vmid).await;
            }
            out(result?)
        }

        VmVerb::SetCdromMedia { vmid, id, media } => {
            let media = *media;
            let detail = match media.image.as_deref() {
                Some(image) => format!("Put {image} in drive {id}"),
                None => format!("Eject drive {id}"),
            };
            let result = state.virt.set_cdrom_media(vmid, &id, media).await;
            record(state, by, vmid, "cdrom-media", detail, &result);
            if result.is_ok() {
                sync_definition(state, vmid).await;
            }
            out(result?)
        }

        VmVerb::DetachCdrom { vmid, id } => {
            let detail = format!("Remove CD/DVD drive {id}");
            let result = state.virt.detach_cdrom(vmid, &id).await;
            record(state, by, vmid, "detach-cdrom", detail, &result);
            if result.is_ok() {
                sync_definition(state, vmid).await;
            }
            out(result?)
        }
    }
}

/// Run the verb where the machine is: here, or on the member that has it.
///
/// `target` is the optional `node` in the request body, honored when the
/// caller knows. When they do not — which is the ordinary case, because the
/// console addresses machines by identifier — the owner is looked up, this
/// node first and for free. A machine nothing has resolves to local, so the
/// refusal an operator sees is the compute domain's own "no machine with
/// identifier 42" rather than a routing error about members.
async fn dispatch(
    state: &Arc<AppState>,
    session: &Session,
    target: Option<String>,
    verb: VmVerb,
) -> Result<Json<serde_json::Value>, ApiError> {
    let by = principal(session);
    let local = state.cluster.node().to_string();

    let member = match target.filter(|node| *node != local) {
        Some(node) => {
            let members = state.cluster.environment_nodes()?;
            members
                .into_iter()
                .find(|member| member.name == node)
                .ok_or_else(|| {
                    ApiError::BadRequest(format!(
                        "\"{node}\" is not a member of this environment, so its machines cannot \
                         be reached from here."
                    ))
                })
                .map(Some)?
        }
        // A create names no machine, so there is nothing to resolve: it lands
        // here unless the caller named somewhere else.
        None => match verb.vmid() {
            Some(vmid) => crate::cluster_vms::owner_of(state, vmid).await,
            None => None,
        },
    };

    match member {
        None => Ok(Json(run_verb(state, &by, verb).await?)),
        Some(member) => Ok(Json(state.peers.vm_verb(&member, &verb, &by).await?)),
    }
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

/// GET /api/vms — every machine in the environment, grouped by node.
///
/// Grouped by node as it always was, with the difference that there can now be
/// more than one group. A member that could not be asked is named in
/// `unreachable` rather than dropped: a list that is quietly short is one an
/// operator reads as "that machine is gone".
pub async fn list(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Json<crate::cluster_vms::EnvironmentVms> {
    Json(crate::cluster_vms::environment(&state).await)
}

/// GET /api/vms/next-id — the identifier a machine created now would get.
///
/// Free **across the environment**, not merely on this node: once a console
/// shows every member's machines, two nodes allocating the same number would
/// put two machines with one identifier in one table — and a machine that can
/// never migrate to the node holding its twin.
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
        serde_json::json!({ "vmid": crate::cluster_vms::next_vmid(&state).await? }),
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

/// The `?node=` a read takes to ask a named member rather than resolving the
/// machine itself. Absent — the ordinary case — the machine is found.
#[derive(Debug, Default, serde::Deserialize)]
pub struct NodeQuery {
    #[serde(default)]
    node: Option<String>,
}

impl NodeQuery {
    fn target(self) -> Option<String> {
        self.node.filter(|node| !node.is_empty())
    }
}

/// GET /api/vms/{vmid} — one machine, in full, wherever it is.
pub async fn get(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    Query(query): Query<NodeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    dispatch(&state, &session, query.target(), VmVerb::Get { vmid }).await
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
///
/// `node` in the body chooses the member to define it on; without one it is
/// defined here. This is the one verb with no machine to resolve by, so it is
/// also the one where naming the node is the only way to mean another member.
pub async fn create(
    session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, VmCreate) = routed_required_body(raw)?;
    dispatch(&state, &session, target, VmVerb::Create(Box::new(request))).await
}

/// PATCH /api/vms/{vmid} — change the configuration, and report which changes
/// the running machine took and which are waiting for a restart.
pub async fn update(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, patch): (_, VmPatch) = routed_body(raw)?;
    dispatch(
        &state,
        &session,
        target,
        VmVerb::Update {
            vmid,
            patch: Box::new(patch),
        },
    )
    .await
}

/// DELETE /api/vms/{vmid} — remove the definition, and the volumes only if
/// asked.
pub async fn delete(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, DeleteRequest) = routed_body(raw)?;
    dispatch(
        &state,
        &session,
        target,
        VmVerb::Delete {
            vmid,
            purge_disks: request.purge_disks,
            may_lose_data: request.i_understand_this_may_lose_data,
        },
    )
    .await
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
///
/// Relayed to the node that *has* the machine, which is the only node that can
/// hand it over. `target` is where it is going and is not a routing field: a
/// migration asked for from a third console still runs between the two members
/// it is about.
pub async fn migrate(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (routed, request): (_, MigrateRequest) = routed_required_body(raw)?;
    dispatch(
        &state,
        &session,
        routed,
        VmVerb::Migrate {
            vmid,
            target: request.target,
        },
    )
    .await
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
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = routed_node_only(raw)?;
    dispatch(&state, &session, target, VmVerb::Start { vmid }).await
}

/// POST /api/vms/{vmid}/shutdown — ask the guest to shut down and let it
/// decide when.
pub async fn shutdown(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = routed_node_only(raw)?;
    dispatch(&state, &session, target, VmVerb::Shutdown { vmid }).await
}

/// POST /api/vms/{vmid}/stop — the equivalent of pulling the power, so it
/// needs the acknowledgement.
pub async fn stop(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, RiskyRequest) = routed_body(raw)?;
    dispatch(
        &state,
        &session,
        target,
        VmVerb::Stop {
            vmid,
            may_lose_data: request.i_understand_this_may_lose_data,
        },
    )
    .await
}

pub async fn reboot(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = routed_node_only(raw)?;
    dispatch(&state, &session, target, VmVerb::Reboot { vmid }).await
}

/// POST /api/vms/{vmid}/reset — the reset button, so it needs the
/// acknowledgement too.
pub async fn reset(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, RiskyRequest) = routed_body(raw)?;
    dispatch(
        &state,
        &session,
        target,
        VmVerb::Reset {
            vmid,
            may_lose_data: request.i_understand_this_may_lose_data,
        },
    )
    .await
}

/// POST /api/vms/{vmid}/disks — create the volume and attach it.
pub async fn attach_disk(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, DiskCreate) = routed_required_body(raw)?;
    dispatch(
        &state,
        &session,
        target,
        VmVerb::AttachDisk {
            vmid,
            disk: Box::new(request),
        },
    )
    .await
}

/// DELETE /api/vms/{vmid}/disks/{id} — detach it, and destroy the volume only
/// if asked.
pub async fn detach_disk(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path((vmid, id)): Path<(u32, String)>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, DeleteRequest) = routed_body(raw)?;
    dispatch(
        &state,
        &session,
        target,
        VmVerb::DetachDisk {
            vmid,
            id,
            purge_disks: request.purge_disks,
            may_lose_data: request.i_understand_this_may_lose_data,
        },
    )
    .await
}

pub async fn attach_nic(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, NicCreate) = routed_required_body(raw)?;
    dispatch(
        &state,
        &session,
        target,
        VmVerb::AttachNic {
            vmid,
            nic: Box::new(request),
        },
    )
    .await
}

pub async fn detach_nic(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path((vmid, id)): Path<(u32, String)>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = routed_node_only(raw)?;
    dispatch(&state, &session, target, VmVerb::DetachNic { vmid, id }).await
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
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, CdromCreate) = routed_body(raw)?;
    dispatch(
        &state,
        &session,
        target,
        VmVerb::AttachCdrom {
            vmid,
            cdrom: Box::new(request),
        },
    )
    .await
}

/// PUT /api/vms/{vmid}/cdroms/{id} — what is in the tray. An absent body or
/// an empty one ejects.
pub async fn set_cdrom_media(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path((vmid, id)): Path<(u32, String)>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, CdromCreate) = routed_body(raw)?;
    dispatch(
        &state,
        &session,
        target,
        VmVerb::SetCdromMedia {
            vmid,
            id,
            media: Box::new(request),
        },
    )
    .await
}

/// DELETE /api/vms/{vmid}/cdroms/{id} — remove the drive. The image in its
/// tray belongs to the media library and is untouched.
pub async fn detach_cdrom(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path((vmid, id)): Path<(u32, String)>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = routed_node_only(raw)?;
    dispatch(&state, &session, target, VmVerb::DetachCdrom { vmid, id }).await
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
///
/// The one write on this file that is **not** relayed to the machine's node.
/// The body is bytes rather than a verb, and putting a megabyte of base64
/// inside a JSON envelope to cross the cluster — on the road already described
/// as the wrong one for anything large — would be making that worse on
/// purpose. A machine on another member is refused here, by name, so the
/// operator is told where to go rather than left with an empty guest.
pub async fn push_file(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    Query(target): Query<PushTarget>,
    body: AxumBody,
) -> Result<Json<PushedFile>, ApiError> {
    if let Some(owner) = crate::cluster_vms::owner_of(&state, vmid).await {
        return Err(ApiError::Conflict(format!(
            "Machine {vmid} is on \"{}\", and a file for its guest goes through that node's own \
             console — the transfer is buffered whole, and relaying it across the cluster would \
             copy it twice for no benefit.",
            owner.name
        )));
    }
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
    record(
        &state,
        &principal(&session),
        vmid,
        "push-file",
        detail,
        &result,
    );
    Ok(Json(result?))
}

/// GET /api/vms/{vmid}/tasks — what has been done to this machine, newest
/// first. History is kept by identifier: a machine nothing has touched answers
/// with an empty list rather than a 404, and so does one that does not exist —
/// the log is not the place that knows which machines do.
pub async fn tasks(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(vmid): Path<u32>,
    Query(query): Query<NodeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Resolved like every other request about a machine: a machine's history
    // is kept by the node that has been doing things to it, so asking here for
    // one that lives elsewhere would answer with an empty list — which reads
    // as "nothing has ever happened to it" rather than as "ask its node".
    dispatch(&state, &session, query.target(), VmVerb::Tasks { vmid }).await
}

/// How many entries `/api/tasks` answers with when the caller does not say,
/// and the most it will answer with when they do. The default is a dashboard
/// panel's worth; the cap is there so a mistyped query cannot ask for the
/// whole log on every poll.
pub(crate) const DEFAULT_TASK_LIMIT: usize = 50;
pub(crate) const MAX_TASK_LIMIT: usize = 500;

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

/// GET /api/environment/tasks — the same window, from every member at once.
///
/// The Logs page. Kept beside its node-local twin rather than in the cluster
/// module, because it is the same log answering the same question — the only
/// difference is how many nodes are asked. See src/cluster_tasks.rs for why
/// the merge into one ordering is left to the caller.
pub async fn environment_tasks(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Query(window): Query<TaskWindow>,
) -> Json<crate::cluster_tasks::EnvironmentTasks> {
    let limit = window
        .limit
        .unwrap_or(DEFAULT_TASK_LIMIT)
        .min(MAX_TASK_LIMIT);
    Json(crate::cluster_tasks::environment(&state, limit).await)
}
