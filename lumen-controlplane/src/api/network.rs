//! Networking endpoints.
//!
//! Thin by design: deserialize, call one `lumen_net::NetworkService` method,
//! serialize. No netlink, no D-Bus, and no validation logic lives here — that
//! is the whole point of the component split (see docs/networking.md).
//!
//! Every route takes the [`Session`] extractor, so an unauthenticated request
//! is a 401 before any handler body runs.
//!
//! Every mutating route accepts the optional `node` field its body has
//! carried since day one — and since the console federation landed, naming
//! another member forwards the request to that member over the peer channel
//! instead of refusing it. The forwarded request lands in the *target's* own
//! networking domain: staged there, validated there, applied inside that
//! node's own checkpoint window, and auto-reverted by that node if no
//! confirm arrives — including because the change severed the path the
//! confirm would have travelled. The two reads a remote edit needs — the
//! staged delta and the nicN pins — take `?node=` for the same reason.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;

use lumen_net::model::{Bond, Bridge, LinkKind, Vlan};
use lumen_net::service::{
    BondPatch, BridgePatch, InterfacesResponse, LinkView, NicPatch, VlanPatch,
};
use lumen_net::{Acknowledgements, NetworkDesiredState};

use crate::api::request::{routed_body, routed_node_only, routed_required_body, Body};
use crate::error::ApiError;
use crate::inventory::NetworkVerb;
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

/// The `?node=` a read takes to ask about another member. Absent or naming
/// this appliance, the answer is local.
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

/// Run one verb against this node's own networking domain. The one
/// definition of what each verb does — the operator-facing handlers below
/// and the peer relay in `api/peer.rs` both end here, which is what makes a
/// forwarded write indistinguishable from a local one.
pub async fn run_verb(state: &AppState, verb: NetworkVerb) -> Result<serde_json::Value, ApiError> {
    fn out<T: serde::Serialize>(value: T) -> Result<serde_json::Value, ApiError> {
        serde_json::to_value(value).map_err(|err| ApiError::Internal(anyhow::anyhow!("{err}")))
    }
    match verb {
        NetworkVerb::Pending => out(state.network.pending().await?),
        NetworkVerb::Discard => out(state.network.discard().await?),
        NetworkVerb::CreateBridge(bridge) => out(state.network.create_bridge(bridge).await?),
        NetworkVerb::CreateBond(bond) => out(state.network.create_bond(bond).await?),
        NetworkVerb::CreateVlan(vlan) => out(state.network.create_vlan(vlan).await?),
        NetworkVerb::UpdateBridge { name, patch } => {
            out(state.network.update_bridge(&name, patch).await?)
        }
        NetworkVerb::UpdateBond { name, patch } => {
            out(state.network.update_bond(&name, patch).await?)
        }
        NetworkVerb::UpdateVlan { name, patch } => {
            out(state.network.update_vlan(&name, patch).await?)
        }
        NetworkVerb::UpdateNic { name, patch } => {
            out(state.network.update_nic(&name, patch).await?)
        }
        NetworkVerb::DeleteLink { name, kind } => {
            out(state.network.delete_link(&name, kind).await?)
        }
        NetworkVerb::Apply { may_disconnect } => out(state
            .network
            .apply(Acknowledgements { may_disconnect })
            .await?),
        NetworkVerb::Confirm => out(state.network.confirm().await?),
        NetworkVerb::Rollback => out(state.network.rollback().await?),
        NetworkVerb::Extend { seconds } => out(state.network.extend(seconds).await?),
        NetworkVerb::ManagementBridge => out(state.network.management_bridge().await?),
        NetworkVerb::Pins => {
            let roots = lumen_net::pins::PinRoots::default();
            out(
                tokio::task::spawn_blocking(move || lumen_net::pins::report(&roots))
                    .await
                    .map_err(|err| ApiError::Internal(anyhow::anyhow!("{err}")))?,
            )
        }
        NetworkVerb::Adopt { slot, mac } => adopt(slot, mac).await,
    }
}

/// Run the verb where it belongs: here, or on the member the request named.
///
/// The target must be in the environment record — a standalone appliance has
/// no record and refuses every remote name, which is the old "this appliance
/// only manages itself" rule surviving in the one case it still applies.
async fn dispatch(
    state: &Arc<AppState>,
    target: Option<String>,
    verb: NetworkVerb,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = target.filter(|node| node != state.cluster.node());
    let Some(node) = target else {
        return Ok(Json(run_verb(state, verb).await?));
    };
    let members = state.cluster.environment_nodes()?;
    let Some(member) = members.iter().find(|member| member.name == node) else {
        return Err(ApiError::BadRequest(format!(
            "\"{node}\" is not a member of this environment, so its network cannot be \
             configured from here."
        )));
    };
    Ok(Json(state.peers.network(member, &verb).await?))
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

/// GET /api/network/nics/pins — the names that have lost their hardware,
/// and the adapters nothing has claimed. `?node=` asks another member.
///
/// Its own route rather than a field on the interfaces view, because it
/// describes what the node's *names* are pinned to rather than what the
/// links are doing: an orphaned pin has no link to hang off, which is
/// precisely the problem it reports.
pub async fn nic_pins(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Query(query): Query<NodeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    dispatch(&state, query.target(), NetworkVerb::Pins).await
}

/// What a card adoption names: the orphaned slot, and the adapter to put
/// in it.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptRequest {
    slot: u32,
    mac: String,
}

