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
    match request.at {
        Some(at) => Ok(Json(state.sys.power_at(request.action, at).await?).into_response()),
        None => {
            state.sys.power_now(request.action).await?;
            Ok(axum::http::StatusCode::ACCEPTED.into_response())
        }
    }
}

/// DELETE /api/system/power — call off whatever is scheduled.
pub async fn cancel_power(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<PowerView>, ApiError> {
    Ok(Json(state.sys.cancel_power().await?))
}
