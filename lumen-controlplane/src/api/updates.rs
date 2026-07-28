//! Update endpoints: what this node could install, and installing it.
//!
//! Thin by design, exactly as the other domains' handlers are: deserialize,
//! call one [`lumen_update::UpdateService`] method, serialize. The rules about
//! what may be installed are the domain's and are not repeated here — see
//! docs/updates.md for the split.
//!
//! The one thing this module does own is the difference between a **read** and
//! a **refresh**. `GET /api/system/updates` answers from what was last found,
//! so opening the console never waits on repository metadata over a link that
//! may be slow or absent; `POST /api/system/updates/check` is the operator
//! saying "ask them now", and is allowed to take its time.

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;

use lumen_update::{ApplyRequest, UpdateView};

use crate::api::request::{body, Body};
use crate::error::ApiError;
use crate::security::Session;
use crate::updates::UpdateProgress;
use crate::AppState;

/// POST /api/system/updates/apply.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyBody {
    /// Install the kernel and the modules built against it, rather than the
    /// ordinary updates. Never both — see [`ApplyRequest`].
    #[serde(default)]
    platform: bool,
    /// Required for the platform set, and named as the sentence being agreed
    /// to, the same way every other acknowledgement in this appliance is.
    #[serde(default)]
    i_understand_the_kernel_moves: bool,
}

/// GET /api/system/updates — what was last found, plus the reboot state read
/// fresh.
///
/// Never asks the repositories. A node whose repository is unreachable still
/// renders this page, carrying the reason the last check failed, because the
/// outstanding-restart notice on it is read from the node itself and is often
/// exactly what such an operator needs to see.
pub async fn updates(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<UpdateView>, ApiError> {
    Ok(Json(state.updates.view().await?))
}

/// POST /api/system/updates/check — ask the repositories now.
pub async fn check(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<UpdateView>, ApiError> {
    Ok(Json(state.updates.check().await?))
}

/// POST /api/system/updates/apply — start installing.
///
/// Answers **202 Accepted** with the progress record: a transaction runs for
/// minutes and an HTTP request that waited for one would be a request the
/// browser gave up on. The console polls
/// [`progress`] from there.
pub async fn apply(
    session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<axum::response::Response, ApiError> {
    let request: ApplyBody = body(raw)?;
    let progress = crate::updates::begin(
        &state,
        &session.0.sub,
        ApplyRequest {
            platform: request.platform,
            i_understand_the_kernel_moves: request.i_understand_the_kernel_moves,
        },
    )
    .await?;
    Ok((axum::http::StatusCode::ACCEPTED, Json(progress)).into_response())
}

/// GET /api/system/updates/progress — the transaction, running or finished.
///
/// `null` when this daemon has not run one. Deliberately not a 404: "nothing
/// is happening" is an answer the console renders, not an error it reports,
/// and it is the state the page is in almost all of the time.
pub async fn progress(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Json<Option<UpdateProgress>> {
    Json(state.update_job.get())
}
