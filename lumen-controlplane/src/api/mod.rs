pub mod auth;
pub mod network;
pub mod request;
pub mod storage;
pub mod vms;

use std::sync::Arc;

use axum::routing::{delete, get, patch, post, put};
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
        // Virtual machines. Same discipline as networking: every handler
        // deserializes, calls one lumen-virt method, and serializes; see
        // src/api/vms.rs and docs/compute.md.
        .route("/api/vms", get(vms::list))
        .route("/api/vms", post(vms::create))
        // What this node offers, for the create dialog to fill its pickers
        // from. Before the {vmid} routes because they are literal segments and
        // must not be read as an identifier.
        .route("/api/vms/next-id", get(vms::next_id))
        .route("/api/vms/cpu-models", get(vms::cpu_models))
        .route("/api/vms/os-catalog", get(vms::os_catalog))
        .route("/api/vms/{vmid}", get(vms::get))
        .route("/api/vms/{vmid}", patch(vms::update))
        .route("/api/vms/{vmid}", delete(vms::delete))
        .route("/api/vms/{vmid}/start", post(vms::start))
        .route("/api/vms/{vmid}/shutdown", post(vms::shutdown))
        .route("/api/vms/{vmid}/stop", post(vms::stop))
        .route("/api/vms/{vmid}/reboot", post(vms::reboot))
        .route("/api/vms/{vmid}/reset", post(vms::reset))
        .route("/api/vms/{vmid}/disks", post(vms::attach_disk))
        .route("/api/vms/{vmid}/disks/{id}", delete(vms::detach_disk))
        .route("/api/vms/{vmid}/nics", post(vms::attach_nic))
        .route("/api/vms/{vmid}/nics/{id}", delete(vms::detach_nic))
        // Storage. The pools are read-only — the one volume write is reached
        // through a machine's disks, because a volume is created for a
        // machine. The media library is not: an operator has to be able to put
        // an installation image on the node from the console.
        .route("/api/storage/pools", get(storage::pools))
        .route("/api/storage/pools/{pool}/volumes", get(storage::volumes))
        .route("/api/storage/iso", get(storage::isos))
        .route("/api/storage/iso/{pool}", post(storage::create_iso_store))
        // No body limit on the upload, and only on the upload: an installation
        // image is gigabytes and is streamed to disk rather than buffered, so
        // the default cap would reject every real one. Every other route keeps
        // it.
        .route(
            "/api/storage/iso/{pool}/{name}",
            put(storage::upload_iso).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route(
            "/api/storage/iso/{pool}/{name}",
            delete(storage::delete_iso),
        )
        .with_state(state)
}

async fn version() -> Json<serde_json::Value> {
    Json(json!({ "version": env!("LUMEN_VERSION") }))
}
