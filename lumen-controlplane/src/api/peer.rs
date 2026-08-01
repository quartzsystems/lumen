//! The peer surface: one control plane answering another.
//!
//! Every route here except `/api/peer/join` requires a peer ticket — a JWT
//! signed with the environment-shared secret, carrying `kind: peer` so a
//! browser session can never pass as one (and a peer ticket can never open a
//! console). The join route is the sole exception: its caller does not have
//! the secret yet, and the one-time token in its body is the authentication.
//!
//! Handlers stay thin, exactly as the operator-facing ones do: deserialize,
//! one service call, serialize.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::error::ApiError;
use crate::inventory::NodeInventory;
use crate::pool::PoolPresence;
use crate::security::PeerSession;
use crate::AppState;
use lumen_cluster::{
    EnvironmentMembership, JoinGrant, JoinRequest, PreflightReport, PreparePayload, TeardownPayload,
};

/// POST /api/peer/join — the issuer's half of an environment join. The
/// token in the body is the authentication; there is no ticket to hold yet.
pub async fn join(
    State(state): State<Arc<AppState>>,
    Json(request): Json<JoinRequest>,
) -> Result<Json<JoinGrant>, ApiError> {
    let secret = state.jwt_secret.read().expect("secret lock").clone();
    Ok(Json(state.cluster.grant_join(&request, &secret).await?))
}

/// POST /api/peer/membership — gossip. The peer pushes its record, we
/// reconcile and answer with the result.
pub async fn membership(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(remote): Json<EnvironmentMembership>,
) -> Result<Json<EnvironmentMembership>, ApiError> {
    Ok(Json(state.cluster.receive_membership(remote).await?))
}

