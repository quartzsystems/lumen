//! System endpoints: the node's own accounts, and its power state.
//!
//! Thin by design, exactly as the other domains' handlers are: deserialize,
//! call one `lumen_sys::SysService` method, serialize. There is no
//! `/etc/passwd` here, no `useradd`, and no validation — see docs/system.md for
//! the split.
//!
//! ## Every account route knows who is asking
//!
//! The [`Session`] extractor is not just an authentication check here; its
//! claims are passed down as `acting_as`. That is what lets the domain refuse
//! to lock the account the operator is signed in as, which is the one rule
//! this page cannot do without: nothing the console offers may take the console
//! away from the person using it.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;

use lumen_sys::model::{NewUser, PowerAction, UserPatch};
use lumen_sys::service::{DeleteUserResponse, PowerView, UserView, UsersResponse};
use lumen_sys::Acknowledgements;

use crate::api::request::{body, required_body, Body};
use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;

/// DELETE /api/system/users/{name}.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteUserRequest {
    /// Off by default: removing an account must not destroy the files it owns
    /// unless the caller asked for that in so many words. The same shape the
    /// machine delete keeps for its disks.
    #[serde(default)]
    remove_home: bool,
    #[serde(default)]
    i_understand_this_may_lose_data: bool,
}

/// POST /api/system/power and /api/system/power/schedule.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PowerRequest {
    action: PowerAction,
    /// Seconds since the epoch. Absent means now.
    #[serde(default)]
    at: Option<u64>,
    /// Go ahead even though the cluster needs this node. Required only when
    /// it does; see [`guard_cluster_power`].
    #[serde(default)]
    i_understand_the_cluster_loses_quorum: bool,
}

