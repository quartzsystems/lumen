pub mod auth;
pub mod cluster;
pub mod console;
pub mod network;
pub mod nodes;
pub mod peer;
pub mod request;
pub mod storage;
pub mod system;
pub mod updates;
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
        .route("/api/vms/{vmid}/tasks", get(vms::tasks))
        .route("/api/vms/{vmid}/start", post(vms::start))
        .route("/api/vms/{vmid}/shutdown", post(vms::shutdown))
        .route("/api/vms/{vmid}/stop", post(vms::stop))
        .route("/api/vms/{vmid}/reboot", post(vms::reboot))
        .route("/api/vms/{vmid}/reset", post(vms::reset))
        // Live migration to another member of the machine's disks' replica
        // set — the two-primaries window around it is the service's guard;
        // see src/api/vms.rs and docs/storage.md.
        .route("/api/vms/{vmid}/migrate", post(vms::migrate))
        // The console viewer. The first says where to connect and why not when
        // there is nowhere; the second is the stream itself. Both are GETs,
        // because an upgrade request is one — see src/api/console.rs.
        .route("/api/vms/{vmid}/console", get(console::info))
        .route("/api/vms/{vmid}/console/ws", get(console::attach))
        // A file into a running guest, over its agent rather than over the
        // console — RFB has no file transfer in it. The limit is raised to what
        // the agent will actually take rather than disabled: unlike an
        // installation image this is buffered, so the cap is the defence.
        .route(
            "/api/vms/{vmid}/files",
            put(vms::push_file).layer(axum::extract::DefaultBodyLimit::max(
                lumen_virt::MAX_GUEST_FILE_BYTES,
            )),
        )
        .route("/api/vms/{vmid}/disks", post(vms::attach_disk))
        .route("/api/vms/{vmid}/disks/{id}", delete(vms::detach_disk))
        .route("/api/vms/{vmid}/nics", post(vms::attach_nic))
        .route("/api/vms/{vmid}/nics/{id}", delete(vms::detach_nic))
        // The same log as /api/vms/{vmid}/tasks, unfiltered and windowed —
        // what the dashboard shows as activity across every machine. Not
        // under /api/vms because it is not about one machine.
        .route("/api/tasks", get(vms::recent_tasks))
        // The nodes themselves: what each has, and what is running on it. The
        // dashboard's compute panel and the Infrastructure section read this.
        .route("/api/nodes", get(nodes::list))
        // The environment and its clusters — the repo's grouped-by-node shape
        // extended one level upward: grouped by cluster, then by node. A node
        // that never joined an environment still answers, with itself as the
        // one unassigned node; see src/api/cluster.rs and docs/cluster.md.
        .route("/api/environment", get(cluster::environment))
        .route("/api/environment/inventory", get(cluster::inventory))
        // One pool name, built on several members at once — what the drive
        // picker across nodes submits. Each pool is still that node's own.
        .route(
            "/api/environment/storage/pools",
            post(cluster::create_cluster_pool),
        )
        // Clearing one member's disk from any member's console. The disk
        // picker spans the environment, so the operation that unblocks it has
        // to as well — see the handler for why the node is in the path.
        .route(
            "/api/environment/nodes/{node}/disks/{disk}/wipe",
            post(cluster::wipe_node_disk),
        )
        // Updates across every member: the same four questions the node-local
        // routes answer, asked of the whole environment. Installing walks the
        // members one at a time, this node last; see src/cluster_updates.rs
        // and docs/updates.md.
        .route("/api/environment/updates", get(updates::cluster_updates))
        .route(
            "/api/environment/updates/check",
            post(updates::cluster_check),
        )
        .route(
            "/api/environment/updates/apply",
            post(updates::cluster_apply),
        )
        .route(
            "/api/environment/updates/progress",
            get(updates::cluster_progress),
        )
        .route("/api/environment/tokens", post(cluster::mint_token))
        .route("/api/environment/join", post(cluster::join))
        .route("/api/environment/preflight", post(cluster::preflight))
        .route(
            "/api/environment/nodes/{node}/bond",
            post(cluster::bond_node_nics),
        )
        .route("/api/environment/clusters", post(cluster::create_cluster))
        .route(
            "/api/environment/clusters/pending",
            get(cluster::create_progress),
        )
        .route("/api/environment/clusters/{name}", get(cluster::cluster))
        .route(
            "/api/environment/clusters/{name}",
            delete(cluster::destroy_cluster),
        )
        // The cluster's typed networks — Core, Management, External — off the
        // replicated record; the console's Networking → Networks page.
        .route(
            "/api/environment/clusters/{name}/networks",
            get(cluster::cluster_networks),
        )
        // Defining an External network builds its bridge on every member
        // before the record admits it exists — see the handler for why the
        // two halves are one call.
        .route(
            "/api/environment/clusters/{name}/networks/external",
            post(cluster::create_external_network),
        )
        // Changing an External network rebuilds it everywhere before the
        // record admits the change, exactly as defining one does. Removing
        // it forgets the definition and leaves the bridges — see the handler.
        .route(
            "/api/environment/clusters/{name}/networks/external/{network}",
            put(cluster::update_external_network).delete(cluster::forget_external_network),
        )
        // Moving the cluster VIP, or taking it away. Acknowledged rather
        // than refused: there is no version of this that does not drop the
        // address for a moment.
        .route(
            "/api/environment/clusters/{name}/vip",
            put(cluster::set_vip),
        )
        // Clearing the cluster VIP's latched failure and probing it
        // again — the step that turns "I fixed the cause" into an address
        // that actually comes back up.
        .route(
            "/api/environment/clusters/{name}/vip/recover",
            post(cluster::recover_vip),
        )
        .route(
            "/api/environment/nodes/{name}",
            delete(cluster::remove_node),
        )
        // The 2→3 scale-out: add an unassigned node to a running cluster —
        // 202, then poll the same pending feed a create uses.
        .route(
            "/api/environment/clusters/{name}/nodes",
            post(cluster::add_node),
        )
        // Fencing: the guarded live test per direction, and the break-glass
        // confirmation for a node that is unreachable and could not be
        // fenced. Both are operator actions, never peer calls.
        .route(
            "/api/environment/clusters/{name}/fence/{node}/test",
            post(cluster::test_fence),
        )
        .route(
            "/api/environment/clusters/{name}/nodes/{node}/confirm-dead",
            post(cluster::confirm_node_dead),
        )
        // Maintenance: take this node out of service and drain it, put it
        // back, and watch the drain in between. The node in the path is
        // always this one — the machines can only be moved by whoever is
        // running them — and the handler says so when it is not.
        .route(
            "/api/environment/clusters/{name}/nodes/{node}/maintenance",
            post(cluster::enter_maintenance).delete(cluster::exit_maintenance),
        )
        .route("/api/environment/maintenance", get(cluster::drain_progress))
        // The peer surface: one control plane answering another, peer-ticket
        // authenticated — except join, whose one-time token is the
        // authentication; see src/api/peer.rs.
        .route("/api/peer/join", post(peer::join))
        .route("/api/peer/membership", post(peer::membership))
        .route("/api/peer/preflight", post(peer::preflight))
        .route("/api/peer/cluster/prepare", post(peer::prepare))
        .route("/api/peer/cluster/start", post(peer::start))
        .route("/api/peer/network/bond", post(peer::create_bond))
        .route("/api/peer/network/bridge", post(peer::create_bridge))
        .route("/api/peer/node/inventory", post(peer::inventory))
        .route("/api/peer/system/updates", post(peer::updates))
        .route("/api/peer/system/updates/check", post(peer::check_updates))
        .route("/api/peer/system/updates/apply", post(peer::apply_updates))
        // Maintenance and power, reached across the wire for one caller: the
        // rolling update. The work still happens on the node it is about; see
        // src/api/peer.rs.
        .route(
            "/api/peer/system/maintenance",
            post(peer::enter_maintenance),
        )
        .route(
            "/api/peer/system/maintenance/progress",
            post(peer::drain_progress),
        )
        .route(
            "/api/peer/system/maintenance/exit",
            post(peer::exit_maintenance),
        )
        .route("/api/peer/system/restart", post(peer::restart))
        .route("/api/peer/storage/pool", post(peer::create_pool))
        .route("/api/peer/storage/wipe", post(peer::wipe_disk))
        .route("/api/peer/cluster/teardown", post(peer::teardown))
        .route("/api/peer/cluster/reconfigure", post(peer::reconfigure))
        .route("/api/peer/volume/prepare", post(peer::prepare_volume))
        .route("/api/peer/volume/prime", post(peer::prime_volume))
        .route("/api/peer/volume/teardown", post(peer::teardown_volume))
        .route(
            "/api/peer/volume/resize-backing",
            post(peer::resize_volume_backing),
        )
        .route("/api/peer/volume/grow", post(peer::grow_volume))
        .route(
            "/api/peer/volume/two-primaries",
            post(peer::volume_two_primaries),
        )
        .route(
            "/api/peer/volume/snapshot",
            post(peer::snapshot_volume_backing),
        )
        .route(
            "/api/peer/volume/rollback-backing",
            post(peer::rollback_volume_backing),
        )
        .route(
            "/api/peer/volume/drop-snapshot",
            post(peer::drop_volume_snapshot),
        )
        .route("/api/peer/volume/down", post(peer::down_volume))
        .route("/api/peer/volume/up", post(peer::up_volume))
        .route(
            "/api/peer/volume/invalidate-remote",
            post(peer::invalidate_remote_volume),
        )
        .route("/api/peer/volume/reconnect", post(peer::reconnect_volume))
        .route(
            "/api/peer/volume/apply-policy",
            post(peer::apply_volume_policy),
        )
        // The pool half of the peer surface: one closed verb enum, run
        // against this node's own daemon over its own loopback — the only
        // way a pool daemon is ever addressed from off-box.
        .route("/api/peer/pool/verb", post(peer::pool_verb))
        .route("/api/peer/definition/store", post(peer::store_definition))
        .route("/api/peer/definition/drop", post(peer::drop_definition))
        // The node itself: its local accounts, and its power state. Every
        // account route passes the session's own principal down, which is what
        // lets the domain refuse to lock the operator out of their own console
        // — see src/api/system.rs.
        .route("/api/system/users", get(system::users))
        .route("/api/system/users", post(system::create_user))
        .route("/api/system/users/{name}", get(system::user))
        .route("/api/system/users/{name}", patch(system::update_user))
        .route("/api/system/users/{name}", delete(system::delete_user))
        .route("/api/system/power", get(system::power))
        .route("/api/system/power", post(system::set_power))
        .route("/api/system/power", delete(system::cancel_power))
        // Updates. The read never touches the network and the refresh always
        // does — the two are separate routes so a console that opens on a node
        // with an unreachable repository still renders. Applying answers 202
        // and is watched through the progress route; see src/api/updates.rs
        // and docs/updates.md.
        .route("/api/system/updates", get(updates::updates))
        .route("/api/system/updates/check", post(updates::check))
        .route("/api/system/updates/apply", post(updates::apply))
        .route("/api/system/updates/progress", get(updates::progress))
        // Storage. The one volume write is reached through a machine's disks,
        // because a volume is created for a machine; a pool is not created for
        // anything, so it lives here. The media library is here too: an
        // operator has to be able to put an installation image on the node
        // from the console.
        .route("/api/storage/pools", get(storage::pools))
        .route("/api/storage/pools", post(storage::create_pool))
        .route("/api/storage/pools/{pool}", delete(storage::destroy_pool))
        .route("/api/storage/devices", get(storage::devices))
        .route("/api/storage/pools/{pool}/volumes", get(storage::volumes))
        // Replicated volumes: DRBD resources over each cluster member's
        // zvols, grouped by cluster; see src/api/storage.rs and
        // docs/cluster.md.
        .route("/api/storage/replicated", get(storage::replicated_volumes))
        .route(
            "/api/storage/replicated",
            post(storage::create_replicated_volume),
        )
        .route(
            "/api/storage/replicated/{cluster}/{name}",
            delete(storage::destroy_replicated_volume),
        )
        .route(
            "/api/storage/replicated/{cluster}/{name}/resize",
            post(storage::resize_replicated_volume),
        )
        .route(
            "/api/storage/replicated/{cluster}/{name}/snapshots",
            get(storage::volume_snapshots),
        )
        .route(
            "/api/storage/replicated/{cluster}/{name}/snapshots",
            post(storage::snapshot_volume),
        )
        .route(
            "/api/storage/replicated/{cluster}/{name}/snapshots/{snapshot}",
            delete(storage::delete_volume_snapshot),
        )
        .route(
            "/api/storage/replicated/{cluster}/{name}/rollback",
            post(storage::rollback_volume),
        )
        .route(
            "/api/storage/replicated/{cluster}/{name}/resolve-split-brain",
            post(storage::resolve_split_brain),
        )
        // The LumenFS pool: the observed view, and the snapshot verbs. Disks
        // are addressed by the compute domain's name for them — the device
        // path is the same fact with slashes in it.
        .route("/api/storage/pool", get(storage::pooled_storage))
        .route(
            "/api/storage/pool/disks/{name}/snapshots",
            post(storage::snapshot_pooled_disk),
        )
        .route(
            "/api/storage/pool/disks/{name}/snapshots/{snapshot}",
            delete(storage::delete_pooled_snapshot),
        )
        .route(
            "/api/storage/pool/disks/{name}/rollback",
            post(storage::rollback_pooled_disk),
        )
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
