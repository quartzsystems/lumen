//! Networking endpoints.
//!
//! Thin by design: deserialize, call one `lumen_net::NetworkService` method,
//! serialize. No netlink, no D-Bus, and no validation logic lives here — that
//! is the whole point of the component split (see docs/networking.md).
//!
//! Every route takes the [`Session`] extractor, so an unauthenticated request
//! is a 401 before any handler body runs.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use lumen_net::model::{Bond, Bridge, LinkKind, Vlan};
use lumen_net::service::{
    ApplyResponse, BondPatch, BridgePatch, InterfacesResponse, LinkView, ManagementBridgeResponse,
    NicPatch, PendingResponse, VlanPatch,
};
use lumen_net::{Acknowledgements, NetworkDesiredState};

use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;

/// An optional JSON body. Mutating routes accept one so they can all carry the
/// same optional `node` field; the console sends a body only when it has
/// something to say.
type Body = Option<Json<Value>>;

/// Split the optional `node` field off a request body and deserialize the rest
/// into a domain type.
///
/// Node grouping is in the API from day one even though there is exactly one
/// node: the response shapes and the request fields must not change when
/// clustering lands, and the console renders its per-node layout now.
fn body<T: DeserializeOwned + Default>(raw: Body) -> Result<T, ApiError> {
    let Some(Json(mut value)) = raw else {
        return Ok(T::default());
    };
    check_node(&mut value)?;
    if value.as_object().is_some_and(|map| map.is_empty()) {
        return Ok(T::default());
    }
    serde_json::from_value(value).map_err(|err| ApiError::BadRequest(err.to_string()))
}

/// For routes whose body carries nothing but the optional `node` field. The
/// node still has to be checked — silently applying a request meant for
/// another node to this one is exactly the wrong failure.
fn node_only(raw: Body) -> Result<(), ApiError> {
    if let Some(Json(mut value)) = raw {
        check_node(&mut value)?;
    }
    Ok(())
}

/// Same as `body`, for bodies that must be present.
fn required_body<T: DeserializeOwned>(raw: Body) -> Result<T, ApiError> {
    let Some(Json(mut value)) = raw else {
        return Err(ApiError::BadRequest("A request body is required.".into()));
    };
    check_node(&mut value)?;
    serde_json::from_value(value).map_err(|err| ApiError::BadRequest(err.to_string()))
}

/// Remove `node` and reject anything that is not this appliance. Removing it
/// keeps the domain types `deny_unknown_fields`, which is what turns a typo in
/// a request into a 400 instead of an ignored setting.
fn check_node(value: &mut Value) -> Result<(), ApiError> {
    let Some(map) = value.as_object_mut() else {
        return Ok(());
    };
    let Some(node) = map.remove("node") else {
        return Ok(());
    };
    let Some(node) = node.as_str() else {
        return Err(ApiError::BadRequest("\"node\" must be a hostname.".into()));
    };
    let local = lumen_net::backend::nm::hostname();
    if node.is_empty() || node == local {
        return Ok(());
    }
    Err(ApiError::BadRequest(format!(
        "This appliance is not in a cluster, so changes cannot be sent to \"{node}\" — it only \
         manages \"{local}\"."
    )))
}

/// The apply request: the acknowledgement the validator demands before it will
/// move the management address off a reachable link.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyRequest {
    #[serde(default)]
    i_understand_this_may_disconnect_me: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtendRequest {
    /// Seconds to add to the confirm window.
    seconds: u32,
}

/// GET /api/network/interfaces — observed state, grouped by node.
pub async fn interfaces(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<InterfacesResponse>, ApiError> {
    Ok(Json(state.network.interfaces().await?))
}

/// GET /api/network/interfaces/:name — one link on the local node.
pub async fn interface(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<LinkView>, ApiError> {
    Ok(Json(state.network.interface(&name).await?))
}

/// GET /api/network/config — the committed desired state.
pub async fn config(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<NetworkDesiredState>, ApiError> {
    Ok(Json(state.network.config().await?))
}

/// GET /api/network/pending — staged delta, validation results, checkpoint.
pub async fn pending(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<PendingResponse>, ApiError> {
    Ok(Json(state.network.pending().await?))
}

/// DELETE /api/network/pending — discard everything staged.
pub async fn discard(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.network.discard().await?))
}

pub async fn create_bridge(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    let bridge: Bridge = required_body(raw)?;
    Ok(Json(state.network.create_bridge(bridge).await?))
}

pub async fn create_bond(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    let bond: Bond = required_body(raw)?;
    Ok(Json(state.network.create_bond(bond).await?))
}

pub async fn create_vlan(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    let vlan: Vlan = required_body(raw)?;
    Ok(Json(state.network.create_vlan(vlan).await?))
}

pub async fn update_bridge(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    let patch: BridgePatch = body(raw)?;
    Ok(Json(state.network.update_bridge(&name, patch).await?))
}

pub async fn update_bond(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    let patch: BondPatch = body(raw)?;
    Ok(Json(state.network.update_bond(&name, patch).await?))
}

pub async fn update_vlan(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    let patch: VlanPatch = body(raw)?;
    Ok(Json(state.network.update_vlan(&name, patch).await?))
}

pub async fn update_nic(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    let patch: NicPatch = body(raw)?;
    Ok(Json(state.network.update_nic(&name, patch).await?))
}

pub async fn delete_bridge(
    session: Session,
    state: State<Arc<AppState>>,
    path: Path<String>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    delete_link(session, state, path, raw, LinkKind::Bridge).await
}

pub async fn delete_bond(
    session: Session,
    state: State<Arc<AppState>>,
    path: Path<String>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    delete_link(session, state, path, raw, LinkKind::Bond).await
}

pub async fn delete_vlan(
    session: Session,
    state: State<Arc<AppState>>,
    path: Path<String>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    delete_link(session, state, path, raw, LinkKind::Vlan).await
}

async fn delete_link(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
    kind: LinkKind,
) -> Result<Json<PendingResponse>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.network.delete_link(&name, kind).await?))
}

/// POST /api/network/apply — validate, checkpoint, push.
pub async fn apply(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<ApplyResponse>, ApiError> {
    let request: ApplyRequest = body(raw)?;
    let ack = Acknowledgements {
        may_disconnect: request.i_understand_this_may_disconnect_me,
    };
    Ok(Json(state.network.apply(ack).await?))
}

/// POST /api/network/confirm — destroy the checkpoint; the change is now
/// permanent.
pub async fn confirm(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.network.confirm().await?))
}

/// POST /api/network/rollback — revert now rather than waiting out the window.
pub async fn rollback(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.network.rollback().await?))
}

/// POST /api/network/apply/extend — more time for a slow operator.
pub async fn extend(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<impl Serialize>, ApiError> {
    let request: ExtendRequest = required_body(raw)?;
    Ok(Json(state.network.extend(request.seconds).await?))
}

/// POST /api/network/management-bridge — convert nicN to brN + port.
pub async fn management_bridge(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<ManagementBridgeResponse>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.network.management_bridge().await?))
}
