//! What each node is, and how much of it is spoken for.
//!
//! Thin in the same way `src/api/vms.rs` is: one call into
//! `lumen_virt::VirtService`, serialized. It is a separate module rather than
//! another handler in that one because a node is not a machine — the console's
//! Infrastructure section is what reads this, and clustering will grow it.
//!
//! There is exactly one node today and the response is still a list. That is
//! deliberate and matches `/api/vms` and `/api/network/interfaces`: the shape
//! must not change when a second node arrives.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use lumen_virt::service::NodesResponse;

use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;

/// GET /api/nodes — every node, its processors and memory, and what the
/// machines running on it are holding.
pub async fn list(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<NodesResponse>, ApiError> {
    Ok(Json(state.virt.nodes().await?))
}
