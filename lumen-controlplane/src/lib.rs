pub mod api;
pub mod cluster_updates;
pub mod config;
pub mod error;
pub mod ha;
pub mod inventory;
pub mod maintenance;
pub mod peers;
pub mod pool;
pub mod realm;
pub mod security;
pub mod tasks;
pub mod tls;
pub mod updates;
pub mod web;

use std::sync::Arc;

use axum::Router;
use tower_http::trace::TraceLayer;

use config::Config;
use lumen_cluster::ClusterService;
use lumen_drbd::DrbdService;
use lumen_net::NetworkService;
use lumen_sys::SysService;
use lumen_update::UpdateService;
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
    /// How the environment-wide reads reach the other members. The real
    /// channel in the daemon, [`inventory::NoPeers`] anywhere there is none;
    /// the workflows reach the full channel through the services that own it.
    pub peers: Arc<dyn inventory::InventoryPeers>,
    /// Replicated volumes: DRBD resources over each member's zvols. Built on
    /// the cluster and storage domains, which is why it is constructed after
    /// both.
    pub drbd: Arc<DrbdService>,
    /// The LumenFS pool on this node's cluster, if there is one — decided by
    /// the daemon's own drop-in at startup, never by a second record. On a
    /// pooled node this is also the engine `virt` holds as its `VmVolumes`.
    pub pool: pool::PoolPresence,
    /// What this node could install, and installing it. Depends on nothing
    /// else here: the packages waiting for a node are a fact about the node,
    /// not about its machines or its cluster.
    pub updates: Arc<UpdateService>,
    /// What has been done to each machine — the console's Tasks table.
    pub tasks: tasks::TaskLog,
    /// The drain of this node, while one is running. Node-local by nature:
    /// only the node running the machines can move them.
    pub drain: maintenance::DrainHandle,
    /// The update transaction, while one is running. Node-local for the same
    /// reason the drain is, and one at a time because the package manager
    /// takes a lock of its own.
    pub update_job: updates::UpdateHandle,
    /// The environment-wide update, while this node is driving one. Unlike the
    /// two above it, this one is about every member — but it still lives on
    /// one node, because the node that started the walk is the node running
    /// it. It does not survive that node updating itself; see
    /// [`cluster_updates`] for why that is the honest design rather than a
    /// gap.
    pub roll: cluster_updates::RollHandle,
}

/// The full application router: /api plus the static web UI fallback.
/// Takes the registry as a parameter so tests can inject a mock realm.
pub fn app(state: Arc<AppState>) -> Router {
    api::router(state.clone())
        .merge(web::router(&state.config.webui_dir))
        .layer(TraceLayer::new_for_http())
}
