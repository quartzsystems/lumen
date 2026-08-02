pub mod auth;
pub mod cluster;
pub mod console;
pub mod imports;
pub mod network;
pub mod nodes;
pub mod peer;
pub mod request;
pub mod shell;
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
        // The name pins: which adapter answers to which nicN, what has lost
        // its hardware, and the one repair for a replaced card. Before the
        // {name} route — these are literal segments, not link names.
        .route("/api/network/nics/pins", get(network::nic_pins))
        .route("/api/network/nics/adopt", post(network::adopt_nic))
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
        // Importing a machine from a VMware archive. The upload streams the
        // archive to the spool the way an installation image streams to the
        // media library — the same no-limit reasoning as that route — and
        // answers with the machine the archive describes; the commit is a
        // 202 watched through the pending feed, the pool workflows' shape.
        .route("/api/vms/import", get(imports::list))
        .route("/api/vms/import/pending", get(imports::progress))
        .route(
            "/api/vms/import/{name}",
            put(imports::upload).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/api/vms/import/{name}", post(imports::commit))
        .route("/api/vms/import/{name}", delete(imports::remove))
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
        .route("/api/vms/{vmid}/cdroms", post(vms::attach_cdrom))
        .route("/api/vms/{vmid}/cdroms/{id}", put(vms::set_cdrom_media))
        .route("/api/vms/{vmid}/cdroms/{id}", delete(vms::detach_cdrom))
        // The same log as /api/vms/{vmid}/tasks, unfiltered and windowed —
        // what the dashboard shows as activity across every machine. Not
        // under /api/vms because it is not about one machine.
        .route("/api/tasks", get(vms::recent_tasks))
        // The same log again, from every member at once — the Logs page. Under
        // /api/environment because that is what it is about; the handler stays
        // beside its node-local twin.
        .route("/api/environment/tasks", get(vms::environment_tasks))
        // The nodes themselves: what each has, and what is running on it. The
        // dashboard's compute panel and the Infrastructure section read this.
        .route("/api/nodes", get(nodes::list))
        // The environment and its clusters — the repo's grouped-by-node shape
        // extended one level upward: grouped by cluster, then by node. A node
        // that never joined an environment still answers, with itself as the
        // one unassigned node; see src/api/cluster.rs and docs/cluster.md.
        .route("/api/environment", get(cluster::environment))
        .route("/api/environment/inventory", get(cluster::inventory))
        // Clearing one member's disk from any member's console. The disk
        // picker spans the environment, so the operation that unblocks it has
        // to as well — see the handler for why the node is in the path.
        .route(
            "/api/environment/nodes/{node}/disks/{disk}/wipe",
            post(cluster::wipe_node_disk),
        )
        // A member's power, through its fence device — the path that works
        // when the node's own operating system does not. Never this node:
        // see the handler.
        .route(
            "/api/environment/nodes/{node}/power",
            post(cluster::power_node),
        )
        // A login session on the node itself — administrators only, and
        // only on the node the request reached. See api/shell.rs.
        .route("/api/environment/nodes/{node}/shell/ws", get(shell::attach))
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
        // A member's power the graceful way: logind's own restart, shutdown,
        // and schedule, on a node that is still listening. Addressed by node
        // in the body — the path above already means the fence device, which
        // is the other thing entirely. See src/cluster_power.rs.
        .route("/api/environment/power", get(system::environment_power))
        .route(
            "/api/environment/power",
            post(system::set_environment_power),
        )
        .route(
            "/api/environment/power",
            delete(system::cancel_environment_power),
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
        // Changing the Core network — its MTU, and which link carries each
        // member's seat. Never its addressing: that is the ring's identity,
        // and it stays behind destroy-and-recreate. See the handler.
        .route(
            "/api/environment/clusters/{name}/networks/core",
            put(cluster::update_core_network),
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
        .route("/api/peer/cluster/core-seat", post(peer::update_core_seat))
        .route("/api/peer/cluster/start", post(peer::start))
        .route("/api/peer/network/bond", post(peer::create_bond))
        .route("/api/peer/network/bridge", post(peer::create_bridge))
        // The networking half of the console federation: one closed verb
        // enum, run against this node's own networking domain — staged,
        // checkpointed, and auto-reverted here exactly as a local edit is.
        .route("/api/peer/network/verb", post(peer::network_verb))
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
        // The graceful power trio, for one caller: an operator restarting,
        // shutting down, or scheduling a member from another member's console.
        // The guards are the target's own; see src/cluster_power.rs.
        .route("/api/peer/tasks", post(peer::tasks))
        // The compute half of the federation: this member's machines, and one
        // closed verb run against them on an operator's behalf. Every guard is
        // this member's; see src/api/vms.rs.
        .route("/api/peer/vms", post(peer::vms))
        .route("/api/peer/vms/verb", post(peer::vm_verb))
        .route("/api/peer/system/power", post(peer::power_state))
        .route("/api/peer/system/power/set", post(peer::set_power))
        .route("/api/peer/system/power/cancel", post(peer::cancel_power))
        .route("/api/peer/node/power", post(peer::power))
        .route("/api/peer/storage/wipe", post(peer::wipe_disk))
        .route("/api/peer/cluster/teardown", post(peer::teardown))
        .route("/api/peer/cluster/reconfigure", post(peer::reconfigure))
        // The pool half of the peer surface: one closed verb enum, run
        // against this node's own daemon over its own loopback — the only
        // way a pool daemon is ever addressed from off-box.
        .route("/api/peer/pool/verb", post(peer::pool_verb))
        // The pool workflows' per-member acts: what a node would bring,
        // becoming a member, ceasing to be one, and the reply-first
        // restart that adopts either. Consent never travels these.
        .route("/api/peer/pool/preflight", post(peer::pool_preflight))
        .route("/api/peer/pool/prepare", post(peer::pool_prepare))
        .route("/api/peer/pool/reconf", post(peer::pool_reconf))
        .route("/api/peer/pool/teardown", post(peer::pool_teardown))
        .route(
            "/api/peer/controlplane/restart",
            post(peer::controlplane_restart),
        )
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
        // The LumenFS pool: the observed view, and the snapshot verbs. Disks
        // are addressed by the compute domain's name for them — the device
        // path is the same fact with slashes in it.
        .route("/api/storage/pool", get(storage::pooled_storage))
        .route("/api/storage/pool", post(storage::create_lumen_pool))
        .route("/api/storage/pool", delete(storage::destroy_lumen_pool))
        .route("/api/storage/pool/members", post(storage::grow_lumen_pool))
        .route(
            "/api/storage/pool/pending",
            get(storage::lumen_pool_pending),
        )
        // The integrity pass, started on every member; progress rides the
        // pool view each member already reports.
        .route("/api/storage/pool/scrub", post(storage::scrub_pool))
        .route(
            "/api/storage/pool/disks/{name}",
            delete(storage::delete_pooled_disk),
        )
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
