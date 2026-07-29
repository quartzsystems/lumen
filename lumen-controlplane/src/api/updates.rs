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

// --- the environment ---------------------------------------------------------
//
// The same four questions asked of every member. A separate set of routes
// rather than a widening of the four above, for the reason `inventory` gives:
// `/api/system/...` means *this appliance*, and every write already on that
// prefix is written against that meaning.

/// GET /api/environment/updates — what every member has waiting.
///
/// Never asks any member's repositories, so opening the console costs one
/// round trip per member and no traffic beyond the cluster. A member that
/// could not be asked comes back carrying the reason rather than being dropped
/// from the list — during a walk, an unreachable member is usually the one
/// restarting its control plane, which is the update working.
pub async fn cluster_updates(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Json<crate::cluster_updates::ClusterUpdateView> {
    Json(crate::cluster_updates::environment(&state).await)
}

/// POST /api/environment/updates/check — every member asks its repositories
/// now, concurrently.
pub async fn cluster_check(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Json<crate::cluster_updates::ClusterUpdateView> {
    Json(crate::cluster_updates::check(&state).await)
}

/// POST /api/environment/updates/apply — update every member, one at a time.
///
/// `{}` installs the ordinary updates in place. `{"rolling": true,
/// "i_understand_each_node_restarts": true}` takes each member that needs a
/// restart through maintenance, its kernel, and a reboot before the next one
/// is touched.
///
/// **202 Accepted** with the walk's first progress: the members are named and
/// the first one is starting, and the walk runs on from there for as long as
/// the transactions take. Watched through [`cluster_progress`].
pub async fn cluster_apply(
    session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<axum::response::Response, ApiError> {
    let request: crate::cluster_updates::RollRequest = body(raw)?;
    let progress = crate::cluster_updates::begin(
        &state,
        &format!("{}@{}", session.0.sub, session.0.realm),
        request,
    )
    .await?;
    Ok((axum::http::StatusCode::ACCEPTED, Json(progress)).into_response())
}

/// GET /api/environment/updates/progress — the walk, running or finished.
///
/// `null` when this node is not running one — which is also what it answers
/// once the walk has updated *this* node, because installing Lumen restarts
/// the control plane and takes the record with it. That is not a gap the
/// console has to paper over: `GET /api/environment/updates` reads what every
/// member actually ended up with, which is the better answer anyway.
pub async fn cluster_progress(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Json<Option<crate::cluster_updates::RollProgress>> {
    Json(state.roll.get())
}
