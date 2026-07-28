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
    use axum::response::IntoResponse;

    let request: PowerRequest = required_body(raw)?;
    guard_cluster_power(&state, request.i_understand_the_cluster_loses_quorum).await?;
    match request.at {
        Some(at) => Ok(Json(state.sys.power_at(request.action, at).await?).into_response()),
        None => {
            state.sys.power_now(request.action).await?;
            Ok(axum::http::StatusCode::ACCEPTED.into_response())
        }
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
async fn guard_cluster_power(state: &Arc<AppState>, acknowledged: bool) -> Result<(), ApiError> {
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
    Ok(Json(state.sys.cancel_power().await?))
}