/// GET /api/system/users — every local account, and what may be done to each.
pub async fn users(
    session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<UsersResponse>, ApiError> {
    Ok(Json(state.sys.users(Some(&session.0.sub)).await?))
}

/// GET /api/system/users/{name} — one account.
pub async fn user(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<UserView>, ApiError> {
    Ok(Json(state.sys.user(&name, Some(&session.0.sub)).await?))
}

/// POST /api/system/users — create one.
///
/// The password is in the body and nowhere else: it never becomes a path
/// component, never a query parameter, and never an argument to anything. See
/// `lumen_sys::exec`.
pub async fn create_user(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<UserView>, ApiError> {
    let request: NewUser = required_body(raw)?;
    Ok(Json(state.sys.create_user(request).await?))
}

/// PATCH /api/system/users/{name} — change one. Absent fields are left alone.
pub async fn update_user(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<UserView>, ApiError> {
    let patch: UserPatch = body(raw)?;
    Ok(Json(
        state
            .sys
            .update_user(&name, patch, Some(&session.0.sub))
            .await?,
    ))
}

/// DELETE /api/system/users/{name} — remove one; its files stay unless asked.
pub async fn delete_user(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<DeleteUserResponse>, ApiError> {
    let request: DeleteUserRequest = body(raw)?;
    Ok(Json(
        state
            .sys
            .delete_user(
                &name,
                request.remove_home,
                Acknowledgements {
                    may_lose_data: request.i_understand_this_may_lose_data,
                },
                Some(&session.0.sub),
            )
            .await?,
    ))
}

/// GET /api/system/power — uptime, the node's clock, and anything scheduled.
pub async fn power(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<PowerView>, ApiError> {
    Ok(Json(state.sys.power().await?))
}

/// POST /api/system/power — restart or shut down, now or at a moment.
///
/// One route rather than four, because "restart" and "shut down" differ by a
/// word and "now" is a schedule with no time on it. The console sends what the
/// operator chose and the answer says what the node is committed to.
///
/// An immediate restart answers with **no body**: the connection is about to
/// go away, and a JSON object claiming success would be a promise this daemon
/// cannot keep. `202 Accepted` is the honest status.
pub async fn set_power(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<axum::response::Response, ApiError> {
    let request: PowerRequest = required_body(raw)?;
    let committed = power_locally(
        &state,
        request.action,
        request.at,
        request.i_understand_the_cluster_loses_quorum,
    )
    .await?;
    Ok(power_response(committed))
}

/// Commit *this* node to a restart or a shutdown, guard included.
///
/// Shared by the three callers that can reach this node's power: its own
/// route above, the environment route addressed at this node by name, and the
/// peer route another member's console relays through. All three must apply
/// the same guard and turn the same backend refusal into the same sentence,
/// and the way to be sure of that is for there to be one of them.
///
/// `None` is an immediate one, which has no view to answer with — the node is
/// going down and there is nothing truthful to say about the state it will be
/// in when it does.
pub(crate) async fn power_locally(
    state: &Arc<AppState>,
    action: PowerAction,
    at: Option<u64>,
    acknowledged: bool,
) -> Result<Option<PowerView>, ApiError> {
    guard_cluster_power(state, acknowledged).await?;
    match at {
        Some(at) => Ok(Some(
            state
                .sys
                .power_at(action, at)
                .await
                .map_err(refusal_said_aloud)?,
        )),
        None => {
            state
                .sys
                .power_now(action)
                .await
                .map_err(refusal_said_aloud)?;
            Ok(None)
        }
    }
}

/// The one shape both power routes answer in: the view for a schedule, and a
/// bare `202` for a node that is going down now.
fn power_response(committed: Option<PowerView>) -> axum::response::Response {
    use axum::response::IntoResponse;

    match committed {
        Some(view) => Json(view).into_response(),
        None => axum::http::StatusCode::ACCEPTED.into_response(),
    }
}

/// A power backend error is a refusal with the reason attached — "the node
/// refused to restart: access denied" is logind's own sentence, written for
/// the operator holding the dialog. The generic `Backend` → 500 mapping
/// would swallow it into "Internal server error.", which is exactly the
/// wrong behavior on the one page whose point is that a node that refuses
/// says so (the reasoning `backend::logind` chose `Reboot(false)` for).
fn refusal_said_aloud(err: lumen_sys::SysError) -> ApiError {
    match err {
        lumen_sys::SysError::Backend(inner) => ApiError::Conflict(format!("{inner:#}")),
        other => other.into(),
    }
}

/// Refuse to take down a node its cluster cannot spare.
///
/// The system domain answers for one node and knows nothing about clusters,
/// which is right — but it means nothing stood between an operator and
/// shutting down the last vote of a quorate cluster. The check belongs here,
/// where both domains are in reach.
///
/// Three ways past it, and they are all deliberate. A node already in
/// maintenance has been through the entering guards and its operator has been
/// told; a cluster that would stay quorate is nobody's problem; and an
/// explicit acknowledgement is always allowed, because an appliance that
/// cannot be powered off by its owner is a worse appliance. What is not
/// allowed is doing it by accident.
pub(crate) async fn guard_cluster_power(
    state: &Arc<AppState>,
    acknowledged: bool,
) -> Result<(), ApiError> {
    if acknowledged {
        return Ok(());
    }
    let node = state.cluster.node().to_string();
    let Some(membership) = state.cluster.environment_record()? else {
        return Ok(());
    };
    let Some(cluster) = membership.node(&node).and_then(|n| n.cluster.clone()) else {
        return Ok(());
    };
    if membership
        .node(&node)
        .is_some_and(lumen_cluster::EnvironmentNode::in_maintenance)
    {
        return Ok(());
    }
    // A cluster that cannot be asked is not evidence that going down is safe,
    // but it is not evidence of the opposite either — and refusing every
    // restart on an unreachable cluster would take the console's power page
    // away exactly when an operator needs it most.
    let Ok(view) = state.cluster.cluster(&cluster).await else {
        return Ok(());
    };
    if view.error.is_some() || lumen_cluster::quorum_survives_loss(&view.quorum) {
        return Ok(());
    }
    Err(ApiError::Conflict(format!(
        "\"{cluster}\" would lose quorum without this node: {} of {} votes are present, and one \
         more going away stops the cluster. Put this node into maintenance first — it moves the \
         machines off and tells the other members to expect the absence — or acknowledge \
         \"i_understand_the_cluster_loses_quorum\".",
        view.quorum.votes, view.quorum.expected_votes
    )))
}

/// DELETE /api/system/power — call off whatever is scheduled.
pub async fn cancel_power(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<PowerView>, ApiError> {
    Ok(Json(cancel_locally(&state).await?))
}

/// Call off *this* node's schedule. Shared by the same three callers
/// [`power_locally`] is.
pub(crate) async fn cancel_locally(state: &Arc<AppState>) -> Result<PowerView, ApiError> {
    state.sys.cancel_power().await.map_err(refusal_said_aloud)
}

// --- the environment ---------------------------------------------------------
//
// The same three questions asked of every member, and answered for whichever
// one the operator named. A separate set of routes rather than a widening of
// the three above, for the reason `inventory` gives: `/api/system/...` means
// *this appliance*, and every write already on that prefix is written against
// that meaning.
//
// Addressed by node in the body rather than in the path, because
// `/api/environment/nodes/{node}/power` already means something else and means
// it more violently: that one cuts a member's power through a fence device,
// for a node that has stopped answering. These routes are the graceful pair —
// logind's own restart, on a node that is still listening.

/// POST and DELETE /api/environment/power.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentPowerRequest {
    /// Which member. Its own name, as the environment record spells it.
    node: String,
    #[serde(default)]
    action: Option<PowerAction>,
    /// Seconds since the epoch. Absent means now.
    #[serde(default)]
    at: Option<u64>,
    #[serde(default)]
    i_understand_the_cluster_loses_quorum: bool,
}

/// GET /api/environment/power — every member's uptime, clock, and schedule.
///
/// A member that could not be asked comes back carrying the reason rather than
/// being dropped from the list. On this page that matters more than most: a
/// member that has stopped answering may be one an operator restarted thirty
/// seconds ago, and a table that silently lost the row would be hiding the
/// consequence of the last thing they did.
pub async fn environment_power(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Json<crate::cluster_power::EnvironmentPower> {
    Json(crate::cluster_power::environment(&state).await)
}

/// POST /api/environment/power — restart or shut one member down, now or at a
/// moment.
pub async fn set_environment_power(
    session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<axum::response::Response, ApiError> {
    let request: EnvironmentPowerRequest = required_body(raw)?;
    let action = request.action.ok_or_else(|| {
        ApiError::BadRequest("Say whether to restart or shut the node down.".to_string())
    })?;
    let committed = crate::cluster_power::set(
        &state,
        &request.node,
        action,
        request.at,
        request.i_understand_the_cluster_loses_quorum,
        &format!("{}@{}", session.0.sub, session.0.realm),
    )
    .await?;
    Ok(power_response(committed))
}

/// DELETE /api/environment/power — call off what one member has scheduled.
pub async fn cancel_environment_power(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<PowerView>, ApiError> {
    let request: EnvironmentPowerRequest = required_body(raw)?;
    Ok(Json(
        crate::cluster_power::cancel(&state, &request.node).await?,
    ))
}
