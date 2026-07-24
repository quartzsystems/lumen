pub mod api;
pub mod config;
pub mod error;
pub mod realm;
pub mod security;
pub mod tls;
pub mod web;

use std::sync::Arc;

use axum::Router;
use tower_http::trace::TraceLayer;

use config::Config;
use lumen_net::NetworkService;
use realm::RealmRegistry;

/// Shared state behind every /api handler.
pub struct AppState {
    pub config: Config,
    pub jwt_secret: Vec<u8>,
    pub realms: RealmRegistry,
    /// The networking domain. Takes its backend as a parameter the same way
    /// the realm registry does, so tests inject the in-memory one and never
    /// touch the runner's NetworkManager.
    pub network: Arc<NetworkService>,
}

/// The full application router: /api plus the static web UI fallback.
/// Takes the registry as a parameter so tests can inject a mock realm.
pub fn app(state: Arc<AppState>) -> Router {
    api::router(state.clone())
        .merge(web::router(&state.config.webui_dir))
        .layer(TraceLayer::new_for_http())
}