/// POST /api/network/nics/adopt — give an orphaned name to a new card.
///
/// The one repair a replaced adapter needs, and deliberately manual: the
/// appliance cannot know which port of a new card carries the network the
/// old one did, and guessing moves storage replication or a cluster ring
/// onto whatever enumerated first. See `lumen_net::pins`.
pub async fn adopt_nic(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, AdoptRequest) = routed_required_body(raw)?;
    dispatch(
        &state,
        target,
        NetworkVerb::Adopt {
            slot: request.slot,
            mac: request.mac,
        },
    )
    .await
}

/// The adoption itself, on this node. The pin alone takes effect at the next
/// boot; renaming now is what makes the profiles above it work today. A
/// rename that fails is reported, not hidden — the pin is written either
/// way, so the next boot repairs what this could not.
async fn adopt(slot: u32, mac: String) -> Result<serde_json::Value, ApiError> {
    let outcome = tokio::task::spawn_blocking(move || {
        let roots = lumen_net::pins::PinRoots::default();
        let device = lumen_net::pins::adopt(&roots, slot, &mac)?;
        let renamed = lumen_net::pins::rename_now(&device, slot);
        Ok::<_, lumen_net::NetError>((device, renamed))
    })
    .await
    .map_err(|err| ApiError::Internal(anyhow::anyhow!("{err}")))??;

    let (device, renamed) = outcome;
    match renamed {
        Ok(()) => {
            tracing::info!(slot, %device, "adapter adopted into an orphaned name");
            Ok(serde_json::json!({
                "adopted": format!("nic{slot}"),
                "device": device,
                "active": true,
            }))
        }
        Err(err) => {
            tracing::warn!(slot, %device, %err, "adopted, but the live rename failed");
            Ok(serde_json::json!({
                "adopted": format!("nic{slot}"),
                "device": device,
                "active": false,
                "note": format!(
                    "{device} is pinned as nic{slot} and will carry that name from the next \
                     restart, but it could not be renamed while running ({err})."
                ),
            }))
        }
    }
}

/// GET /api/network/config — the committed desired state.
pub async fn config(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<NetworkDesiredState>, ApiError> {
    Ok(Json(state.network.config().await?))
}

/// GET /api/network/pending — staged delta, validation results, checkpoint.
/// `?node=` asks another member, which is how the console watches a change
/// it staged there.
pub async fn pending(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Query(query): Query<NodeQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    dispatch(&state, query.target(), NetworkVerb::Pending).await
}

/// DELETE /api/network/pending — discard everything staged.
pub async fn discard(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = routed_node_only(raw)?;
    dispatch(&state, target, NetworkVerb::Discard).await
}

pub async fn create_bridge(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, bridge): (_, Bridge) = routed_required_body(raw)?;
    dispatch(&state, target, NetworkVerb::CreateBridge(bridge)).await
}

pub async fn create_bond(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, bond): (_, Bond) = routed_required_body(raw)?;
    dispatch(&state, target, NetworkVerb::CreateBond(bond)).await
}

pub async fn create_vlan(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, vlan): (_, Vlan) = routed_required_body(raw)?;
    dispatch(&state, target, NetworkVerb::CreateVlan(vlan)).await
}

pub async fn update_bridge(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, patch): (_, BridgePatch) = routed_body(raw)?;
    dispatch(&state, target, NetworkVerb::UpdateBridge { name, patch }).await
}

pub async fn update_bond(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, patch): (_, BondPatch) = routed_body(raw)?;
    dispatch(&state, target, NetworkVerb::UpdateBond { name, patch }).await
}

pub async fn update_vlan(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, patch): (_, VlanPatch) = routed_body(raw)?;
    dispatch(&state, target, NetworkVerb::UpdateVlan { name, patch }).await
}

pub async fn update_nic(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, patch): (_, NicPatch) = routed_body(raw)?;
    dispatch(&state, target, NetworkVerb::UpdateNic { name, patch }).await
}

pub async fn delete_bridge(
    session: Session,
    state: State<Arc<AppState>>,
    path: Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    delete_link(session, state, path, raw, LinkKind::Bridge).await
}

pub async fn delete_bond(
    session: Session,
    state: State<Arc<AppState>>,
    path: Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    delete_link(session, state, path, raw, LinkKind::Bond).await
}

pub async fn delete_vlan(
    session: Session,
    state: State<Arc<AppState>>,
    path: Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    delete_link(session, state, path, raw, LinkKind::Vlan).await
}

async fn delete_link(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
    kind: LinkKind,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = routed_node_only(raw)?;
    dispatch(&state, target, NetworkVerb::DeleteLink { name, kind }).await
}

/// POST /api/network/apply — validate, checkpoint, push.
pub async fn apply(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, ApplyRequest) = routed_body(raw)?;
    dispatch(
        &state,
        target,
        NetworkVerb::Apply {
            may_disconnect: request.i_understand_this_may_disconnect_me,
        },
    )
    .await
}

/// POST /api/network/confirm — destroy the checkpoint; the change is now
/// permanent.
pub async fn confirm(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = routed_node_only(raw)?;
    dispatch(&state, target, NetworkVerb::Confirm).await
}

/// POST /api/network/rollback — revert now rather than waiting out the window.
pub async fn rollback(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = routed_node_only(raw)?;
    dispatch(&state, target, NetworkVerb::Rollback).await
}

/// POST /api/network/apply/extend — more time for a slow operator.
pub async fn extend(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (target, request): (_, ExtendRequest) = routed_required_body(raw)?;
    dispatch(
        &state,
        target,
        NetworkVerb::Extend {
            seconds: request.seconds,
        },
    )
    .await
}

/// POST /api/network/management-bridge — convert nicN to brN + port.
pub async fn management_bridge(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let target = routed_node_only(raw)?;
    dispatch(&state, target, NetworkVerb::ManagementBridge).await
}