/// POST /api/peer/preflight — could this node join a cluster right now?
pub async fn preflight(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<PreflightReport>, ApiError> {
    Ok(Json(state.cluster.peer_preflight().await?))
}

/// POST /api/peer/node/inventory — what this node has: its processors and
/// memory, its links, and its pools.
///
/// A read, on a surface that is otherwise all writes, because the console's
/// environment-wide tables need every member's answer and only each member
/// can give its own. One call rather than three: the three reads happen on
/// the same node at the same moment, and splitting them would only let the
/// three halves of one row disagree.
pub async fn inventory(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<NodeInventory>, ApiError> {
    Ok(Json(crate::inventory::local(&state).await))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WipeDiskRequest {
    /// The disk as this node names it — a kernel name, a `/dev` path, or the
    /// by-id path. The storage domain matches all three.
    pub disk: String,
}

/// POST /api/peer/storage/wipe — clear one disk here, on behalf of an
/// operator working from another member's console.
///
/// The acknowledgement is not taken from the wire: consent was given to
/// the console the operator is looking at, and a peer route that accepted
/// it from a body would be a second, quieter way to clear a node's disks.
///
/// Every guard that matters is still this node's. A disk holding a pool, a
/// mount, or swap is refused here regardless of what the caller believes,
/// because this node is the only one that can see what is actually on it.
pub async fn wipe_disk(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(request): Json<WipeDiskRequest>,
) -> Result<Json<lumen_zfs::BlockDevice>, ApiError> {
    Ok(Json(
        state
            .storage
            .wipe_disk(
                &request.disk,
                lumen_zfs::Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await?,
    ))
}

// --- updates -----------------------------------------------------------------

/// POST /api/peer/system/updates — what this node has waiting, and what it is
/// installing.
///
/// Never asks the repositories, exactly as the operator-facing read does not:
/// an environment-wide page load must not turn into one network request per
/// member over links this node knows nothing about.
///
/// Both halves in one answer for the reason `inventory` returns three: they
/// are read here at the same moment, and splitting them would only let the two
/// halves of one row disagree about whether this node is installing something.
pub async fn updates(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::cluster_updates::NodeUpdates>, ApiError> {
    Ok(Json(crate::cluster_updates::NodeUpdates {
        view: state.updates.view().await?,
        progress: state.update_job.get(),
    }))
}

/// POST /api/peer/system/updates/check — ask this node's repositories now, on
/// behalf of an operator working from another member's console.
///
/// A failed check is not an error here. The service records the reason and the
/// view carries it, and a member whose mirror is unreachable still has a
/// reboot state and a previous answer worth putting in the table — the same
/// judgement `GET /api/system/updates` makes about its own page.
pub async fn check_updates(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::cluster_updates::NodeUpdates>, ApiError> {
    if let Err(err) = state.updates.check().await {
        tracing::warn!("a peer asked for a check and the repositories could not be reached: {err}");
    }
    Ok(Json(crate::cluster_updates::NodeUpdates {
        view: state.updates.view().await?,
        progress: state.update_job.get(),
    }))
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerApplyRequest {
    #[serde(default)]
    pub platform: bool,
    /// Who asked, as the coordinator knows them.
    ///
    /// Taken from the wire, unlike the acknowledgement below it, and the
    /// difference is the point: this is a label for the journal, not a
    /// permission. The peer ticket is what authorizes the call at all; what
    /// this buys is that an operator reading node-b's transaction sees the
    /// person who pressed the button rather than only the node that relayed
    /// it. Absent, or from a caller that sends nothing, it falls back to the
    /// calling node's name.
    #[serde(default)]
    pub by: Option<String>,
}

/// POST /api/peer/system/updates/apply — start installing here, on behalf of
/// an operator working from another member's console.
///
/// The acknowledgement is not taken from the wire, for the same reason
/// `create_pool` and `wipe_disk` do not take theirs: consent was given to the
/// console the operator is looking at, and a peer route that accepted "yes,
/// move the kernel" from a body would be a second, quieter way to replace a
/// node's kernel.
///
/// Every guard that matters is still this node's. Whether the platform set
/// resolves *here* is a question only this node's package manager can answer,
/// and it is asked here, against this node's repositories — the coordinator's
/// belief about it carries no weight.
pub async fn apply_updates(
    peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(request): Json<PeerApplyRequest>,
) -> Result<Json<crate::updates::UpdateProgress>, ApiError> {
    let by = match request.by.as_deref() {
        Some(principal) => format!("{principal} via {}", peer.0),
        None => peer.0.clone(),
    };
    Ok(Json(
        crate::updates::begin(
            &state,
            &by,
            lumen_update::ApplyRequest {
                platform: request.platform,
                i_understand_the_kernel_moves: request.platform,
            },
        )
        .await?,
    ))
}

// --- maintenance and power, for a rolling update -----------------------------
//
// These four exist for one caller: the rolling update in
// `src/cluster_updates.rs`, which takes each member through maintenance,
// installs its kernel, restarts it, and waits for it to rejoin.
//
// They are deliberately *not* a peer copy of the operator-facing maintenance
// routes, which refuse to act on any node but their own ("its machines can
// only be moved by the node running them"). That rule is unchanged and is why
// these exist: the work still happens here, on the node it is about, through
// the same `crate::maintenance` entry points the local console calls. What
// crosses the wire is only the instruction to begin.

/// POST /api/peer/system/maintenance — take this node out of service and
/// drain it, on behalf of a rolling update running elsewhere.
///
/// Answers as soon as the node is flagged and the drain is published; the
/// machines are still moving. The caller polls [`drain_progress`].
///
/// Every guard is this node's. Whether the cluster can spare it, whether its
/// machines have anywhere to go, and which member each one may move to are all
/// decided here, because this is the only node that can see them.
pub async fn enter_maintenance(
    peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(request): Json<PeerMaintenanceRequest>,
) -> Result<Json<crate::maintenance::MaintenanceProgress>, ApiError> {
    let by = match request.by.as_deref() {
        Some(principal) => format!("{principal} via {}", peer.0),
        None => peer.0.clone(),
    };
    Ok(Json(crate::maintenance::begin(&state, &by, true).await?))
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerMaintenanceRequest {
    /// Who asked. A label for this node's own record, as on the apply route.
    #[serde(default)]
    pub by: Option<String>,
}

/// POST /api/peer/system/maintenance/progress — the drain, while there is one.
pub async fn drain_progress(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Json<Option<crate::maintenance::MaintenanceProgress>> {
    Json(state.drain.get())
}

/// POST /api/peer/system/maintenance/exit — put this node back into service.
///
/// Machines that were evacuated stay where they went. Failback is an
/// operator's decision here exactly as it is after an HA restart, and a
/// rolling update that moved work twice per node would take twice as long and
/// twice the risk to end up where it started.
pub async fn exit_maintenance(
    peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(request): Json<PeerMaintenanceRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let by = match request.by.as_deref() {
        Some(principal) => format!("{principal} via {}", peer.0),
        None => peer.0.clone(),
    };
    crate::maintenance::end(&state, &by).await?;
    Ok(Json(serde_json::json!({ "in_service": true })))
}

/// POST /api/peer/node/power — cut or cycle a member's power through THIS
/// node's fence device.
///
/// Exists for one caller: a node asked to hard-power itself. It cannot — the
/// answer would go down with it — so its control plane relays the request to
/// a member whose fence path reaches the target's BMC. The acknowledgement
/// was collected on the node the operator asked; what crosses the wire is an
/// instruction already acknowledged.
pub async fn power(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(request): Json<PeerPowerRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let action = match request.action {
        crate::api::cluster::NodePowerAction::Off => lumen_cluster::HardPower::Off,
        crate::api::cluster::NodePowerAction::Cycle => lumen_cluster::HardPower::Cycle,
    };
    state
        .cluster
        .power_member(&request.node, action, true)
        .await?;
    Ok(Json(
        serde_json::json!({ "node": request.node, "action": request.action }),
    ))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerPowerRequest {
    pub node: String,
    #[serde(default)]
    pub action: crate::api::cluster::NodePowerAction,
}

/// POST /api/peer/system/restart — restart this node now.
///
/// The quorum guard is the same one the operator-facing power route uses, and
/// it is called here rather than trusted to the caller. A rolling update
/// reaches this having already put the node into maintenance, which is one of
/// the three ways past that guard — so the guard passing is evidence the
/// sequence was followed, not a formality skipped.
///
/// There is no acknowledgement to override it with. An operator who wants to
/// take down a node their cluster cannot spare can still do it, on that node's
/// own power page, where they are the ones being told what it costs.
pub async fn restart(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::api::system::guard_cluster_power(&state, false).await?;
    state
        .sys
        .power_now(lumen_sys::model::PowerAction::Reboot)
        .await?;
    Ok(Json(serde_json::json!({ "restarting": true })))
}

/// POST /api/peer/cluster/prepare — realize this node's Core seat and write
/// the cluster configuration.
pub async fn prepare(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<PreparePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_prepare(&payload).await?;
    Ok(Json(serde_json::json!({ "prepared": true })))
}

/// POST /api/peer/network/bond — build a bond here, through this node's own
/// networking domain. The create wizard's Core-redundancy shortcut; the bond
/// that results is an ordinary link, owned and edited by Networking.
pub async fn create_bond(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(bond): Json<lumen_net::Bond>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_create_bond(&bond).await?;
    Ok(Json(serde_json::json!({ "bonded": true })))
}

/// POST /api/peer/network/bridge — build an External network's bridge here,
/// through this node's own networking domain. Like the bond above, what comes
/// out is an ordinary link that outlives the cluster.
pub async fn create_bridge(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(seat): Json<lumen_cluster::join::ExternalSeat>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_create_bridge(&seat).await?;
    Ok(Json(serde_json::json!({ "built": true })))
}

/// POST /api/peer/cluster/start — enable and start the cluster stack.
pub async fn start(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_start().await?;
    Ok(Json(serde_json::json!({ "started": true })))
}

/// POST /api/peer/cluster/teardown — put this node back exactly as it was.
pub async fn teardown(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<TeardownPayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_teardown(&payload).await?;
    Ok(Json(serde_json::json!({ "torn_down": true })))
}

/// POST /api/peer/cluster/reconfigure — take a regenerated configuration
/// and reload corosync, live. The scale-out's reach into a running member.
pub async fn reconfigure(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<lumen_cluster::join::ReconfigurePayload>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_reconfigure(&payload).await?;
    Ok(Json(serde_json::json!({ "reconfigured": true })))
}

// --- replicated machine definitions -----------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct DefinitionRef {
    pub vmid: u32,
}

/// POST /api/peer/definition/store — keep a machine's definition, home node
/// included, so an HA restart can define it here after its node dies.
pub async fn store_definition(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<lumen_cluster::StoredDefinition>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_store_definition(&payload)?;
    Ok(Json(serde_json::json!({ "stored": true })))
}

/// POST /api/peer/definition/drop — forget one.
pub async fn drop_definition(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<DefinitionRef>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.peer_drop_definition(payload.vmid)?;
    Ok(Json(serde_json::json!({ "dropped": true })))
}

/// POST /api/peer/pool/verb — run one pool verb against **this node's own
/// daemon**, on behalf of another member's control plane.
///
/// This route exists because the daemon's control surface is loopback-only
/// by design: `fence-peer` lives on it, and a fence verdict arriving from
/// off-box is not something the appliance accepts. So a member that needs a
/// device exported *here* — a migration destination, a rollback checking
/// exports — asks this control plane, and this control plane speaks to its
/// own daemon over its own loopback. Each daemon is only ever addressed by
/// the machine it runs on.
///
/// The verb is a closed enum, deserialized before anything runs: a peer can
/// ask for one of the named verbs and nothing else.
pub async fn pool_verb(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(verb): Json<lumen_pool::PoolVerb>,
) -> Result<Json<lumen_pool::PoolAnswer>, ApiError> {
    let PoolPresence::Present { control, .. } = &state.pool else {
        // Absent and Broken answer the same sentence: either way there is
        // no daemon here to run the verb, and the pool-wide caller treats
        // both as this member refusing.
        return Err(ApiError::Conflict(
            "this node carries no pool daemon".into(),
        ));
    };
    let control = *control;
    let answer = tokio::task::spawn_blocking(move || {
        // Blocking client, blocking pool — the same discipline as the
        // fleet's own local calls.
        let mut client = lumen_pool::Client::connect(control)
            .map_err(|err| format!("this node's pool daemon did not answer: {err}"))?;
        lumen_pool::execute(&mut client, &verb)
    })
    .await
    .map_err(|err| ApiError::Internal(anyhow::anyhow!("the pool verb never ran: {err}")))?
    .map_err(ApiError::Conflict)?;
    Ok(Json(answer))
}

/// POST /api/peer/pool/preflight — what this node would bring to a pool,
/// read fresh: its drop-in state, its ublk device, its disks as its own
/// scanner sees them right now.
pub async fn pool_preflight(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::pool_workflow::PoolNodeFacts>, ApiError> {
    Ok(Json(crate::pool_workflow::local_preflight(&state).await?))
}

/// POST /api/peer/pool/prepare — become a pool member, on this node's own
/// disks and through its own guards.
///
/// No acknowledgement is taken from the wire: the operator's consent was
/// given to the coordinator, and a peer route that accepted "yes, erase
/// them" from a body would be a second, quieter way to reformat a node.
pub async fn pool_prepare(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(prepare): Json<crate::pool_workflow::PoolPrepare>,
) -> Result<Json<crate::pool_workflow::PoolPrepared>, ApiError> {
    Ok(Json(
        crate::pool_workflow::local_prepare(&state, &prepare).await?,
    ))
}

/// POST /api/peer/pool/reconf — a serving member takes the grow's conf
/// change: one more dial, the grown member list, a daemon restart. The
/// member patches its own conf; its brick paths never cross the wire.
pub async fn pool_reconf(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
    Json(reconf): Json<crate::pool_workflow::PoolReconf>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::pool_workflow::local_reconf(&state, &reconf).await?;
    Ok(Json(serde_json::json!({ "reconfigured": true })))
}

/// POST /api/peer/pool/teardown — stop carrying a pool: daemon down,
/// bricks wiped, drop-in removed. The destroy workflow and the create
/// workflow's unwind share this one definition.
pub async fn pool_teardown(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::pool_workflow::local_teardown(&state).await?;
    Ok(Json(serde_json::json!({ "cleared": true })))
}

/// POST /api/peer/controlplane/restart — reply first, restart after.
///
/// `PoolPresence` is resolved once at startup, so adopting a created or
/// destroyed pool means restarting this process. The restart runs as a
/// detached transient unit *after* the reply is on the wire, so the
/// coordinator hears "restarting" from a control plane that then actually
/// goes away. Distinct from `/api/peer/restart`, which reboots the node.
pub async fn controlplane_restart(
    _peer: PeerSession,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let deploy = state.pool_deploy.clone();
    tokio::spawn(async move {
        // A beat for the reply to leave the socket.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        if let Err(err) = deploy.restart_controlplane().await {
            tracing::warn!("the requested control plane restart failed: {err}");
        }
    });
    Ok(Json(serde_json::json!({ "restarting": true })))
}
