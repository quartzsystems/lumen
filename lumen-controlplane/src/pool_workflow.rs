//! Creating and destroying a LumenFS pool: the transactional workflows the
//! drive wizard drives.
//!
//! The shape is the appliance's standard one. The coordinator computes
//! everything up front — node ids, the pool identity, every brick's
//! identity, which end of the peer link each member holds — and then walks
//! the members **sequentially**, because this reformats disks and a failure
//! part-way should stop, unwind what was prepared, and name where it
//! stopped. Consent is taken once, here; the peer routes deliberately
//! accept no acknowledgement from the wire.
//!
//! Two facts shape the ending. `PoolPresence` is resolved once at control
//! plane startup (its own comment prices this): a created or destroyed
//! pool is adopted by **restarting the control planes**, peers first, this
//! node last — and this node's own restart comes only after the workflow
//! has already marked itself complete, so a console poll that lands before
//! the restart sees a finished job, and one that lands after reads the
//! observed pool itself. No control plane restarts before every member has
//! prepared, so a failed create never leaves a half-restarted fleet.
//!
//! Progress rides this module's own handle rather than the cluster
//! create's — same steps shape, so the console renders both with one
//! component — because a workflow in this crate cannot drive the cluster
//! crate's private setters, and `cluster_updates` already set the
//! precedent.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use lumen_cluster::join::{StepProgress, StepState, WorkflowPhase};
use lumen_cluster::validate::{ValidationCode, ValidationError};
use lumen_cluster::{ClusterError, EnvironmentMembership, EnvironmentNode};
use lumen_pool::{BrickFormat, PeerRole, PoolConfig};
use serde::{Deserialize, Serialize};

use crate::AppState;

/// The peer link's one port: `lumen-pool.xml` opens 7800–7809, one pool
/// per cluster, so the first.
const PEER_PORT: u16 = 7800;

/// How long a freshly started daemon gets to answer its loopback control
/// socket before the prepare is called a failure.
const DAEMON_ANSWER_DEADLINE: Duration = Duration::from_secs(15);

/// How long a restarted control plane gets to come back and answer for its
/// pool before the adopt step is called a failure.
const ADOPT_DEADLINE: Duration = Duration::from_secs(60);

const POLL: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Progress: the cluster create's shape, owned here.

/// One pool job as the console polls it.
#[derive(Debug, Clone, Serialize)]
pub struct PoolProgress {
    /// `create` or `destroy` — one job slot serves both, and the dialog
    /// reads which it is watching.
    pub action: String,
    pub phase: WorkflowPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub steps: Vec<StepProgress>,
}

/// The job slot on [`AppState`]. In-memory on purpose: the coordinator's
/// own restart is the workflow's *last* act, and the observed pool — not
/// this feed — is the truth a console falls back to after it.
#[derive(Clone, Default)]
pub struct PoolJobHandle {
    inner: Arc<Mutex<Option<PoolProgress>>>,
}

impl PoolJobHandle {
    pub fn snapshot(&self) -> Option<PoolProgress> {
        self.inner.lock().unwrap().clone()
    }

    pub fn busy(&self) -> bool {
        matches!(
            self.snapshot(),
            Some(PoolProgress {
                phase: WorkflowPhase::Running,
                ..
            })
        )
    }

    fn begin(&self, progress: PoolProgress) {
        *self.inner.lock().unwrap() = Some(progress);
    }

    fn set_step(&self, step: &str, node: &str, state: StepState, detail: Option<String>) {
        let mut slot = self.inner.lock().unwrap();
        if let Some(progress) = slot.as_mut() {
            if let Some(found) = progress
                .steps
                .iter_mut()
                .find(|s| s.step == step && s.node.as_deref() == Some(node))
            {
                found.state = state;
                found.detail = detail;
            }
        }
    }

    fn append_step(&self, step: StepProgress) {
        let mut slot = self.inner.lock().unwrap();
        if let Some(progress) = slot.as_mut() {
            progress.steps.push(step);
        }
    }

    fn finish(&self, phase: WorkflowPhase, error: Option<String>) {
        let mut slot = self.inner.lock().unwrap();
        if let Some(progress) = slot.as_mut() {
            progress.phase = phase;
            progress.error = error;
        }
    }
}

fn step(name: &str, node: &str) -> StepProgress {
    StepProgress {
        step: name.to_string(),
        node: Some(node.to_string()),
        state: StepState::Pending,
        detail: None,
    }
}

// ---------------------------------------------------------------------------
// The wire shapes the peer routes carry.

/// What a member's preflight reports — read fresh, never trusted from the
/// wizard's snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolNodeFacts {
    /// A drop-in exists here — present *or* broken; either way a pool
    /// cannot be created over it.
    pub fsd_conf_present: bool,
    /// The ublk control device the exports need.
    pub ublk_available: bool,
    /// This member's disks, as its own scanner reports them right now.
    pub devices: Vec<lumen_zfs::BlockDevice>,
}

/// One brick as the coordinator planned it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrickPlan {
    /// The kernel name as the picker reported it; the member resolves it
    /// to its own stable path.
    pub disk: String,
    pub tier: u8,
    pub wal_holder: bool,
    /// Minted by the coordinator, lowercase hex — every identity is known
    /// before anything runs.
    pub brick_uuid: String,
}

