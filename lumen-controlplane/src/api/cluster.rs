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

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::api::request::{body, required_body, Body};
use crate::error::ApiError;
use crate::security::Session;
use crate::AppState;
use lumen_cluster::{
    Acknowledgements, ClusterCreate, ClusterNetworks, ClusterView, CreateProgress,
    EnvironmentResponse, FenceTestView, MintedToken, PreflightView,
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
/// pending feed is its progress too. When the regime flips, the volume
/// replication policies are refreshed right after — chained here, because
/// the cluster domain cannot reach the storage domain above it.
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
    let cluster_name = name.clone();
    let state = state.clone();
    tokio::spawn(async move {
        if state.cluster.execute_add_node(plan).await.is_ok() {
            if let Err(err) = state.drbd.refresh_policies(&cluster_name).await {
                tracing::error!(
                    cluster = %cluster_name,
                    "the node joined but the volume policies did not refresh — run the \
                     scale-out's policy refresh again: {err}"
                );
            }
        }
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

/// POST /api/environment/clusters/{name}/nodes/{node}/maintenance — take this
/// node out of service, and drain it unless told not to.
///
/// 202 with the drain's first progress: the flag and standby are done by the
/// time this answers, the machines are still moving. Poll
/// `GET /api/environment/maintenance`.
pub async fn enter_maintenance(
    session: Session,
    State(state): State<Arc<AppState>>,
    Path((name, node)): Path<(String, String)>,
    raw: Body,
) -> Result<(StatusCode, Json<crate::maintenance::MaintenanceProgress>), ApiError> {
    let request: MaintenanceRequest = body(raw)?;
    guard_cluster(&state, &name, &node)?;
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
) -> Result<Json<lumen_cluster::MaintenanceView>, ApiError> {
    guard_cluster(&state, &name, &node)?;
    Ok(Json(
        crate::maintenance::end(&state, &principal(&session)).await?,
    ))
}

/// GET /api/environment/maintenance — the drain of this node, while there is
/// one to report. `null` when this node has never been drained since the
/// control plane started.
pub async fn drain_progress(
    _session: Session,
    State(state): State<Arc<AppState>>,
) -> Json<Option<crate::maintenance::MaintenanceProgress>> {
    Json(state.drain.get())
}

/// Both maintenance routes carry a cluster and a node in the path for the
/// same reason every other cluster route does — so the URL says what it is
/// about. Only one pairing is answerable: this node, in the cluster it is
/// actually in.
fn guard_cluster(state: &AppState, cluster: &str, node: &str) -> Result<(), ApiError> {
    if node != state.cluster.node() {
        return Err(ApiError::Conflict(format!(
            "Maintenance runs on the node it is about — its machines can only be moved by the \
             node running them. Open the console of \"{node}\"."
        )));
    }
    let membership = state.cluster.environment_record()?.ok_or_else(|| {
        ApiError::Conflict("This node has not joined an environment.".to_string())
    })?;
    match membership.node(node).and_then(|n| n.cluster.as_deref()) {
        Some(actual) if actual == cluster => Ok(()),
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
