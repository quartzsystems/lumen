//! Thin HTTP handlers over `lumen_cluster::ClusterService`.
//!
//! The reads present the environment, grouped by cluster and then by node.
//! The writes are the environment's workflows: minting a join token (which
//! bootstraps the environment on first use), joining, building a cluster —
//! polled per node, per step — and destroying one. The two pieces the
//! handlers own, because the control plane owns them everywhere: swapping
//! the live session secret after a join, and reloading the TLS listener onto
//! the environment certificate.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::api::request::{body, required_body, Body};
use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;
use lumen_cluster::{
    Acknowledgements, ClusterCreate, ClusterNetworks, ClusterView, CreateProgress,
    EnvironmentResponse, ExternalNetwork, FenceTestView, MintedToken, PreflightView,
};

/// GET /api/environment — the whole environment: every cluster, every node,
/// and the unassigned nodes, in one answer. A node that never joined an
/// environment answers with itself as the one unassigned node.
pub async fn environment(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<EnvironmentResponse>, ApiError> {
    Ok(Json(state.cluster.environment().await?))
}

/// GET /api/environment/clusters/{name} — one cluster, the same view the
/// environment answer carries.
pub async fn cluster(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<ClusterView>, ApiError> {
    Ok(Json(state.cluster.cluster(&name).await?))
}

/// GET /api/environment/inventory — every member's processors, memory,
/// links, and pools, side by side.
///
/// The environment-wide read behind the console's cross-node tables. The
/// node-local routes it draws from — `/api/nodes`,
/// `/api/network/interfaces`, `/api/storage/pools` — keep meaning "this
/// appliance", because the edits the console offers on a link or a pool land
/// on the node that owns it.
///
/// Always 200. A member that could not be asked is a row carrying its reason,
/// not an error that costs the operator the members that did answer.
pub async fn inventory(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Json<crate::inventory::InventoryResponse> {
    Json(crate::inventory::environment(&state).await)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WipeDiskRequest {
    #[serde(default)]
    pub i_understand_this_may_lose_data: bool,
}

/// POST /api/environment/nodes/{node}/disks/{disk}/wipe — clear one member's
/// disk so a pool can be built on it.
///
/// The node rides the path for the same reason the bond route's does: a
/// `node` field in a body means "this appliance" everywhere else in this API,
/// and this route means a member that is deliberately not necessarily us.
///
/// Answers with the disk as the owning node now reports it, so the picker
/// that could not offer it a moment ago can offer it without a second read.
pub async fn wipe_node_disk(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((node, disk)): Path<(String, String)>,
    raw: Body,
) -> Result<Json<lumen_zfs::BlockDevice>, ApiError> {
    let request: WipeDiskRequest = body(raw)?;
    Ok(Json(
        crate::inventory::wipe_node_disk(
            &state,
            &node,
            &disk,
            request.i_understand_this_may_lose_data,
        )
        .await?,
    ))
}

/// POST /api/environment/nodes/{node}/power — cut or cycle a member's power
/// through its fence device.
///
/// The path that does not need the target's cooperation, and the reason it
/// exists: a node whose operating system is wedged cannot be restarted by
/// asking it, and every graceful route — this console's own power page, the
/// peer restart the rolling update uses — asks. Its BMC still answers, and
/// the cluster already holds those credentials for fencing.
///
/// The node rides the path for the same reason the wipe route's does: a
/// `node` field in a body means "this appliance" everywhere else in this
/// API, and this route means a member that is deliberately not us. The
/// guards, the refusal to do this to the node serving the request included,
/// are the cluster domain's.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodePowerRequest {
    #[serde(default)]
    pub action: NodePowerAction,
    #[serde(default)]
    pub i_understand_this_cuts_the_power: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodePowerAction {
    /// Cut the power and leave it off.
    #[default]
    Off,
    /// Power-cycle it: off, then on again.
    Cycle,
}

pub async fn power_node(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(node): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: NodePowerRequest = body(raw)?;
    let action = match request.action {
        NodePowerAction::Off => lumen_cluster::HardPower::Off,
        NodePowerAction::Cycle => lumen_cluster::HardPower::Cycle,
    };
    // A node cannot command its own BMC — the answer would go down with it —
    // so the request rides a cluster-mate's fence path instead. The
    // acknowledgement is demanded here first, so the relay never carries an
    // unacknowledged cut.
    if node == state.cluster.node() {
        if !request.i_understand_this_cuts_the_power {
            return Err(ApiError::Conflict(
                "This takes the power away at the machine — every virtual machine on it stops \
                 where it is, with no shutdown. Acknowledge that first."
                    .to_string(),
            ));
        }
        let record = state.cluster.environment_record()?.ok_or_else(|| {
            ApiError::Conflict("This node has not joined an environment.".to_string())
        })?;
        let assignment = record
            .node(&node)
            .and_then(|n| n.cluster.clone())
            .ok_or_else(|| {
                ApiError::Conflict(format!(
                    "\"{node}\" is not in a cluster, so no member holds a fence device for it."
                ))
            })?;
        let mates: Vec<_> = record
            .nodes
            .iter()
            .filter(|n| n.cluster.as_deref() == Some(assignment.as_str()) && n.name != node)
            .collect();
        if mates.is_empty() {
            return Err(ApiError::Conflict(format!(
                "\"{node}\" is the only member of its cluster — no other member can reach its BMC."
            )));
        }
        // Any mate's fence path will do; the first that answers wins, and
        // the last refusal is the one worth reporting when none does.
        let mut refusal = None;
        for mate in mates {
            match state.peers.power(mate, &node, action).await {
                Ok(()) => {
                    return Ok(Json(serde_json::json!({
                        "node": node,
                        "action": request.action,
                        "via": mate.name,
                    })))
                }
                Err(err) => refusal = Some(err),
            }
        }
        return Err(refusal.expect("at least one mate was tried").into());
    }
    state
        .cluster
        .power_member(&node, action, request.i_understand_this_cuts_the_power)
        .await?;
    Ok(Json(
        serde_json::json!({ "node": node, "action": request.action }),
    ))
}

/// GET /api/environment/clusters/{name}/networks — the cluster's typed
/// networks: Core, Management, and the External list, as the replicated
/// record carries them. The console's Networks page reads this.
pub async fn cluster_networks(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<ClusterNetworks>, ApiError> {
    Ok(Json(state.cluster.cluster_networks(&name)?))
}

/// POST /api/environment/clusters/{name}/networks/external — define an
/// External network and build its bridge on every member.
///
/// One call rather than a definition write and a realization pass, because
/// the two must not be separable: an External network the record claims and
/// a member has not built is exactly the state the consistency rule forbids.
/// Answers with the network as recorded, so the console renders what the
/// cluster actually agreed to rather than what was typed.
pub async fn create_external_network(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<(StatusCode, Json<ExternalNetwork>), ApiError> {
    let network: ExternalNetwork = required_body(raw)?;
    let created = state
        .cluster
        .create_external_network(&name, network)
        .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// PUT /api/environment/clusters/{name}/networks/core — change the Core
/// network without destroying the cluster: the MTU, and which link carries
/// each member's seat.
///
/// The subnet and the per-member addresses are deliberately not changeable
/// here — they are corosync's ring 0 addressing, and the request shape
/// cannot carry a new subnet at all. Members change one at a time through
/// their own networking domains, record last; see
/// `ClusterService::update_core_network`.
pub async fn update_core_network(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<lumen_cluster::CoreNetwork>, ApiError> {
    let update: lumen_cluster::CoreNetworkUpdate = required_body(raw)?;
    Ok(Json(state.cluster.update_core_network(&name, update).await?))
}

/// PUT /api/environment/clusters/{name}/networks/external/{network} — change
/// an External network and rebuild it on every member.
///
/// The name is in the path and not changeable through it: it is what a
/// machine's adapter refers to, so renaming would strand every machine on the
/// network. Everything else — the bridge, the VLAN semantics, each member's
/// uplink — moves here.
pub async fn update_external_network(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((name, network)): Path<(String, String)>,
    raw: Body,
) -> Result<Json<ExternalNetwork>, ApiError> {
    let wanted: ExternalNetwork = required_body(raw)?;
    Ok(Json(
        state
            .cluster
            .update_external_network(&name, &network, wanted)
            .await?,
    ))
}

/// DELETE /api/environment/clusters/{name}/networks/external/{network} —
/// forget an External network.
///
/// The definition goes; the bridges the members built stay. They are ordinary
/// links with machines possibly still on them, and Interfaces is the page that
/// can say what is still using one.
pub async fn forget_external_network(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((name, network)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .cluster
        .forget_external_network(&name, &network)
        .await?;
    Ok(Json(serde_json::json!({
        "removed": true,
        "note": "The bridges on each member were left in place — machines may still be attached \
                 to them. Remove them per node on Networking → Interfaces."
    })))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetVipRequest {
    /// The new address, or `null` to take the cluster VIP away.
    #[serde(default)]
    pub address: Option<std::net::Ipv4Addr>,
    /// The operator has been told this drops the address the console may
    /// itself be reached on.
    #[serde(default)]
    pub i_understand_this_may_disconnect_me: bool,
}

/// PUT /api/environment/clusters/{name}/vip — move the cluster VIP, or
/// take it away with a `null` address.
///
/// Guarded by an acknowledgement rather than by a refusal, because there is no
/// safe version of this: the old address comes down before the new one goes
/// up, and a session held on the VIP loses its connection in between. The
/// members' own addresses stay valid throughout, which is what makes that
/// recoverable rather than a lockout — and is what the console says before
/// asking.
pub async fn set_vip(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: SetVipRequest = required_body(raw)?;
    if !request.i_understand_this_may_disconnect_me {
        return Err(ApiError::Conflict(
            "Changing the cluster VIP takes the old one down before the new one comes up. \
             If this console is reached on it, the connection drops mid-operation — the change \
             still completes, and each member's own address still works. Acknowledge that first."
                .to_string(),
        ));
    }
    let vip = state.cluster.set_vip(&name, request.address).await?;
    Ok(Json(serde_json::json!({ "vip": vip })))
}

/// POST /api/environment/clusters/{name}/vip/recover — clear the cluster
/// address's recorded failures and let Pacemaker probe it again.
///
/// The other half of a fault the console can only otherwise describe.
/// Pacemaker latches a failed operation, so an address stopped with "Not
/// installed" stays stopped after the missing piece is installed — nothing
/// asks again on its own. This asks again.
///
/// Answers with the address as Pacemaker has it immediately afterwards, not
/// with a success flag: the probe decides the outcome, and a recovery run
/// before the cause was fixed answers with the same failure it started with.
pub async fn recover_vip(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<lumen_cluster::VipView>, ApiError> {
    Ok(Json(state.cluster.recover_vip(&name).await?))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintTokenRequest {
    /// Where the joining node should dial, `host:port`. Defaults to this
    /// node's own management address — the override exists for a node
    /// reached through a NAT the default could not know about.
    #[serde(default)]
    pub address: Option<String>,
}

/// POST /api/environment/tokens — mint a one-time join token. The first
/// mint on a fresh node bootstraps the environment, and the listener starts
/// serving the environment certificate the same moment — before the token
/// leaves this node, because the token pins that certificate.
pub async fn mint_token(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<MintedToken>, ApiError> {
    let request: MintTokenRequest = body(raw)?;
    let address = match request.address {
        Some(address) => address,
        None => local_address(&state).await?,
    };
    let minted = state.cluster.mint_token(&address).await?;
    if minted.bootstrapped {
        adopt_environment_tls(&state).await;
    }
    Ok(Json(minted))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinBody {
    /// The pasted token, whole.
    pub token: String,
    /// This node's own `host:port`, when the default derivation would name
    /// the wrong interface.
    #[serde(default)]
    pub address: Option<String>,
}

/// POST /api/environment/join — join an environment with a pasted token.
/// The session secret becomes the environment's, which signs every session
/// out — including the operator's own. The answer says so, because the
/// login page appearing right after is the join *working*, and it must not
/// read as a failure.
pub async fn join(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: JoinBody = required_body(raw)?;
    let address = match request.address {
        Some(address) => address,
        None => local_address(&state).await?,
    };
    let outcome = state.cluster.join(&request.token, &address).await?;

    // The environment's secret replaces this node's, live and on disk, so a
    // restart keeps the environment sessions rather than resurrecting the
    // old ones.
    {
        let mut secret = state.jwt_secret.write().expect("secret lock");
        *secret = outcome.session_secret.clone();
    }
    if let Err(err) = crate::security::store_secret(
        &state.config.state_dir.join("session-secret"),
        &outcome.session_secret,
    ) {
        tracing::error!("the environment secret did not persist: {err:#}");
    }
    adopt_environment_tls(&state).await;

    Ok(Json(serde_json::json!({
        "joined": true,
        "note": "This console's sessions are now the environment's. Sign in again — the same \
                 password works on every environment node."
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightRequest {
    pub nodes: Vec<String>,
}

/// POST /api/environment/preflight — could these nodes form a cluster right
/// now? The wizard's first page, with per-node reasons and each node's
/// links for the NIC pickers.
pub async fn preflight(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<Json<Vec<PreflightView>>, ApiError> {
    let request: PreflightRequest = required_body(raw)?;
    Ok(Json(state.cluster.preflight(&request.nodes).await?))
}

/// POST /api/environment/nodes/{node}/bond — build a bond on one environment
/// node, before it is a cluster member. The wizard's shortcut to a Core seat
/// that survives a cable: it lands in the target node's networking domain, so
/// what comes out is an ordinary link, edited and deleted on its Networking
/// page.
///
/// The target rides the path, not the body: a `node` field in a body means
/// "this appliance" everywhere else in this API (see `check_node`), and this
/// route means the opposite — a member that is deliberately not us.
pub async fn bond_node_nics(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(node): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let bond: lumen_net::Bond = required_body(raw)?;
    state.cluster.bond_node_nics(&node, &bond).await?;
    Ok(Json(serde_json::json!({ "bonded": true })))
}

/// POST /api/environment/clusters — start a create. Validation answers now;
/// the workflow runs in the background and `GET
/// /api/environment/clusters/pending` is the wizard's progress feed.
pub async fn create_cluster(
    _session: Session,
    State(state): State<Arc<AppState>>,
    raw: Body,
) -> Result<(StatusCode, Json<CreateProgress>), ApiError> {
    let request: ClusterCreate = required_body(raw)?;
    let progress = state.cluster.create_cluster(request).await?;
    Ok((StatusCode::ACCEPTED, Json(progress)))
}

/// GET /api/environment/clusters/pending — the create in flight, or the
/// last one finished. 404 when none was ever started.
pub async fn create_progress(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Result<Json<CreateProgress>, ApiError> {
    state
        .cluster
        .create_progress()
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("No cluster create has been started.".to_string()))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestroyRequest {
    #[serde(default)]
    pub i_understand_this_may_lose_data: bool,
}

impl DestroyRequest {
    fn ack(&self) -> Acknowledgements {
        Acknowledgements {
            may_lose_data: self.i_understand_this_may_lose_data,
        }
    }
}

/// DELETE /api/environment/clusters/{name} — tear the cluster down on every
/// member and forget it. Every member must clean up for the record to
/// change; a destroy that loses a node halfway reports it and leaves the
/// cluster recorded.
pub async fn destroy_cluster(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: DestroyRequest = body(raw)?;
    state.cluster.destroy_cluster(&name, request.ack()).await?;
    Ok(Json(serde_json::json!({ "destroyed": true })))
}

/// POST /api/environment/clusters/{name}/nodes — the 2→3 scale-out: an
/// unassigned environment node joins a *running* cluster. Validation
/// answers now; the workflow runs in the background and the create's
/// pending feed is its progress too.
pub async fn add_node(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    raw: Body,
) -> Result<(StatusCode, Json<CreateProgress>), ApiError> {
    // Deserialized directly, not through `required_body`: its `node` field
    // names the node being *added*, which is exactly what the cross-node
    // guard would strip and refuse.
    let Some(value) = raw.into_value() else {
        return Err(ApiError::BadRequest("A request body is required.".into()));
    };
    let request: lumen_cluster::MemberCreate =
        serde_json::from_value(value).map_err(|err| ApiError::BadRequest(err.to_string()))?;
    let plan = state.cluster.prepare_add_node(&name, request).await?;
    let progress = state
        .cluster
        .create_progress()
        .expect("prepare_add_node begins the progress");
    let state = state.clone();
    tokio::spawn(async move {
        let _ = state.cluster.execute_add_node(plan).await;
    });
    Ok((StatusCode::ACCEPTED, Json(progress)))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FenceTestRequest {
    #[serde(default)]
    pub i_understand_this_power_cycles_the_node: bool,
}

/// POST /api/environment/clusters/{name}/fence/{node}/test — a guarded live
/// fence test: the target really power-cycles through its BMC. The service
/// refuses it without the acknowledgement, on an unhealthy cluster, and from
/// the target's own console; the outcome is recorded on the membership
/// record either way, because a failed test is an answer, not an error.
pub async fn test_fence(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((name, node)): Path<(String, String)>,
    raw: Body,
) -> Result<Json<FenceTestView>, ApiError> {
    let request: FenceTestRequest = body(raw)?;
    Ok(Json(
        state
            .cluster
            .test_fence(
                &name,
                &node,
                request.i_understand_this_power_cycles_the_node,
            )
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmDeadRequest {
    #[serde(default)]
    pub i_have_verified_the_node_is_powered_off: bool,
}

/// POST /api/environment/clusters/{name}/nodes/{node}/confirm-dead — the
/// break-glass: the operator vouches that an unfenced-unreachable node is
/// powered off, and the cluster recovers as if fencing succeeded. Offered in
/// exactly that one state and no other.
pub async fn confirm_node_dead(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path((name, node)): Path<(String, String)>,
    raw: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let request: ConfirmDeadRequest = body(raw)?;
    state
        .cluster
        .confirm_node_dead(
            &name,
            &node,
            request.i_have_verified_the_node_is_powered_off,
        )
        .await?;
    Ok(Json(serde_json::json!({ "confirmed": true })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceRequest {
    /// Move the running machines off before answering. Default on: a node
    /// declared out of service with its machines still on it is the surprising
    /// outcome, not the expected one. `false` is for the operator who is about
    /// to shut those machines down anyway.
    #[serde(default = "yes")]
    pub evacuate: bool,
}

fn yes() -> bool {
    true
}

/// Written out rather than derived, and that is the whole point: a request
/// with no body at all is deserialized by `body()` as `T::default()`, which
/// never runs serde's field defaults. Deriving this would silently turn the
/// bodiless "put this node into maintenance" into "and leave the machines on
/// it" — the opposite of what the field says.
impl Default for MaintenanceRequest {
    fn default() -> Self {
        MaintenanceRequest { evacuate: true }
    }
}

/// POST /api/environment/clusters/{name}/nodes/{node}/maintenance — take the
/// node out of service, and drain it unless told not to.
///
/// The work always happens on the node it is about — its machines can only
/// be moved by the node running them. When that node is not this one, what
/// crosses the wire is only the instruction to begin, over the same peer
/// verbs the rolling update uses; every guard is the target's own.
///
/// 202 with the drain's first progress: the flag and standby are done by the
/// time this answers, the machines are still moving. Poll
/// `GET /api/environment/maintenance?node={node}`.
pub async fn enter_maintenance(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path((name, node)): Path<(String, String)>,
    raw: Body,
) -> Result<(StatusCode, Json<crate::maintenance::MaintenanceProgress>), ApiError> {
    let request: MaintenanceRequest = body(raw)?;
    let member = member_of(&state, &name, &node)?;
    if node != state.cluster.node() {
        // The peer verb always drains: holding a node out of service
        // without moving its machines is only useful on the node one is
        // standing on, and the verb exists for the drain.
        if !request.evacuate {
            return Err(ApiError::Conflict(format!(
                "Maintenance without a drain runs on the node it is about — open the console \
                 of \"{node}\"."
            )));
        }
        let progress = state
            .peers
            .enter_maintenance(&member, &principal(&session))
            .await?;
        return Ok((StatusCode::ACCEPTED, Json(progress)));
    }
    let progress =
        crate::maintenance::begin(&state, &principal(&session), request.evacuate).await?;
    let status = if request.evacuate {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(progress)))
}

/// DELETE /api/environment/clusters/{name}/nodes/{node}/maintenance — put the
/// node back into service. Machines that were evacuated stay where they went;
/// failback is an operator's decision, the same as it is after an HA restart.
pub async fn exit_maintenance(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path((name, node)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let member = member_of(&state, &name, &node)?;
    if node != state.cluster.node() {
        state
            .peers
            .exit_maintenance(&member, &principal(&session))
            .await?;
        return Ok(Json(serde_json::json!({ "in_service": true })));
    }
    let view = crate::maintenance::end(&state, &principal(&session)).await?;
    Ok(Json(serde_json::to_value(view).unwrap_or_else(
        |_| serde_json::json!({ "in_service": true }),
    )))
}

/// The drain query: which node's drain, when not this one's.
#[derive(Debug, Default, Deserialize)]
pub struct DrainQuery {
    pub node: Option<String>,
}

/// GET /api/environment/maintenance — the drain of the named node (this one
/// when unnamed), while there is one to report. `null` when that node has
/// never been drained since its control plane started.
pub async fn drain_progress(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Query(query): Query<DrainQuery>,
) -> Result<Json<Option<crate::maintenance::MaintenanceProgress>>, ApiError> {
    match query.node {
        Some(ref node) if node != state.cluster.node() => {
            let record = state.cluster.environment_record()?.ok_or_else(|| {
                ApiError::Conflict("This node has not joined an environment.".to_string())
            })?;
            let member = record.node(node).cloned().ok_or_else(|| {
                ApiError::Conflict(format!("\"{node}\" is not in the environment."))
            })?;
            Ok(Json(state.peers.drain_progress(&member).await?))
        }
        _ => Ok(Json(state.drain.get())),
    }
}

/// Both maintenance routes carry a cluster and a node in the path for the
/// same reason every other cluster route does — so the URL says what it is
/// about. This checks the pairing is real and hands back the member's record
/// entry: the address a remote instruction travels to.
fn member_of(
    state: &AppState,
    cluster: &str,
    node: &str,
) -> Result<lumen_cluster::EnvironmentNode, ApiError> {
    let membership = state.cluster.environment_record()?.ok_or_else(|| {
        ApiError::Conflict("This node has not joined an environment.".to_string())
    })?;
    let Some(member) = membership.node(node) else {
        return Err(ApiError::Conflict(format!(
            "\"{node}\" is not in the environment."
        )));
    };
    match member.cluster.as_deref() {
        Some(actual) if actual == cluster => Ok(member.clone()),
        Some(actual) => Err(ApiError::Conflict(format!(
            "\"{node}\" is a member of \"{actual}\", not \"{cluster}\"."
        ))),
        None => Err(ApiError::Conflict(format!(
            "\"{node}\" is not in a cluster."
        ))),
    }
}

fn principal(session: &Session) -> String {
    format!("{}@{}", session.0.sub, session.0.realm)
}

/// DELETE /api/environment/nodes/{name} — remove an unassigned node from
/// the environment.
pub async fn remove_node(
    _session: Session,
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.cluster.remove_node(&name).await?;
    Ok(Json(serde_json::json!({ "removed": true })))
}

/// This node's `host:port` as a joining or joined peer should dial it: the
/// management address the networking domain observes, and the port this
/// listener is on.
async fn local_address(state: &AppState) -> Result<String, ApiError> {
    let port = state
        .config
        .listen
        .rsplit_once(':')
        .map(|(_, port)| port)
        .unwrap_or("8443");
    let observed = state.network.observe().await.map_err(ApiError::from)?;
    let host = observed
        .addressed()
        .find(|link| link.state.is_up())
        .or_else(|| observed.addressed().next())
        .and_then(|link| link.addresses.first())
        .and_then(|cidr| cidr.split('/').next())
        .map(str::to_string)
        .ok_or_else(|| {
            ApiError::Conflict(
                "This node has no management address yet, so peers could not reach it. \
                 Configure networking first, or pass \"address\" explicitly."
                    .to_string(),
            )
        })?;
    Ok(format!("{host}:{port}"))
}

/// Point the TLS listener at the environment's certificate, once one exists.
/// Best-effort: a reload failure is loud in the journal but must not fail
/// the operation that caused it — the old certificate keeps serving.
async fn adopt_environment_tls(state: &AppState) {
    let Some(tls) = &state.tls else {
        return;
    };
    let (cert, key) = state.cluster.serving_cert_paths();
    if !cert.is_file() {
        return;
    }
    match tls.reload_from_pem_file(&cert, &key).await {
        Ok(()) => tracing::info!("serving the environment certificate"),
        Err(err) => tracing::error!("could not reload TLS onto the environment certificate: {err}"),
    }
}