/// Everything one member needs to become a pool member, computed up front.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolPrepare {
    pub node_id: u8,
    pub pool_uuid: String,
    pub bricks: Vec<BrickPlan>,
    pub peer: Option<PeerRole>,
    /// The daemon's control surface — the unit's loopback constant, and an
    /// ephemeral port in the tests that stand a real daemon in for it.
    pub control: std::net::SocketAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolPrepared {
    pub node_id: u8,
}

// ---------------------------------------------------------------------------
// The operator's request.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrickChoice {
    pub disk: String,
    pub tier: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrickSeat {
    pub node: String,
    pub bricks: Vec<BrickChoice>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LumenPoolCreate {
    pub seats: Vec<BrickSeat>,
    #[serde(default)]
    pub i_understand_this_erases_the_disks: bool,
}

// ---------------------------------------------------------------------------
// The peer seam.

/// What the workflow asks of a *remote* member's control plane. The local
/// member never travels through this — the workflow calls the local halves
/// below directly, exactly as `create_cluster_pool` does.
#[async_trait]
pub trait PoolWorkflowPeers: Send + Sync {
    async fn pool_preflight(&self, node: &EnvironmentNode) -> Result<PoolNodeFacts, ClusterError>;
    async fn pool_prepare(
        &self,
        node: &EnvironmentNode,
        prepare: &PoolPrepare,
    ) -> Result<PoolPrepared, ClusterError>;
    async fn pool_teardown(&self, node: &EnvironmentNode) -> Result<(), ClusterError>;
    /// Reply-then-restart: the far handler answers first and restarts
    /// itself after, detached.
    async fn restart_controlplane(&self, node: &EnvironmentNode) -> Result<(), ClusterError>;
    /// Whether the member's control plane answers for a pool: relays the
    /// status verb. `Ok(true)` — it has one; `Ok(false)` — it answered
    /// "no pool here"; `Err` — it could not be asked at all.
    async fn pool_present(&self, node: &EnvironmentNode) -> Result<bool, ClusterError>;
}

/// A control plane with no channel refuses by name, exactly as
/// [`crate::inventory::NoPeers`] does for everything else it is asked.
#[async_trait]
impl PoolWorkflowPeers for crate::inventory::NoPeers {
    async fn pool_preflight(&self, node: &EnvironmentNode) -> Result<PoolNodeFacts, ClusterError> {
        Err(no_channel(node))
    }
    async fn pool_prepare(
        &self,
        node: &EnvironmentNode,
        _prepare: &PoolPrepare,
    ) -> Result<PoolPrepared, ClusterError> {
        Err(no_channel(node))
    }
    async fn pool_teardown(&self, node: &EnvironmentNode) -> Result<(), ClusterError> {
        Err(no_channel(node))
    }
    async fn restart_controlplane(&self, node: &EnvironmentNode) -> Result<(), ClusterError> {
        Err(no_channel(node))
    }
    async fn pool_present(&self, node: &EnvironmentNode) -> Result<bool, ClusterError> {
        Err(no_channel(node))
    }
}

fn no_channel(node: &EnvironmentNode) -> ClusterError {
    ClusterError::Conflict(format!(
        "This control plane has no peer channel, so \"{}\" could not be asked.",
        node.name
    ))
}

// ---------------------------------------------------------------------------
// The local halves — used by the workflow for its own seat, and by the
// peer handlers serving a coordinator elsewhere. One definition each.

pub async fn local_preflight(state: &AppState) -> Result<PoolNodeFacts, ClusterError> {
    let fsd_conf_present = match PoolConfig::load() {
        Ok(Some(_)) => true,
        Ok(None) => false,
        // Present but unreadable is still present: a pool cannot be
        // created over a drop-in somebody wrote, however broken.
        Err(_) => true,
    };
    let devices = state
        .storage
        .block_devices()
        .await
        .map_err(|err| ClusterError::Conflict(err.to_string()))?
        .devices;
    Ok(PoolNodeFacts {
        fsd_conf_present,
        ublk_available: lumen_pool::ublk_available(),
        devices,
    })
}

pub async fn local_prepare(
    state: &AppState,
    prepare: &PoolPrepare,
) -> Result<PoolPrepared, ClusterError> {
    // Resolve every disk fresh, on this member's own scan — the by-id
    // path the format runs against comes from here, not from the wire.
    let devices = state
        .storage
        .block_devices()
        .await
        .map_err(|err| ClusterError::Conflict(err.to_string()))?
        .devices;
    let mut resolved = Vec::with_capacity(prepare.bricks.len());
    for plan in &prepare.bricks {
        let Some(device) = devices.iter().find(|d| d.name == plan.disk) else {
            return Err(ClusterError::Conflict(format!(
                "this node has no disk named \"{}\"",
                plan.disk
            )));
        };
        if device.claimed {
            return Err(ClusterError::Conflict(format!(
                "\"{}\" is in use here: {}",
                plan.disk,
                device.used_by.as_deref().unwrap_or("claimed")
            )));
        }
        resolved.push((plan.clone(), device.path.clone()));
    }

    // Clear each disk through this member's own guarded wipe — the layer
    // that can see a mount, swap, or an imported pool is the only one
    // allowed to say the disk is clearable.
    for (plan, _) in &resolved {
        state
            .storage
            .wipe_disk(
                &plan.disk,
                lumen_zfs::Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .map_err(|err| ClusterError::Conflict(format!("\"{}\": {err}", plan.disk)))?;
    }

    // Format non-holders first, the holder last with the others' roster —
    // a crash mid-way leaves no anchor naming bricks that do not exist.
    let roster: Vec<(String, u8)> = resolved
        .iter()
        .filter(|(plan, _)| !plan.wal_holder)
        .map(|(plan, _)| (plan.brick_uuid.clone(), plan.tier))
        .collect();
    let mut ordered: Vec<&(BrickPlan, String)> = resolved.iter().collect();
    ordered.sort_by_key(|(plan, _)| plan.wal_holder);
    for (plan, path) in ordered {
        let others = if plan.wal_holder {
            roster.as_slice()
        } else {
            &[]
        };
        state
            .pool_deploy
            .format_brick(
                &BrickFormat {
                    path: path.clone(),
                    tier: plan.tier,
                    wal_holder: plan.wal_holder,
                    brick_uuid: plan.brick_uuid.clone(),
                },
                &prepare.pool_uuid,
                others,
            )
            .await
            .map_err(ClusterError::Conflict)?;
    }

    let config = PoolConfig {
        bricks: resolved.iter().map(|(_, path)| path.into()).collect(),
        node: prepare.node_id,
        peer: prepare.peer,
        control: prepare.control,
    };
    state
        .pool_deploy
        .write_conf(&config)
        .await
        .map_err(ClusterError::Conflict)?;
    state
        .pool_deploy
        .enable_daemon()
        .await
        .map_err(ClusterError::Conflict)?;

    // The daemon must answer its own loopback before this member is
    // called prepared. Suspended is a fine answer — a two-node pool sits
    // suspended until its peer arrives; *answering* is the check.
    let control = prepare.control;
    let deadline = std::time::Instant::now() + DAEMON_ANSWER_DEADLINE;
    loop {
        let answered = tokio::task::spawn_blocking(move || {
            lumen_pool::Client::connect(control)
                .and_then(|mut client| client.status().map(|_| ()))
                .is_ok()
        })
        .await
        .unwrap_or(false);
        if answered {
            break;
        }
        if std::time::Instant::now() > deadline {
            return Err(ClusterError::Conflict(
                "the pool daemon started but never answered its control socket".into(),
            ));
        }
        tokio::time::sleep(POLL).await;
    }

    Ok(PoolPrepared {
        node_id: prepare.node_id,
    })
}

/// Remove this member's pool: stop the daemon, wipe the bricks its
/// drop-in names, remove the drop-in last — so an interrupted teardown
/// still knows its bricks on the retry. Used by the destroy workflow and
/// by the create workflow's unwind; one definition of "this node stops
/// carrying a pool".
pub async fn local_teardown(state: &AppState) -> Result<(), ClusterError> {
    let bricks = match PoolConfig::load() {
        Ok(Some(config)) => config.bricks,
        // No drop-in, or one too broken to read: nothing to wipe *by
        // name*; the stop and the removal below still run.
        Ok(None) | Err(_) => Vec::new(),
    };
    state
        .pool_deploy
        .disable_daemon()
        .await
        .map_err(ClusterError::Conflict)?;
    for brick in &bricks {
        state
            .pool_deploy
            .wipe_brick(&brick.display().to_string())
            .await
            .map_err(ClusterError::Conflict)?;
    }
    state
        .pool_deploy
        .remove_conf()
        .await
        .map_err(ClusterError::Conflict)
}

// ---------------------------------------------------------------------------
// Validation: every problem, never just the first.

pub fn validate_create(
    request: &LumenPoolCreate,
    membership: &EnvironmentMembership,
    local: &str,
    facts: &BTreeMap<String, Result<PoolNodeFacts, String>>,
    quorate: bool,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let invalid =
        |code, field: Option<&str>, message: String| ValidationError::new(code, field, message);

    if !request.i_understand_this_erases_the_disks {
        errors.push(invalid(
            ValidationCode::UnacknowledgedDestructiveOperation,
            None,
            "Building a pool reformats every disk it is given. Acknowledge that first.".into(),
        ));
    }

    let assignment = membership.node(local).and_then(|n| n.cluster.clone());
    let Some(cluster_name) = assignment else {
        errors.push(invalid(
            ValidationCode::NodeNotInEnvironment,
            None,
            "This node is not assigned to a cluster, and a pool spans its cluster.".into(),
        ));
        return errors;
    };
    let record = membership
        .clusters
        .iter()
        .find(|c| c.definition.name == cluster_name);

    // One replicated-storage engine per cluster, ever.
    if record.is_some_and(|c| !c.volumes.is_empty()) {
        errors.push(invalid(
            ValidationCode::EngineConflict,
            None,
            format!(
                "\"{cluster_name}\" has replicated DRBD volumes. A cluster runs one storage \
                 engine, and this one is already spoken for."
            ),
        ));
    }

    if !quorate {
        errors.push(invalid(
            ValidationCode::NotQuorate,
            None,
            "The cluster is not quorate. Reformatting disks from the wrong side of a \
             partition is how data is lost."
                .into(),
        ));
    }

    let members: Vec<&str> = membership
        .nodes
        .iter()
        .filter(|n| n.cluster.as_deref() == Some(cluster_name.as_str()))
        .map(|n| n.name.as_str())
        .collect();
    for member in &members {
        if !request.seats.iter().any(|s| s.node == *member) {
            errors.push(invalid(
                ValidationCode::PoolMemberMissing,
                None,
                format!(
                    "\"{member}\" has no seat. Every member of the cluster holds a replica, \
                     so every member brings bricks."
                ),
            ));
        }
    }

    for seat in &request.seats {
        if !members.contains(&seat.node.as_str()) {
            errors.push(invalid(
                ValidationCode::NodeNotInEnvironment,
                None,
                format!("\"{}\" is not a member of \"{cluster_name}\".", seat.node),
            ));
            continue;
        }
        if seat.bricks.is_empty() {
            errors.push(invalid(
                ValidationCode::NoBricksForMember,
                None,
                format!("\"{}\" was given no disks.", seat.node),
            ));
            continue;
        }
        if !seat.bricks.iter().any(|b| b.tier == 0) {
            errors.push(invalid(
                ValidationCode::NoTierZeroBrick,
                None,
                format!(
                    "\"{}\" has no tier-0 brick, and the WAL and map working set live on \
                     tier 0.",
                    seat.node
                ),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for brick in &seat.bricks {
            if !seen.insert(brick.disk.as_str()) {
                errors.push(invalid(
                    ValidationCode::DuplicateDisk,
                    None,
                    format!("\"{}\" lists \"{}\" twice.", seat.node, brick.disk),
                ));
            }
        }

        match facts.get(&seat.node) {
            None | Some(Err(_)) => {
                let why = facts
                    .get(&seat.node)
                    .and_then(|f| f.as_ref().err().cloned())
                    .unwrap_or_else(|| "it was never asked".into());
                errors.push(invalid(
                    ValidationCode::PoolMemberUnreachable,
                    None,
                    format!(
                        "\"{}\" could not be asked about its disks: {why}",
                        seat.node
                    ),
                ));
            }
            Some(Ok(node_facts)) => {
                if node_facts.fsd_conf_present {
                    errors.push(invalid(
                        ValidationCode::PoolAlreadyPresent,
                        None,
                        format!(
                            "\"{}\" already carries a pool daemon drop-in. Destroy the \
                             existing pool first.",
                            seat.node
                        ),
                    ));
                }
                if !node_facts.ublk_available {
                    errors.push(invalid(
                        ValidationCode::UblkUnavailable,
                        None,
                        format!(
                            "\"{}\" has no /dev/ublk-control — its kernel cannot serve \
                             guest disks from a pool.",
                            seat.node
                        ),
                    ));
                }
                for brick in &seat.bricks {
                    match node_facts.devices.iter().find(|d| d.name == brick.disk) {
                        None => errors.push(invalid(
                            ValidationCode::UnknownDisk,
                            None,
                            format!("\"{}\" has no disk named \"{}\".", seat.node, brick.disk),
                        )),
                        // `claimed` — something live has it — is refused;
                        // a partition table alone is not, because the
                        // prepare wipes through the member's own guards.
                        Some(device) if device.claimed => errors.push(invalid(
                            ValidationCode::DiskInUse,
                            None,
                            format!(
                                "\"{}\" on \"{}\" is in use: {}",
                                brick.disk,
                                seat.node,
                                device.used_by.as_deref().unwrap_or("claimed")
                            ),
                        )),
                        Some(_) => {}
                    }
                }
            }
        }

        // Replication rides the Core network and nothing else.
        let on_core =
            record.is_some_and(|c| c.networks.core.members.iter().any(|m| m.node == seat.node));
        if !on_core {
            errors.push(invalid(
                ValidationCode::NoCoreSeat,
                None,
                format!(
                    "\"{}\" has no seat on the cluster's Core network, and replication \
                     rides nothing else.",
                    seat.node
                ),
            ));
        }
    }

    // The engine replicates between exactly two members today; three is
    // phase 5's reassignment.
    if members.len() != 2 {
        errors.push(invalid(
            if members.len() < 2 {
                ValidationCode::TooFewNodes
            } else {
                ValidationCode::TooManyNodes
            },
            None,
            format!(
                "A pool replicates between exactly two members today; \"{cluster_name}\" \
                 has {}.",
                members.len()
            ),
        ));
    }

    errors
}

// ---------------------------------------------------------------------------
// The plan, computed before anything runs.

struct MemberPlan {
    name: String,
    prepare: PoolPrepare,
}

fn plan_members(
    request: &LumenPoolCreate,
    membership: &EnvironmentMembership,
    cluster_name: &str,
) -> Result<Vec<MemberPlan>, ClusterError> {
    let record = membership
        .clusters
        .iter()
        .find(|c| c.definition.name == cluster_name)
        .ok_or_else(|| {
            ClusterError::Conflict(format!("\"{cluster_name}\" has no cluster record."))
        })?;
    // Sorted by name: the same deterministic ids every node would derive.
    let mut names: Vec<String> = request.seats.iter().map(|s| s.node.clone()).collect();
    names.sort();
    let pool_uuid = lumen_pool::mint_pool_uuid();
    let listener_core = record
        .networks
        .core
        .members
        .iter()
        .find(|m| m.node == names[0])
        .ok_or_else(|| ClusterError::Conflict(format!("\"{}\" has no Core seat.", names[0])))?
        .address;

    let mut plans = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let seat = request
            .seats
            .iter()
            .find(|s| s.node == *name)
            .expect("names came from the seats");
        // The first tier-0 brick holds the WAL; brick order inside a seat
        // is (tier, disk) so the choice is deterministic too.
        let mut bricks: Vec<BrickChoice> = seat.bricks.clone();
        bricks.sort_by(|a, b| (a.tier, &a.disk).cmp(&(b.tier, &b.disk)));
        let mut planned = Vec::with_capacity(bricks.len());
        let mut holder_named = false;
        for brick in &bricks {
            let wal_holder = !holder_named && brick.tier == 0;
            holder_named |= wal_holder;
            planned.push(BrickPlan {
                disk: brick.disk.clone(),
                tier: brick.tier,
                wal_holder,
                brick_uuid: lumen_pool::mint_brick_uuid(&format!("{name}/{}", brick.disk)),
            });
        }
        let peer = if index == 0 {
            PeerRole::Listen(std::net::SocketAddr::from((listener_core, PEER_PORT)))
        } else {
            PeerRole::Dial(std::net::SocketAddr::from((listener_core, PEER_PORT)))
        };
        plans.push(MemberPlan {
            name: name.clone(),
            prepare: PoolPrepare {
                node_id: index as u8,
                pool_uuid: pool_uuid.clone(),
                bricks: planned,
                peer: Some(peer),
                control: lumen_pool::DEFAULT_CONTROL
                    .parse()
                    .expect("a valid constant"),
            },
        });
    }
    Ok(plans)
}

// ---------------------------------------------------------------------------
// Create.

pub async fn create_pool(
    state: &Arc<AppState>,
    request: LumenPoolCreate,
) -> Result<PoolProgress, ClusterError> {
    if state.pool_job.busy() {
        return Err(ClusterError::Conflict(
            "A pool job is already running. Wait for it to finish.".into(),
        ));
    }
    let membership = state
        .cluster
        .environment_record()?
        .ok_or_else(|| ClusterError::Conflict("This node has not joined an environment.".into()))?;
    let local = state.cluster.node().to_string();
    let cluster_name = membership
        .node(&local)
        .and_then(|n| n.cluster.clone())
        .unwrap_or_default();

    // Quorum, from the cluster's own view; a view that cannot be read is
    // not quorate for this purpose.
    let quorate = if cluster_name.is_empty() {
        false
    } else {
        match state.cluster.cluster(&cluster_name).await {
            Ok(view) => view.error.is_none() && view.quorum.quorate,
            Err(_) => false,
        }
    };

    // Preflight every seat — the local one directly, the rest over the
    // peer channel — so validation judges fresh facts.
    let mut facts: BTreeMap<String, Result<PoolNodeFacts, String>> = BTreeMap::new();
    for seat in &request.seats {
        let outcome = if seat.node == local {
            local_preflight(state).await.map_err(|err| err.to_string())
        } else {
            match membership.node(&seat.node) {
                Some(node) => state
                    .pool_peers
                    .pool_preflight(node)
                    .await
                    .map_err(|err| err.to_string()),
                None => Err("not a node in this environment".into()),
            }
        };
        facts.insert(seat.node.clone(), outcome);
    }

    let errors = validate_create(&request, &membership, &local, &facts, quorate);
    if !errors.is_empty() {
        return Err(ClusterError::Invalid(errors));
    }

    let plans = plan_members(&request, &membership, &cluster_name)?;
    let mut steps = Vec::new();
    for plan in &plans {
        steps.push(step("prepare", &plan.name));
    }
    for plan in &plans {
        steps.push(step("restart", &plan.name));
    }
    state.pool_job.begin(PoolProgress {
        action: "create".into(),
        phase: WorkflowPhase::Running,
        error: None,
        steps,
    });

    let task_state = state.clone();
    tokio::spawn(async move {
        run_create(&task_state, &membership, &local, plans).await;
    });
    Ok(state
        .pool_job
        .snapshot()
        .expect("begun above, before the spawn"))
}

async fn run_create(
    state: &Arc<AppState>,
    membership: &EnvironmentMembership,
    local: &str,
    plans: Vec<MemberPlan>,
) {
    let job = state.pool_job.clone();
    let mut prepared: Vec<&MemberPlan> = Vec::new();

    for plan in &plans {
        job.set_step("prepare", &plan.name, StepState::Running, None);
        let outcome = if plan.name == local {
            local_prepare(state, &plan.prepare).await.map(|_| ())
        } else {
            match membership.node(&plan.name) {
                Some(node) => state
                    .pool_peers
                    .pool_prepare(node, &plan.prepare)
                    .await
                    .map(|_| ()),
                None => Err(ClusterError::NotFound(format!(
                    "\"{}\" left the environment mid-create.",
                    plan.name
                ))),
            }
        };
        match outcome {
            Ok(()) => job.set_step("prepare", &plan.name, StepState::Done, None),
            Err(err) => {
                job.set_step(
                    "prepare",
                    &plan.name,
                    StepState::Failed,
                    Some(err.to_string()),
                );
                // Unwind what was prepared, newest first. Best effort per
                // member, each its own visible step — a disk that was
                // reformatted is returned or named, never silent.
                for undo in prepared.iter().rev() {
                    job.append_step(StepProgress {
                        step: "unwind".into(),
                        node: Some(undo.name.clone()),
                        state: StepState::Running,
                        detail: None,
                    });
                    let undone = if undo.name == local {
                        local_teardown(state).await
                    } else {
                        match membership.node(&undo.name) {
                            Some(node) => state.pool_peers.pool_teardown(node).await,
                            None => Ok(()),
                        }
                    };
                    job.set_step(
                        "unwind",
                        &undo.name,
                        match undone {
                            Ok(()) => StepState::Unwound,
                            Err(_) => StepState::Failed,
                        },
                        undone.err().map(|e| e.to_string()),
                    );
                }
                job.finish(
                    WorkflowPhase::Failed,
                    Some(format!(
                        "\"{}\" could not be prepared: {err}. Everything prepared before it \
                         was unwound.",
                        plan.name
                    )),
                );
                return;
            }
        }
        prepared.push(plan);
    }

    // Every member is prepared and its daemon answers. Adopt: restart the
    // peers first, each verified to come back answering *for its pool*;
    // this node last, after the job is already marked complete — the feed
    // dies with this process, and the observed pool takes over as truth.
    for plan in &plans {
        if plan.name == local {
            continue;
        }
        job.set_step("restart", &plan.name, StepState::Running, None);
        let Some(node) = membership.node(&plan.name) else {
            job.set_step(
                "restart",
                &plan.name,
                StepState::Failed,
                Some("left the environment mid-create".into()),
            );
            job.finish(
                WorkflowPhase::Failed,
                Some(format!(
                    "\"{}\" left the environment mid-create.",
                    plan.name
                )),
            );
            return;
        };
        let outcome = adopt_member(state, node, true).await;
        match outcome {
            Ok(()) => job.set_step("restart", &plan.name, StepState::Done, None),
            Err(err) => {
                // The pool is real and running on every member; wiping it
                // over a restart-verification timeout would be the wrong
                // side of caution. Named failure, no unwind.
                job.set_step("restart", &plan.name, StepState::Failed, Some(err.clone()));
                job.finish(
                    WorkflowPhase::Failed,
                    Some(format!(
                        "\"{}\" was prepared but its control plane did not come back \
                         answering for the pool: {err}. The pool itself is intact; retry \
                         the restart.",
                        plan.name
                    )),
                );
                return;
            }
        }
    }

    job.set_step("restart", local, StepState::Done, None);
    job.finish(WorkflowPhase::Complete, None);
    if let Err(err) = state.pool_deploy.restart_controlplane().await {
        tracing::warn!("the coordinator could not restart itself to adopt the pool: {err}");
    }
}

/// Restart one remote member's control plane and wait for it to come back
/// answering for its pool (`expect_pool`) — or answering that it has none
/// (destroy's half).
async fn adopt_member(
    state: &Arc<AppState>,
    node: &EnvironmentNode,
    expect_pool: bool,
) -> Result<(), String> {
    state
        .pool_peers
        .restart_controlplane(node)
        .await
        .map_err(|err| err.to_string())?;
    let deadline = std::time::Instant::now() + ADOPT_DEADLINE;
    loop {
        tokio::time::sleep(POLL).await;
        match state.pool_peers.pool_present(node).await {
            Ok(present) if present == expect_pool => return Ok(()),
            _ if std::time::Instant::now() > deadline => {
                return Err(format!(
                    "did not come back {} within {} seconds",
                    if expect_pool {
                        "answering for its pool"
                    } else {
                        "without a pool"
                    },
                    ADOPT_DEADLINE.as_secs()
                ));
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Destroy.

pub async fn destroy_pool(
    state: &Arc<AppState>,
    acknowledged: bool,
) -> Result<PoolProgress, ClusterError> {
    if state.pool_job.busy() {
        return Err(ClusterError::Conflict(
            "A pool job is already running. Wait for it to finish.".into(),
        ));
    }
    if !acknowledged {
        return Err(ClusterError::invalid(ValidationError::new(
            ValidationCode::UnacknowledgedDestructiveOperation,
            None,
            "Destroying the pool wipes every brick. Acknowledge that first.",
        )));
    }

    match &state.pool {
        crate::pool::PoolPresence::Absent => {
            return Err(ClusterError::NotFound(
                "This node carries no pool to destroy.".into(),
            ))
        }
        crate::pool::PoolPresence::Present { service, .. } => {
            // The observed view is the guard: a silent member could be
            // hiding a vdisk a machine still uses — integrity over
            // availability, so missing evidence refuses too.
            let observed = service.state().await;
            if observed.health == lumen_pool::PoolHealth::Unknown {
                return Err(ClusterError::Conflict(
                    "A member cannot be reached, so what the pool holds cannot be known. \
                     A destroy with missing evidence is refused."
                        .into(),
                ));
            }
            if !observed.vdisks.is_empty() {
                return Err(ClusterError::Conflict(format!(
                    "The pool still holds {} disk(s). Delete the machines that use them \
                     first — a destroy is not a shortcut past them.",
                    observed.vdisks.len()
                )));
            }
        }
        // Broken is destroyable with the acknowledgement: there is no
        // daemon to ask, and leaving the operator no way out of Broken
        // would be worse.
        crate::pool::PoolPresence::Broken(_) => {}
    }

    let membership = state.cluster.environment_record()?;
    let local = state.cluster.node().to_string();
    // Every member of this node's cluster, local last — the coordinator
    // stays capable until nothing is left to coordinate.
    let mut members: Vec<String> = membership
        .as_ref()
        .map(|m| {
            let assignment = m.node(&local).and_then(|n| n.cluster.clone());
            m.nodes
                .iter()
                .filter(|n| n.cluster == assignment && assignment.is_some())
                .map(|n| n.name.clone())
                .collect()
        })
        .unwrap_or_default();
    if members.is_empty() {
        members.push(local.clone());
    }
    members.sort_by_key(|name| *name == local);

    let mut steps = Vec::new();
    for member in &members {
        steps.push(step("teardown", member));
    }
    for member in &members {
        steps.push(step("restart", member));
    }
    state.pool_job.begin(PoolProgress {
        action: "destroy".into(),
        phase: WorkflowPhase::Running,
        error: None,
        steps,
    });

    let task_state = state.clone();
    tokio::spawn(async move {
        run_destroy(&task_state, membership, &local, members).await;
    });
    Ok(state
        .pool_job
        .snapshot()
        .expect("begun above, before the spawn"))
}

async fn run_destroy(
    state: &Arc<AppState>,
    membership: Option<EnvironmentMembership>,
    local: &str,
    members: Vec<String>,
) {
    let job = state.pool_job.clone();
    for member in &members {
        job.set_step("teardown", member, StepState::Running, None);
        let outcome = if member == local {
            local_teardown(state).await
        } else {
            match membership.as_ref().and_then(|m| m.node(member)) {
                Some(node) => state.pool_peers.pool_teardown(node).await,
                None => Err(ClusterError::NotFound(format!(
                    "\"{member}\" is not in the environment record."
                ))),
            }
        };
        match outcome {
            Ok(()) => job.set_step("teardown", member, StepState::Done, None),
            Err(err) => {
                // A partial destroy is named, never papered over: what was
                // already torn down stays down, and the retry resumes.
                job.set_step("teardown", member, StepState::Failed, Some(err.to_string()));
                job.finish(
                    WorkflowPhase::Failed,
                    Some(format!(
                        "\"{member}\" could not be torn down: {err}. Members before it are \
                         already clear; retry to finish."
                    )),
                );
                return;
            }
        }
    }

    for member in &members {
        if member == local {
            continue;
        }
        job.set_step("restart", member, StepState::Running, None);
        let Some(node) = membership.as_ref().and_then(|m| m.node(member)) else {
            job.set_step("restart", member, StepState::Done, None);
            continue;
        };
        match adopt_member(state, node, false).await {
            Ok(()) => job.set_step("restart", member, StepState::Done, None),
            Err(err) => {
                job.set_step("restart", member, StepState::Failed, Some(err.clone()));
                job.finish(
                    WorkflowPhase::Failed,
                    Some(format!(
                        "\"{member}\"'s control plane did not come back after the destroy: \
                         {err}. Its disks are already clear; restart it by hand."
                    )),
                );
                return;
            }
        }
    }

    job.set_step("restart", local, StepState::Done, None);
    job.finish(WorkflowPhase::Complete, None);
    if let Err(err) = state.pool_deploy.restart_controlplane().await {
        tracing::warn!("the coordinator could not restart itself after the destroy: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-node environment, deserialized from the same JSON shape the
    /// replicated record actually carries — so the fixture cannot drift
    /// from the record types without failing loudly here.
    fn membership(volumes: bool, core_members: &[&str]) -> EnvironmentMembership {
        let nodes: Vec<serde_json::Value> = ["lumen01", "lumen02"]
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "address": format!("{name}:8443"),
                    "controlplane_version": "1",
                    "cluster": "alpha",
                })
            })
            .collect();
        let core: Vec<serde_json::Value> = core_members
            .iter()
            .enumerate()
            .map(|(i, name)| {
                serde_json::json!({
                    "node": name,
                    "interface": "bond0",
                    "address": format!("10.10.0.{}", i + 1),
                })
            })
            .collect();
        let definition_nodes: Vec<serde_json::Value> = ["lumen01", "lumen02"]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                serde_json::json!({
                    "name": name,
                    "ring0": format!("10.10.0.{}", i + 1),
                    "ring1": format!("10.20.0.{}", i + 1),
                    "bmc": { "address": format!("10.30.0.{}", i + 1), "username": "admin" },
                })
            })
            .collect();
        let volume_records: Vec<serde_json::Value> = if volumes {
            vec![serde_json::json!({
                "name": "alpha-v0", "size_bytes": 1, "zvol_bytes": 1,
                "port": 7788, "minor": 1, "replicas": [],
            })]
        } else {
            Vec::new()
        };
        serde_json::from_value(serde_json::json!({
            "id": "env",
            "version": 1,
            "nodes": nodes,
            "clusters": [{
                "definition": {
                    "name": "alpha",
                    "nodes": definition_nodes,
                },
                "networks": {
                    "core": {
                        "subnet": "10.10.0.0/24",
                        "mtu": 9000,
                        "members": core,
                    },
                    "management": { "subnet": "10.20.0.0/24", "members": [] },
                    "external": [],
                },
                "volumes": volume_records,
            }],
        }))
        .expect("the fixture matches the record's own shape")
    }

    fn facts_for(
        names: &[&str],
        devices: Vec<lumen_zfs::BlockDevice>,
    ) -> BTreeMap<String, Result<PoolNodeFacts, String>> {
        names
            .iter()
            .map(|name| {
                (
                    (*name).to_string(),
                    Ok(PoolNodeFacts {
                        fsd_conf_present: false,
                        ublk_available: true,
                        devices: devices.clone(),
                    }),
                )
            })
            .collect()
    }

    fn free_disk(name: &str) -> lumen_zfs::BlockDevice {
        lumen_zfs::BlockDevice {
            name: name.into(),
            path: format!("/dev/disk/by-id/scsi-{name}"),
            kernel_path: format!("/dev/{name}"),
            size: 1 << 40,
            ..Default::default()
        }
    }

    fn request(seats: &[(&str, &[(&str, u8)])], acknowledged: bool) -> LumenPoolCreate {
        LumenPoolCreate {
            seats: seats
                .iter()
                .map(|(node, bricks)| BrickSeat {
                    node: (*node).into(),
                    bricks: bricks
                        .iter()
                        .map(|(disk, tier)| BrickChoice {
                            disk: (*disk).into(),
                            tier: *tier,
                        })
                        .collect(),
                })
                .collect(),
            i_understand_this_erases_the_disks: acknowledged,
        }
    }

    fn codes(errors: &[ValidationError]) -> Vec<&'static str> {
        errors.iter().map(|e| e.code.as_str()).collect()
    }

    #[test]
    fn a_clean_request_validates_clean() {
        let membership = membership(false, &["lumen01", "lumen02"]);
        let request = request(
            &[("lumen01", &[("sdb", 0)]), ("lumen02", &[("sdb", 0)])],
            true,
        );
        let facts = facts_for(&["lumen01", "lumen02"], vec![free_disk("sdb")]);
        let errors = validate_create(&request, &membership, "lumen01", &facts, true);
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// Every refusal, each by its code — the wizard matches on these.
    #[test]
    fn every_problem_is_reported_with_its_code() {
        let membership = membership(true, &["lumen01"]);
        let mut busy = free_disk("sdc");
        busy.claimed = true;
        busy.used_by = Some("mounted at /".into());
        let request = request(
            &[
                // Duplicate disk, an unknown disk, a busy disk, no tier 0.
                ("lumen01", &[("sdb", 1), ("sdb", 1), ("sdz", 1), ("sdc", 1)]),
                // Not a member at all.
                ("lumen09", &[("sdb", 0)]),
            ],
            false,
        );
        let mut facts = facts_for(&["lumen01"], vec![free_disk("sdb"), busy]);
        facts.insert("lumen09".into(), Err("no route".into()));
        let errors = validate_create(&request, &membership, "lumen01", &facts, false);
        let found = codes(&errors);
        for wanted in [
            "unacknowledged_destructive_operation",
            "engine_conflict",
            "not_quorate",
            "pool_member_missing",     // lumen02 has no seat
            "node_not_in_environment", // lumen09
            "no_tier_zero_brick",
            "duplicate_disk",
            "unknown_disk",
            "disk_in_use",
        ] {
            assert!(found.contains(&wanted), "missing {wanted} in {found:?}");
        }
    }

    #[test]
    fn a_seat_off_the_core_network_is_refused_by_name() {
        let membership = membership(false, &["lumen01"]); // lumen02 has no Core seat
        let request = request(
            &[("lumen01", &[("sdb", 0)]), ("lumen02", &[("sdb", 0)])],
            true,
        );
        let facts = facts_for(&["lumen01", "lumen02"], vec![free_disk("sdb")]);
        let errors = validate_create(&request, &membership, "lumen01", &facts, true);
        assert!(codes(&errors).contains(&"no_core_seat"), "{errors:?}");
    }

    #[test]
    fn a_member_already_carrying_a_drop_in_is_refused() {
        let membership = membership(false, &["lumen01", "lumen02"]);
        let request = request(
            &[("lumen01", &[("sdb", 0)]), ("lumen02", &[("sdb", 0)])],
            true,
        );
        let mut facts = facts_for(&["lumen01", "lumen02"], vec![free_disk("sdb")]);
        if let Some(Ok(f)) = facts.get_mut("lumen02") {
            f.fsd_conf_present = true;
        }
        let errors = validate_create(&request, &membership, "lumen01", &facts, true);
        assert!(
            codes(&errors).contains(&"pool_already_present"),
            "{errors:?}"
        );
    }

    /// The plan is deterministic: ids by sorted name, the listener is node
    /// 0 on the Core address, the first tier-0 brick holds the WAL, and
    /// every identity is minted before anything runs.
    #[test]
    fn the_plan_is_computed_whole_before_anything_runs() {
        let membership = membership(false, &["lumen01", "lumen02"]);
        let request = request(
            &[
                ("lumen02", &[("sdc", 1), ("sdb", 0)]),
                ("lumen01", &[("sdb", 0)]),
            ],
            true,
        );
        let plans = plan_members(&request, &membership, "alpha").unwrap();
        assert_eq!(plans[0].name, "lumen01");
        assert_eq!(plans[0].prepare.node_id, 0);
        assert_eq!(
            plans[0].prepare.peer,
            Some(PeerRole::Listen("10.10.0.1:7800".parse().unwrap()))
        );
        assert_eq!(
            plans[1].prepare.peer,
            Some(PeerRole::Dial("10.10.0.1:7800".parse().unwrap()))
        );
        assert_eq!(plans[0].prepare.pool_uuid, plans[1].prepare.pool_uuid);
        // lumen02's bricks sort (tier, disk): sdb tier 0 first, and it —
        // not the tier-1 disk — holds the WAL.
        let two = &plans[1].prepare.bricks;
        assert!(two[0].disk == "sdb" && two[0].wal_holder);
        assert!(two[1].disk == "sdc" && !two[1].wal_holder);
        let mut uuids: Vec<&str> = plans
            .iter()
            .flat_map(|p| p.prepare.bricks.iter().map(|b| b.brick_uuid.as_str()))
            .collect();
        uuids.sort_unstable();
        uuids.dedup();
        assert_eq!(uuids.len(), 3, "every brick identity is distinct");
    }
}
