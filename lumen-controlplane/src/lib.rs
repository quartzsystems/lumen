pub mod api;
pub mod config;
pub mod error;
pub mod ha;
pub mod maintenance;
pub mod peers;
pub mod realm;
pub mod security;
pub mod tasks;
pub mod tls;
pub mod web;

use std::sync::Arc;

use axum::Router;
use tower_http::trace::TraceLayer;

use config::Config;
use lumen_cluster::ClusterService;
use lumen_drbd::DrbdService;
use lumen_net::NetworkService;
use lumen_sys::SysService;
use lumen_virt::VirtService;
use lumen_zfs::StorageService;
use realm::RealmRegistry;

/// Shared state behind every /api handler.
///
/// Each domain takes its backend as a parameter the same way the realm
/// registry does, so tests inject the in-memory ones and never touch the
/// runner's NetworkManager, hypervisor, or storage.
pub struct AppState {
    pub config: Config,
    /// The live session-signing secret, swappable at runtime: joining an
    /// environment replaces this node's own secret with the shared one.
    pub jwt_secret: security::SessionSecret,
    /// The TLS listener's live configuration, for reloading onto the
    /// environment certificate after a bootstrap or join. `None` in the
    /// plain-HTTP development mode and under the tests.
    pub tls: Option<axum_server::tls_rustls::RustlsConfig>,
    pub realms: RealmRegistry,
    /// The node itself: its local accounts and its power state. The most basic
    /// domain, and the one that owns the privileged-command runner the storage
    /// domain borrows for `zpool create`.
    pub sys: Arc<SysService>,
    /// Bridges, bonds, VLAN interfaces.
    pub network: Arc<NetworkService>,
    /// Pools, datasets, and the volumes a machine's disks live on.
    pub storage: Arc<StorageService>,
    /// The machines themselves. Depends on the other two — a machine needs a
    /// bridge to attach to and a volume to boot from — which is why it is
    /// constructed last.
    pub virt: Arc<VirtService>,
    /// The environment and its clusters: membership, quorum, fencing.
    pub cluster: Arc<ClusterService>,
    /// Replicated volumes: DRBD resources over each member's zvols. Built on
    /// the cluster and storage domains, which is why it is constructed after
    /// both.
    pub drbd: Arc<DrbdService>,
    /// What has been done to each machine — the console's Tasks table.
    pub tasks: tasks::TaskLog,
    /// The drain of this node, while one is running. Node-local by nature:
    /// only the node running the machines can move them.
    pub drain: maintenance::DrainHandle,
}

/// The full application router: /api plus the static web UI fallback.
/// Takes the registry as a parameter so tests can inject a mock realm.
pub fn app(state: Arc<AppState>) -> Router {
    api::router(state.clone())
        .merge(web::router(&state.config.webui_dir))
        .layer(TraceLayer::new_for_http())
}
