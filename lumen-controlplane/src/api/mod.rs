pub mod auth;
pub mod network;

use std::sync::Arc;

use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde_json::json;

use crate::AppState;

/// Everything under /api.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/version", get(version))
        .route("/api/auth/realms", get(auth::realms))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/me", get(auth::me))
        // Networking. Every route below requires a session (the handlers take
        // the Session extractor); see src/api/network.rs.
        .route("/api/network/interfaces", get(network::interfaces))
        .route("/api/network/interfaces/{name}", get(network::interface))
        .route("/api/network/config", get(network::config))
        .route("/api/network/pending", get(network::pending))
        .route("/api/network/pending", delete(network::discard))
        .route("/api/network/bridges", post(network::create_bridge))
        .route("/api/network/bonds", post(network::create_bond))
        .route("/api/network/vlans", post(network::create_vlan))
        .route("/api/network/bridges/{name}", patch(network::update_bridge))
        .route("/api/network/bonds/{name}", patch(network::update_bond))
        .route("/api/network/vlans/{name}", patch(network::update_vlan))
        .route("/api/network/nics/{name}", patch(network::update_nic))
        .route(
            "/api/network/bridges/{name}",
            delete(network::delete_bridge),
        )
        .route("/api/network/bonds/{name}", delete(network::delete_bond))
        .route("/api/network/vlans/{name}", delete(network::delete_vlan))
        .route("/api/network/apply", post(network::apply))
        .route("/api/network/apply/extend", post(network::extend))
        .route("/api/network/confirm", post(network::confirm))
        .route("/api/network/rollback", post(network::rollback))
        .route(
            "/api/network/management-bridge",
            post(network::management_bridge),
        )
        .with_state(state)
}

async fn version() -> Json<serde_json::Value> {
    Json(json!({ "version": env!("LUMEN_VERSION") }))
}
