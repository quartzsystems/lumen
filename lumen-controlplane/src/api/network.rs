//! Networking endpoints.
//!
//! Thin by design: deserialize, call one `lumen_net::NetworkService` method,
//! serialize. No netlink, no D-Bus, and no validation logic lives here â€” that
//! is the whole point of the component split (see docs/networking.md).
//!
//! Every route takes the [`Session`] extractor, so an unauthenticated request
//! is a 401 before any handler body runs.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use lumen_net::model::{Bond, Bridge, LinkKind, Vlan};
use lumen_net::service::{
    ApplyResponse, BondPatch, BridgePatch, InterfacesResponse, LinkView, ManagementBridgeResponse,
    NicPatch, PendingResponse, VlanPatch,
};
use lumen_net::{Acknowledgements, NetworkDesiredState};

use crate::api::request::{body, node_only, required_body, Body};
use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;

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

/// GET /api/network/interfaces â€” observed state, grouped by node.
pub async fn interfaces(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<InterfacesResponse>, ApiError> {
    Ok(Json(state.network.interfaces().await?))
}

/// GET /api/network/interfaces/:name â€” one link on the local node.
pub async fn interface(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<LinkView>, ApiError> {
    Ok(Json(state.network.interface(&name).await?))
}

/// GET /api/network/nics/pins â€” the names that have lost their hardware,
/// and the adapters nothing has claimed.
///
/// Its own route rather than a field on the interfaces view, because it
/// describes what the node's *names* are pinned to rather than what the
/// links are doing: an orphaned pin has no link to hang off, which is
/// precisely the problem it reports.
pub async fn nic_pins(
    _session: Session,
    State(_state): State<Arc<AppState>>,
) -> Result<Json<lumen_net::pins::PinReport>, ApiError> {
    let roots = lumen_net::pins::PinRoots::default();
    Ok(Json(
        tokio::task::spawn_blocking(move || lumen_net::pins::report(&roots))
            .await
            .map_err(|err| ApiError::Internal(anyhow::anyhow!("{err}")))?,
    ))
}

/// What a card adoption names: the orphaned slot, and the adapter to put
/// in it.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptRequest {
    slot: u32,
    mac: String,
}

/// POST /api/network/nics/adopt â€” give an orphaned name to a new card.
///
/// The one repair a replaced adapter needs, and deliberately manual: the
/// appliance cannot know which port of a new card carries the network the
/// old one did, and guessing moves storage replication or a cluster ring
/// onto whatever enumerated first. See `lumen_net::pins`.
pub async fn adopt_nic(
    _session: Session,
    State(_state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: AdoptRequest = required_body(raw)?;
    let outcome = tokio::task::spawn_blocking(move || {
        let roots = lumen_net::pins::PinRoots::default();
        let device = lumen_net::pins::adopt(&roots, request.slot, &request.mac)?;
        // The pin alone takes effect at the next boot; renaming now is what
        // makes the profiles above it work today. A rename that fails is
        // reported, not hidden â€” the pin is written either way, so the next
        // boot repairs what this could not.
        let renamed = lumen_net::pins::rename_now(&device, request.slot);
        Ok::<_, lumen_net::NetError>((device, renamed))
    })
    .await
    .map_err(|err| ApiError::Internal(anyhow::anyhow!("{err}")))??;

    let (device, renamed) = outcome;
    let slot = request.slot;
    match renamed {
        Ok(()) => {
            tracing::info!(slot, %device, "adapter adopted into an orphaned name");
            Ok(Json(serde_json::json!({
                "adopted": format!("nic{slot}"),
                "device": device,
                "active": true,
            })))
        }
        Err(err) => {
            tracing::warn!(slot, %device, %err, "adopted, but the live rename failed");
            Ok(Json(serde_json::json!({
                "adopted": format!("nic{slot}"),
                "device": device,
                "active": false,
                "note": format!(
                    "{device} is pinned as nic{slot} and will carry that name from the next \
                     restart, but it could not be renamed while running ({err})."
                ),
            })))
        }
    }
}

/// GET /api/network/config â€” the committed desired state.
pub async fn config(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<NetworkDesiredState>, ApiError> {
    Ok(Json(state.network.config().await?))
}

/// GET /api/network/pending â€” staged delta, validation results, checkpoint.
pub async fn pending(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<PendingResponse>, ApiError> {
    Ok(Json(state.network.pending().await?))
}

/// DELETE /api/network/pending â€” discard everything staged.
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

/// POST /api/network/apply â€” validate, checkpoint, push.
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

/// POST /api/network/confirm â€” destroy the checkpoint; the change is now
/// permanent.
pub async fn confirm(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.network.confirm().await?))
}

/// POST /api/network/rollback â€” revert now rather than waiting out the window.
pub async fn rollback(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<PendingResponse>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.network.rollback().await?))
}

/// POST /api/network/apply/extend â€” more time for a slow operator.
pub async fn extend(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<impl Serialize>, ApiError> {
    let request: ExtendRequest = required_body(raw)?;
    Ok(Json(state.network.extend(request.seconds).await?))
}

/// POST /api/network/management-bridge â€” convert nicN to brN + port.
pub async fn management_bridge(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<ManagementBridgeResponse>, ApiError> {
    node_only(raw)?;
    Ok(Json(state.network.management_bridge().await?))
}
