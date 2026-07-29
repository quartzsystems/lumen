//! Updating every node in the environment, one member at a time.
//!
//! The node-local feature in `src/updates.rs` answers "what can *this* node
//! install, and install it". This module is the other half: the same two
//! questions asked of every member, and one operator action that walks the
//! whole environment.
//!
//! ## Two walks, one sequencer
//!
//! - **Ordinary** — the ordinary updates, installed in place on every member.
//!   Nothing drains, nothing restarts.
//! - **Rolling** — each member that needs a restart is taken out of service,
//!   emptied, brought fully up to date including its kernel, restarted, and
//!   waited for before the next one is touched.
//!
//! They are the same walk with more stages per member, which is why they are
//! one function and not two. A rolling update only drains the members that
//! actually need restarting: a node with nothing but userland updates waiting
//! is installed in place, because taking it down to install `openssl-libs`
//! would be a cost with no purpose.
//!
//! ## The one node a rolling update will not restart
//!
//! The node running the walk. A control plane cannot drive a rolling update
//! through its own reboot, and the alternatives are worse than the limit:
//! handing coordination to a peer mid-walk is a distributed hand-off with its
//! own failure modes, and rebooting anyway leaves a node out of service with
//! nothing left running to put it back.
//!
//! So a rolling update finishes with that node named, still pending, and told
//! what to do about it — which is either Maintenance by hand, or running the
//! same update again from another member's console, where every other node is
//! already current and this one is the only work left. Two passes, no new
//! machinery, and each pass is the operation that was already tested.
//!
//! ## Why it is a walk and not a fan-out
//!
//! Reading is concurrent — six members answering at once, the wall clock the
//! slowest one rather than the sum, exactly as [`crate::inventory`] does.
//!
//! Installing is **sequential**, one member at a time, and that is the whole
//! design. Three reasons, in order of how much they cost when ignored:
//!
//! 1. A transaction that fails on one node usually fails on the next for the
//!    same reason — a mirror that has gone away, an index that is signed with
//!    a key the nodes do not have, a package that will not resolve. Pressing
//!    on turns one node that did not update into six.
//! 2. Lumen's own packages restart the control plane (`%postun
//!    %systemd_postun_with_restart` in `packages/lumen-controlplane.spec`).
//!    Six consoles restarting at once is an environment with no console at
//!    all for as long as it takes them all to come back.
//! 3. It is the shape a rolling update needs anyway. Draining a node,
//!    restarting it, and waiting for it to rejoin before touching the next
//!    one is this same walk with more per-member steps.
//!
//! ## The local node goes last
//!
//! For reason 2, sharpened: the node running this walk is the one driving it.
//! If it updated itself first, its control plane would restart, and the walk
//! — which lives in that process's memory — would stop with members still
//! untouched. Every peer is finished before the coordinator touches itself.
//!
//! ## Which means the walk's own record is not the source of truth
//!
//! When the coordinator finally updates itself, the control plane restarts and
//! [`RollProgress`] goes with it. This is not worked around, because it cannot
//! honestly be: there is no database here (see the `lumen_update` crate note),
//! and a record that claimed to survive a process that did not would be worse
//! than one that plainly does not.
//!
//! What survives is better. Every member knows what is installed on it, and
//! [`environment`] asks all of them. A console that reconnects after the
//! coordinator restarted reads the truth from the nodes themselves rather than
//! from a job record — the same discipline `src/updates.rs` already applies to
//! the reboot state, and for the same reason.
//!
//! ## Waiting for a member that is restarting its own control plane
//!
//! It follows that "did that member finish?" cannot be answered by polling its
//! transaction feed alone: a member installing `lumen-controlplane` restarts
//! the daemon holding that feed, and comes back reporting no transaction at
//! all. Treating that as a failure would fail every member that updated Lumen
//! — which is to say, the whole point.
//!
//! So a member is watched two ways, and either answers. Its transaction feed
//! is read while it lasts; if the feed goes away or comes back empty, the
//! member is asked what is still waiting on it, and the package database
//! answers. See [`await_member`].

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lumen_cluster::join::{StepState, WorkflowPhase};
use lumen_update::{ApplyRequest, UpdateView};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::updates::UpdateProgress;
use crate::AppState;

/// How long one member may take before the walk gives up on it.
///
/// An hour, matching the deadline the update backend gives its own privileged
/// runner: a member downloading a kernel over a slow link is doing exactly
/// what it was asked to, and a walk that timed out sooner than the transaction
/// it started would report a failure that had not happened.
const MEMBER_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// How long a member's machines may take to move off it.
///
/// A live migration is bounded by how fast a machine's memory can be copied
/// over the Core network, and a node carrying several large ones is doing them
/// one at a time. Twenty minutes is generous for that and short enough that a
/// drain which is genuinely stuck does not hold the cluster's update open all
/// afternoon.
const DRAIN_DEADLINE: Duration = Duration::from_secs(20 * 60);

/// How long a member may take to come back from a restart.
///
/// This is a bare-metal appliance: firmware, then a pool import, then the
/// stack. Fifteen minutes covers a slow server with a lot of disks; a node
/// that has not answered by then is one an operator needs to go and look at,
/// and the update says so rather than waiting indefinitely.
const REJOIN_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// The poll interval, backing off from the first question to the steady rate.
///
/// It starts short so a member with nothing to do is not waited on for five
/// seconds to say so, and settles because a transaction that has been running
/// for two minutes will not finish in the next quarter-second.
const POLL_FIRST: Duration = Duration::from_millis(250);
const POLL_MAX: Duration = Duration::from_secs(5);

// --- the environment-wide read ----------------------------------------------

/// One member's update state, from one round trip.
///
/// Both halves together for the reason [`crate::inventory::NodeInventory`]
/// carries three: they are read on the same node at the same moment, and
/// splitting them would only let the two halves of one row disagree about
/// whether that member is installing something.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeUpdates {
    pub view: UpdateView,
    /// The transaction running — or last run — on that member. `None` when
    /// its control plane has not run one since it started, which is also what
    /// a member that just restarted into a new version reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<UpdateProgress>,
}

/// One member as the cluster table carries it: its updates, or why it has
/// none to report.
///
/// `reachable: false` with an `error` is a member that is in the environment
/// and could not be asked — a different fact from a member with nothing
/// waiting, and one the console must be able to tell apart. During a walk it
/// is also the *expected* state of the member currently restarting its
/// control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberUpdates {
    pub node: String,
    pub local: bool,
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates: Option<NodeUpdates>,
}

/// What is waiting across the environment, rolled up.
///
/// Counted per member rather than per distinct package, and deliberately: six
/// nodes each waiting on the same `openssl-libs` is six transactions and six
/// chances to fail, not one update. The number an operator is deciding about
/// is the work, not the package list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterCounts {
    pub members: usize,
    pub reachable: usize,
    /// Members with at least one ordinary update waiting.
    pub members_with_updates: usize,
    /// Members with at least one platform package waiting.
    pub members_with_platform: usize,
    /// Ordinary updates, summed across members.
    pub updates: usize,
    /// Of those, the ones an advisory names as security fixes.
    pub security: usize,
    /// Platform packages, summed across members.
    pub platform: usize,
    /// Members already waiting on a restart to finish an earlier update.
    pub restarts_required: usize,
    /// Members whose platform set is waiting but will not resolve. These are
    /// the ones a rolling update cannot take, and naming them is how the
    /// console explains why.
    pub platform_blocked: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterUpdateView {
    pub members: Vec<MemberUpdates>,
    pub counts: ClusterCounts,
}

/// Every member's updates, asked for concurrently.
///
/// A node with no environment yet still answers — with itself, alone. The
/// console renders the same table either way, which is what lets the cluster
/// panel exist on a single appliance without a second code path.
pub async fn environment(state: &Arc<AppState>) -> ClusterUpdateView {
    gather(state, false).await
}

/// The same, but every member asks its repositories first.
///
/// Concurrent for the same reason the read is: an operator pressing "check
/// every node" should wait for the slowest member, not for all of them added
/// together. A member whose check fails still answers — with its previous
/// view and the reason, which is what [`lumen_update::UpdateService::check`]
/// records.
pub async fn check(state: &Arc<AppState>) -> ClusterUpdateView {
    gather(state, true).await
}

async fn gather(state: &Arc<AppState>, refresh: bool) -> ClusterUpdateView {
    let local_name = state.cluster.node().to_string();
    let nodes = state.cluster.environment_nodes().unwrap_or_default();

    let members = if nodes.is_empty() {
        vec![local_member(state, &local_name, refresh).await]
    } else {
        let calls = nodes.iter().map(|node| {
            let state = state.clone();
            let local_name = local_name.clone();
            async move {
                if node.name == local_name {
                    return local_member(&state, &node.name, refresh).await;
                }
                let asked = if refresh {
                    state.peers.check_updates(node).await
                } else {
                    state.peers.updates(node).await
                };
                match asked {
                    Ok(updates) => MemberUpdates {
                        node: node.name.clone(),
                        local: false,
                        reachable: true,
                        error: None,
                        updates: Some(updates),
                    },
                    Err(err) => MemberUpdates {
                        node: node.name.clone(),
                        local: false,
                        reachable: false,
                        error: Some(err.to_string()),
                        updates: None,
                    },
                }
            }
        });
        futures_util::future::join_all(calls).await
    };

    let counts = roll_up(&members);
    ClusterUpdateView { members, counts }
}

/// This node's own answer, from its own service. Never a socket: only this
/// process holds the update service, and a loopback round trip would add ways
/// to fail without adding anything true.
async fn local_member(state: &Arc<AppState>, node: &str, refresh: bool) -> MemberUpdates {
    // A refresh that fails still leaves a view worth rendering — the service
    // records the reason and `view()` carries it — so the error is not
    // propagated here. `reachable` means "this node answered", and it did.
    if refresh {
        if let Err(err) = state.updates.check().await {
            tracing::warn!("this node could not reach its repositories: {err}");
        }
    }
    match state.updates.view().await {
        Ok(view) => MemberUpdates {
            node: node.to_string(),
            local: true,
            reachable: true,
            error: None,
            updates: Some(NodeUpdates {
                view,
                progress: state.update_job.get(),
            }),
        },
        Err(err) => MemberUpdates {
            node: node.to_string(),
            local: true,
            reachable: false,
            error: Some(err.to_string()),
            updates: None,
        },
    }
}

fn roll_up(members: &[MemberUpdates]) -> ClusterCounts {
    let mut counts = ClusterCounts {
        members: members.len(),
        ..ClusterCounts::default()
    };
    for member in members {
        let Some(updates) = member.updates.as_ref() else {
            continue;
        };
        counts.reachable += 1;
        let view = &updates.view;
        if !view.updates.is_empty() {
            counts.members_with_updates += 1;
        }
        if view.platform.pending() {
            counts.members_with_platform += 1;
            if !view.platform.resolves {
                counts.platform_blocked += 1;
            }
        }
        counts.updates += view.updates.len();
        counts.security += view.counts.security;
        counts.platform += view.platform.updates.len();
        if view.reboot.required {
            counts.restarts_required += 1;
        }
    }
    counts
}

// --- the walk ----------------------------------------------------------------

/// Where one member is in its leg.
///
/// An ordinary walk only ever reaches `Installing`. A rolling one goes through
/// all of them, and the stage is what turns "this is taking a while" into
/// something an operator can read: draining is the machines moving, rejoining
/// is the node booting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberStage {
    /// Not started.
    Waiting,
    /// Asking that member's repositories what it has.
    Checking,
    /// Out of service, machines moving off.
    Draining,
    /// The package manager is running.
    Installing,
    /// Told to restart.
    Restarting,
    /// Restarted; waiting for it to answer and to be online again.
    Rejoining,
    /// Putting it back into service.
    Returning,
    /// Nothing left to do for this member.
    Finished,
}

/// What one member's leg of the walk did.
///
/// Richer than the [`lumen_cluster::join::StepProgress`] a cluster create
/// publishes, because a step here is a whole transaction: what it changed and
/// whether it left a restart outstanding are both things the operator acts on
/// next, and neither fits in a detail string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberStep {
    pub node: String,
    pub state: StepState,
    #[serde(default = "waiting_stage")]
    pub stage: MemberStage,
    /// The sentence for this leg — why it was skipped, or why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What the package manager reported changing on that member.
    #[serde(default)]
    pub changed: Vec<String>,
    /// Whether that member is now running a kernel it no longer has. Always
    /// false after an ordinary update, which is the property that feature
    /// exists for — and false after a rolling one, because that is what the
    /// restart was for.
    #[serde(default)]
    pub restart_required: bool,
    /// Machines that would not move off during the drain. Non-empty is why a
    /// rolling update stopped rather than restarting the node under them.
    #[serde(default)]
    pub stranded: Vec<crate::maintenance::Stranded>,
}

fn waiting_stage() -> MemberStage {
    MemberStage::Waiting
}

/// Which of the two walks this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollKind {
    /// Ordinary updates, installed in place. Nothing drains, nothing
    /// restarts.
    Ordinary,
    /// Everything, including the kernel — each member drained, installed,
    /// restarted, and waited for before the next one is touched.
    Rolling,
}

/// The whole of a walk, as the console polls it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollProgress {
    pub kind: RollKind,
    pub phase: WorkflowPhase,
    /// Who asked, as the `user@realm` principal.
    pub by: String,
    /// Unix seconds, against the coordinating node's clock.
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    /// Why the walk stopped early. A walk that finished with every member
    /// either updated or already current has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// One per member, in the order they are visited — every peer, then this
    /// node. Named in full before anything is installed, so the console shows
    /// the whole of the work up front.
    pub steps: Vec<MemberStep>,
    /// What the walk deliberately did not do, and what the operator should do
    /// about it. Set when a rolling update finishes with the coordinating node
    /// still needing a restart — see [`begin`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_to_you: Option<String>,
}

/// Shared handle: the walk writes it, the progress route reads it.
///
/// One at a time. A second walk would drive the same members through the same
/// package managers, and the two would interleave into an order neither
/// operator asked for.
#[derive(Clone, Default)]
pub struct RollHandle {
    inner: Arc<Mutex<Option<RollProgress>>>,
}

impl RollHandle {
    pub fn get(&self) -> Option<RollProgress> {
        self.inner.lock().expect("roll progress lock").clone()
    }

    pub fn running(&self) -> bool {
        self.get()
            .is_some_and(|progress| progress.phase == WorkflowPhase::Running)
    }

    fn set(&self, progress: RollProgress) {
        *self.inner.lock().expect("roll progress lock") = Some(progress);
    }

    /// Rewrite one member's step in place, leaving the rest of the feed alone.
    fn step(&self, node: &str, apply: impl FnOnce(&mut MemberStep)) {
        let mut guard = self.inner.lock().expect("roll progress lock");
        let Some(progress) = guard.as_mut() else {
            return;
        };
        if let Some(step) = progress.steps.iter_mut().find(|step| step.node == node) {
            apply(step);
        }
    }

    fn finish(&self, phase: WorkflowPhase, error: Option<String>) {
        let mut guard = self.inner.lock().expect("roll progress lock");
        let Some(progress) = guard.as_mut() else {
            return;
        };
        progress.phase = phase;
        progress.finished_at = Some(now());
        progress.error = error;
    }

    fn leave(&self, sentence: String) {
        let mut guard = self.inner.lock().expect("roll progress lock");
        if let Some(progress) = guard.as_mut() {
            progress.left_to_you = Some(sentence);
        }
    }
}

/// What the console asked for, across the environment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollRequest {
    /// Take each member through a restart, so a new kernel takes effect.
    ///
    /// Without this, the walk installs the ordinary updates in place and
    /// nothing goes down. With it, each member is drained, brought fully up to
    /// date, restarted, and waited for before the next one is touched.
    #[serde(default)]
    pub rolling: bool,
    /// Required for a rolling update, and named as the sentence being agreed
    /// to, the same way every other acknowledgement in this appliance is.
    #[serde(default)]
    pub i_understand_each_node_restarts: bool,
    /// Install the platform set without restarting anything.
    ///
    /// Refused, and the refusal is the feature: a kernel installed on every
    /// member and running on none is a cluster that looks updated and is one
    /// power cut away from finding out it is not. The platform set moves
    /// through `rolling`, which restarts each node as it goes.
    #[serde(default)]
    pub platform: bool,
}

/// Start walking the environment, and answer as soon as it is under way.
///
/// The guards run here, synchronously, so a refusal arrives on the request the
/// operator made rather than as a job that fails a moment later — the same
/// split [`crate::updates::begin`] makes, for the same reason.
pub async fn begin(
    state: &Arc<AppState>,
    by: &str,
    request: RollRequest,
) -> Result<RollProgress, ApiError> {
    if request.platform && !request.rolling {
        return Err(ApiError::Conflict(
            "The kernel and the storage modules are not installed across a cluster this way. \
             They do not take effect until a node restarts, and a kernel installed on every \
             member and running on none is a cluster that looks updated and is not. Use a \
             rolling update, which drains each node and restarts it before it touches the next \
             one."
                .to_string(),
        ));
    }
    if request.rolling && !request.i_understand_each_node_restarts {
        return Err(ApiError::Conflict(
            "A rolling update restarts every node in this cluster, one at a time. Each one is \
             taken out of service and its machines are moved off first, and the next is not \
             touched until the previous has rejoined — but the machines that move are \
             interrupted for as long as a live migration takes, and any that cannot move stop \
             the update rather than being restarted underneath. Acknowledge \
             \"i_understand_each_node_restarts\" to go ahead."
                .to_string(),
        ));
    }
    if state.roll.running() {
        return Err(ApiError::Conflict(
            "This node is already updating the environment. Watch that one finish rather than \
             starting a second — the two would drive the same members through the same package \
             managers in an order neither of you asked for."
                .to_string(),
        ));
    }
    if state.update_job.running() {
        return Err(ApiError::Conflict(
            "This node is already installing updates on its own. Watch that one finish first — \
             an environment-wide update ends by installing on this node, and the package manager \
             takes a lock that the two would fight over."
                .to_string(),
        ));
    }

    let order = visiting_order(state, request.rolling)?;
    let progress = RollProgress {
        kind: if request.rolling {
            RollKind::Rolling
        } else {
            RollKind::Ordinary
        },
        phase: WorkflowPhase::Running,
        by: by.to_string(),
        started_at: now(),
        finished_at: None,
        error: None,
        left_to_you: None,
        steps: order
            .iter()
            .map(|node| MemberStep {
                node: node.clone(),
                state: StepState::Pending,
                stage: MemberStage::Waiting,
                detail: None,
                changed: Vec::new(),
                restart_required: false,
                stranded: Vec::new(),
            })
            .collect(),
    };
    state.roll.set(progress.clone());

    let background = state.clone();
    let owner = by.to_string();
    tokio::spawn(async move {
        walk(&background, order, &owner, request.rolling).await;
    });
    Ok(progress)
}

/// Who is visited, and in what order: every peer by name, then this node.
///
/// By name rather than by however the membership record happens to be ordered,
/// so two operators watching the same environment are told the same story, and
/// a walk retried after a failure visits the members in the order it did
/// before.
///
/// This node last — see the module note. It is the one member whose update
/// can stop the walk by restarting the process running it.
///
/// A rolling update is scoped to **this node's cluster** rather than to the
/// environment, and has to be: draining a node means moving its machines to
/// another member that holds current replicas of their disks, and that is a
/// question only a cluster answers. An environment-wide roll would reach nodes
/// with nowhere to move work to and no quorum to reason about.
fn visiting_order(state: &Arc<AppState>, rolling: bool) -> Result<Vec<String>, ApiError> {
    let local = state.cluster.node().to_string();

    let mut peers: Vec<String> = if rolling {
        let membership = state.cluster.environment_record()?.ok_or_else(|| {
            ApiError::Conflict(
                "A rolling update restarts nodes one at a time and moves their machines to the \
                 members that can take them, and this node has not joined an environment. Update \
                 it from its own page instead."
                    .to_string(),
            )
        })?;
        let cluster = membership
            .node(&local)
            .and_then(|node| node.cluster.clone())
            .ok_or_else(|| {
                ApiError::Conflict(
                    "A rolling update needs a cluster: taking a node down safely means moving its \
                     machines onto a member holding current replicas of their disks, and this \
                     node is not in one. Install the updates in place instead, and restart this \
                     node from its own Maintenance page."
                        .to_string(),
                )
            })?;
        membership
            .nodes
            .iter()
            .filter(|node| node.cluster.as_deref() == Some(cluster.as_str()))
            .map(|node| node.name.clone())
            .filter(|name| name != &local)
            .collect()
    } else {
        state
            .cluster
            .environment_nodes()
            .unwrap_or_default()
            .iter()
            .map(|node| node.name.clone())
            .filter(|name| name != &local)
            .collect()
    };

    peers.sort();
    peers.push(local);
    Ok(peers)
}

/// The walk proper.
///
/// Stops at the first member that fails. What already worked stays — those
/// nodes are updated, which is useful on its own — and the error names the
/// member so a retry can finish the job, skipping the ones that are already
/// current.
async fn walk(state: &Arc<AppState>, order: Vec<String>, by: &str, rolling: bool) {
    let local = state.cluster.node().to_string();

    for node in &order {
        let is_local = node == &local;
        state.roll.step(node, |step| {
            step.state = StepState::Running;
            step.stage = MemberStage::Checking;
        });

        match visit(state, node, is_local, by, rolling).await {
            Ok(Visit::AlreadyCurrent) => {
                state.roll.step(node, |step| {
                    step.state = StepState::Done;
                    step.stage = MemberStage::Finished;
                    step.detail = Some("Already up to date — nothing was installed.".to_string());
                });
            }
            // The one member a rolling update will not restart: the one
            // running the rolling update. Left plainly pending rather than
            // dressed up as done — it genuinely is not.
            Ok(Visit::LeftToTheOperator(sentence)) => {
                state.roll.step(node, |step| {
                    step.state = StepState::Pending;
                    step.stage = MemberStage::Waiting;
                    step.detail = Some(sentence.clone());
                });
                state.roll.leave(sentence);
            }
            Ok(Visit::Installed {
                changed,
                restart_required,
            }) => {
                tracing::info!(%node, changed = changed.len(), "member updated");
                state.roll.step(node, |step| {
                    step.state = StepState::Done;
                    step.stage = MemberStage::Finished;
                    step.changed = changed;
                    step.restart_required = restart_required;
                });
            }
            Err(reason) => {
                tracing::warn!(%node, "the walk stopped: {reason}");
                // A member that got as far as being drained is put back into
                // service before the walk gives up on it. Leaving a node out
                // of service because an update failed would turn one failure
                // into a cluster running with a member deliberately idle, and
                // an operator who never asked for that would have to find it.
                if rolling && !is_local {
                    restore(state, node, by).await;
                }
                state.roll.step(node, |step| {
                    step.state = StepState::Failed;
                    step.detail = Some(reason.clone());
                });
                let done: Vec<&String> = order.iter().take_while(|name| *name != node).collect();
                state.roll.finish(
                    WorkflowPhase::Failed,
                    Some(format!(
                        "\"{node}\" could not be updated: {reason} {} Nothing after it was \
                         attempted — an update that fails on one member usually fails on the next \
                         for the same reason.",
                        if done.is_empty() {
                            "No member was updated.".to_string()
                        } else {
                            format!(
                                "{} finished first and {} updated.",
                                done.iter()
                                    .map(|name| name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                if done.len() == 1 { "is" } else { "are" }
                            )
                        }
                    )),
                );
                return;
            }
        }
    }

    state.roll.finish(WorkflowPhase::Complete, None);
}

/// What one member's leg came to.
enum Visit {
    /// Nothing was waiting, so nothing was run.
    AlreadyCurrent,
    Installed {
        changed: Vec<String>,
        restart_required: bool,
    },
    /// Deliberately not done, and the sentence saying why not.
    LeftToTheOperator(String),
}

/// One member: ask it what is waiting, install it, and wait for it to finish.
///
/// The check is not an optimization. The walk has to act on what is true on
/// that member now, not on whatever the console was last shown, and asking
/// first is also what lets "nothing to do" be a plain answer rather than an
/// error message parsed back out of a refusal.
///
/// ## What a member needs decides what it gets
///
/// A rolling update does not drain and restart every member on principle — it
/// does it to the members that need it. Three cases, decided from that fresh
/// check:
///
/// - **Needs a restart** — the platform set is waiting, or an earlier install
///   already left a kernel installed and not running. The full cycle: drain,
///   install everything, restart, wait for it back, return it to service.
/// - **Only ordinary updates** — installed in place. Nothing drains and
///   nothing restarts, because nothing here requires it, and taking a node
///   down to install `openssl-libs` would be a cost with no purpose.
/// - **Nothing** — stepped over.
///
/// That means a rolling update over a cluster where one node has a kernel
/// waiting restarts exactly that node.
async fn visit(
    state: &Arc<AppState>,
    node: &str,
    is_local: bool,
    by: &str,
    rolling: bool,
) -> Result<Visit, String> {
    let fresh = ask(state, node, is_local, true).await?;
    let platform = &fresh.view.platform;

    // A restart is owed either because a new kernel is waiting, or because one
    // was installed earlier and the node is still running the old one. The
    // second is worth catching: it is what a cluster looks like the morning
    // after somebody installed the platform set on every node by hand.
    let needs_restart = rolling && (platform.pending() || fresh.view.reboot.required);

    if !needs_restart {
        if fresh.view.updates.is_empty() {
            return Ok(Visit::AlreadyCurrent);
        }
        return install(state, node, is_local, by, false).await;
    }

    // The gate, checked on the member rather than assumed from the
    // coordinator's copy: a kernel whose storage modules have not caught up is
    // refused outright, and the refusal carries the solver's own words. This
    // is the failure the whole update domain exists to prevent, and a rolling
    // update is the one place it would be automated into every node in turn.
    if platform.pending() && !platform.resolves {
        return Err(format!(
            "its kernel and storage modules cannot be installed together yet, so nothing was \
             installed on it and nothing after it was attempted. This is ordinary for a few days \
             after a point release — installing the kernel without the modules built against it \
             is what leaves a node unable to import its pool. Its package manager said: {}",
            platform.detail.as_deref().unwrap_or("no reason was given")
        ));
    }

    // The coordinator cannot restart itself and go on driving. Nothing is
    // attempted on it rather than half-attempted; the sentence says what to do
    // instead. See the module note.
    if is_local {
        return Ok(Visit::LeftToTheOperator(format!(
            "This node is the one running the update, so it was not restarted — a node cannot \
             drive a rolling update through its own reboot. It still needs one{}. Finish it from \
             another member's console, where running this same update will find only this node \
             left to do, or take it through Maintenance by hand.",
            if platform.pending() {
                format!(
                    ", and has {} kernel and storage package{} waiting",
                    platform.updates.len(),
                    if platform.updates.len() == 1 { "" } else { "s" }
                )
            } else {
                String::new()
            }
        )));
    }

    rolling_visit(state, node, by, &fresh).await
}

/// The full cycle for one member: out of service, up to date, restarted, back.
///
/// Ordered so that nothing irreversible happens before the reversible parts
/// have succeeded. The node is emptied before anything is installed, so a
/// drain that strands a machine costs nothing but a return to service; the
/// ordinary updates go in before the kernel, so a mirror that fails has not
/// left a half-moved platform set; and the restart is last, because it is the
/// only step that cannot be taken back.
async fn rolling_visit(
    state: &Arc<AppState>,
    node: &str,
    by: &str,
    fresh: &NodeUpdates,
) -> Result<Visit, String> {
    let member = peer(state, node)?;

    // 1. Out of service, and empty. A machine that would not move stops the
    //    update: restarting a node underneath a running machine is exactly
    //    the outcome the drain exists to prevent, and pressing on would do it
    //    on every node in turn.
    state
        .roll
        .step(node, |step| step.stage = MemberStage::Draining);
    let stranded = drain_member(state, &member, node, by).await?;
    if !stranded.is_empty() {
        let names: Vec<&str> = stranded.iter().map(|s| s.name.as_str()).collect();
        state.roll.step(node, |step| {
            step.stranded = stranded.clone();
        });
        return Err(format!(
            "it could not be emptied — {} would not move off it ({}). It has been put back into \
             service and nothing was installed. Restarting a node with machines still running on \
             it is what draining is for.",
            if names.len() == 1 {
                "one machine".to_string()
            } else {
                format!("{} machines", names.len())
            },
            names.join(", ")
        ));
    }

    // 2. Everything it has waiting, ordinary first.
    state
        .roll
        .step(node, |step| step.stage = MemberStage::Installing);
    let mut changed = Vec::new();
    if !fresh.view.updates.is_empty() {
        if let Visit::Installed { changed: some, .. } =
            install(state, node, false, by, false).await?
        {
            changed.extend(some);
        }
    }
    if fresh.view.platform.pending() {
        if let Visit::Installed { changed: some, .. } =
            install(state, node, false, by, true).await?
        {
            changed.extend(some);
        }
    }

    // 3. Down it goes.
    state
        .roll
        .step(node, |step| step.stage = MemberStage::Restarting);
    state
        .peers
        .restart(&member)
        .await
        .map_err(|err| format!("it would not restart: {err}"))?;

    // 4. Back up, running the kernel it has, and online again.
    state
        .roll
        .step(node, |step| step.stage = MemberStage::Rejoining);
    await_rejoin(state, node).await?;

    // 5. Back into service. Its machines stay where they went.
    state
        .roll
        .step(node, |step| step.stage = MemberStage::Returning);
    state
        .peers
        .exit_maintenance(&member, by)
        .await
        .map_err(|err| {
            format!(
                "it updated and restarted, but could not be put back into service: {err}. It is \
                 running and healthy and its machines can be moved back to it once it is."
            )
        })?;

    Ok(Visit::Installed {
        changed,
        restart_required: false,
    })
}

/// Install one set on one member and wait for the transaction.
async fn install(
    state: &Arc<AppState>,
    node: &str,
    is_local: bool,
    by: &str,
    platform: bool,
) -> Result<Visit, String> {
    let request = ApplyRequest {
        platform,
        // The coordinator holds the operator's acknowledgement; a peer takes
        // its own from the route, never from the wire. See `api/peer.rs`.
        i_understand_the_kernel_moves: platform,
    };
    let started = if is_local {
        crate::updates::begin(state, by, request)
            .await
            .map_err(|err| err.to_string())?
    } else {
        let member = peer(state, node)?;
        state
            .peers
            .apply_updates(&member, platform, by)
            .await
            .map_err(|err| err.to_string())?
    };

    await_member(state, node, is_local, started.started_at).await
}

/// Wait for one member's transaction, by whichever of the two answers arrives.
///
/// The transaction feed is the direct answer and is used while it lasts. But a
/// member installing `lumen-controlplane` restarts the daemon that holds that
/// feed: it stops answering for a few seconds, then comes back reporting no
/// transaction at all. That is a **success** — the transaction ran as a
/// transient systemd unit and outlived the process that started it — and the
/// evidence is what the package database now says is waiting.
///
/// So an empty or replaced feed sends the question to the package database
/// instead. Both paths end at a fact about the node; neither guesses.
async fn await_member(
    state: &Arc<AppState>,
    node: &str,
    is_local: bool,
    started_at: u64,
) -> Result<Visit, String> {
    let deadline = tokio::time::Instant::now() + MEMBER_DEADLINE;
    let mut interval = POLL_FIRST;
    // Whether the member has stopped answering at some point. Only then is an
    // absent feed evidence of a restart rather than of a member that never
    // started the transaction at all.
    let mut went_away = false;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "it has been installing for {} minutes and has not finished. The transaction may \
                 still be running — this walk stopped watching, it did not stop the update.",
                MEMBER_DEADLINE.as_secs() / 60
            ));
        }
        tokio::time::sleep(interval).await;
        interval = (interval * 2).min(POLL_MAX);

        let asked = match ask(state, node, is_local, false).await {
            Ok(asked) => asked,
            Err(_) => {
                // Expected while it restarts its control plane. The deadline
                // is what decides a member is genuinely gone.
                went_away = true;
                continue;
            }
        };

        match asked.progress.as_ref() {
            // Its own account of the transaction we started.
            Some(progress) if progress.started_at == started_at => match progress.phase {
                WorkflowPhase::Running => continue,
                WorkflowPhase::Complete => {
                    return Ok(Visit::Installed {
                        changed: progress.changed.clone(),
                        restart_required: progress
                            .reboot
                            .as_ref()
                            .is_some_and(|reboot| reboot.required),
                    })
                }
                WorkflowPhase::Failed => {
                    return Err(progress
                        .error
                        .clone()
                        .unwrap_or_else(|| "the package manager gave no reason".to_string()))
                }
            },
            // A feed that is absent, or is about some other transaction, on a
            // member that stopped answering: its control plane restarted. The
            // package database is asked instead.
            _ if went_away => return settled(state, node, is_local).await,
            // It has not published the transaction yet. Keep asking.
            _ => continue,
        }
    }
}

/// The second answer: what is still waiting on a member whose control plane
/// restarted mid-transaction.
///
/// An empty list is the transaction having done what it was asked. Anything
/// left is reported as the failure it is — and named, because "these three
/// packages are still waiting" is what an operator retries against.
async fn settled(state: &Arc<AppState>, node: &str, is_local: bool) -> Result<Visit, String> {
    let after = ask(state, node, is_local, true).await.map_err(|err| {
        format!(
            "its control plane restarted during the transaction — which is what installing Lumen \
             does — and it could not then be asked what it ended up with: {err}"
        )
    })?;

    if after.view.updates.is_empty() {
        // What changed cannot be recovered: the process that would have
        // recorded it is the one that restarted. Saying so is better than
        // reporting an empty list as though nothing moved.
        return Ok(Visit::Installed {
            changed: Vec::new(),
            restart_required: after.view.reboot.required,
        });
    }
    Err(format!(
        "its control plane restarted during the transaction and {} update{} still waiting \
         afterwards: {}",
        after.view.updates.len(),
        if after.view.updates.len() == 1 {
            " was"
        } else {
            "s were"
        },
        after
            .view
            .updates
            .iter()
            .map(|update| update.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Take one member out of service and wait for its machines to move.
///
/// Returns what would not move. An empty list is what "safe to restart" means,
/// and it is the drain's own answer rather than anything inferred here — the
/// node being emptied is the only one that can see what is still on it.
async fn drain_member(
    state: &Arc<AppState>,
    member: &lumen_cluster::EnvironmentNode,
    node: &str,
    by: &str,
) -> Result<Vec<crate::maintenance::Stranded>, String> {
    let started = state
        .peers
        .enter_maintenance(member, by)
        .await
        .map_err(|err| format!("it could not be taken out of service: {err}"))?;
    if started.phase != WorkflowPhase::Running {
        return Ok(started.stranded);
    }

    let deadline = tokio::time::Instant::now() + DRAIN_DEADLINE;
    let mut interval = POLL_FIRST;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "its machines have been moving for {} minutes and have not finished. It has been \
                 left out of service; nothing was installed and nothing was restarted.",
                DRAIN_DEADLINE.as_secs() / 60
            ));
        }
        tokio::time::sleep(interval).await;
        interval = (interval * 2).min(POLL_MAX);

        // Unlike a transaction, a drain does not restart the daemon reporting
        // it — so a member that stops answering here has genuinely gone away,
        // and the deadline is what says so.
        let Ok(Some(progress)) = state.peers.drain_progress(member).await else {
            tracing::debug!(%node, "no drain to report yet");
            continue;
        };
        match progress.phase {
            WorkflowPhase::Running => continue,
            // A drain is `Complete` even when it stranded something: the node
            // *is* out of service, which is what was asked. Whether it is
            // empty is the stranded list's job, and it is the caller's rule.
            WorkflowPhase::Complete => {
                tracing::info!(
                    %node,
                    stranded = progress.stranded.len(),
                    "the member finished draining"
                );
                return Ok(progress.stranded);
            }
            WorkflowPhase::Failed => {
                return Err(format!(
                    "it could not be emptied: {}",
                    progress
                        .error
                        .unwrap_or_else(|| "no reason was given".to_string())
                ))
            }
        }
    }
}

/// Wait for a member to come back from its restart.
///
/// Three things have to be true, and each rules out a different way of being
/// wrong:
///
/// - it answers at all — the daemon is up;
/// - it is **not** reporting an outstanding restart — the new kernel is the
///   one running, which is the entire reason it was rebooted, and is the check
///   that catches a node that came up on its old kernel;
/// - this node's cluster view has it online — corosync has it back, so the
///   next member can be taken down without the cluster losing two at once.
///
/// The third is read from the coordinator rather than asked of the member,
/// deliberately: whether the *rest of the cluster* can see it is the question
/// that matters before the next node goes down, and a node cannot answer that
/// about itself.
///
/// ## Nothing here fails early
///
/// Every unmet condition is a reason to keep waiting, and only the deadline
/// ends it. That is deliberate, and it is about one specific way of being
/// wrong: `went_down` is inferred from the member not answering, and a member
/// can stop answering for a moment without having rebooted — a dropped
/// connection during the seconds between being told to restart and going. If
/// an outstanding restart were treated as a failure the first time it were
/// seen, that blip would be reported as "it came back on its old kernel" about
/// a node that had not yet left.
///
/// So the state is remembered rather than acted on, and the deadline reports
/// the *last* thing that was true. A node genuinely stuck on its old kernel
/// still says so; a node that blipped just waits a little longer.
async fn await_rejoin(state: &Arc<AppState>, node: &str) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + REJOIN_DEADLINE;
    let mut interval = POLL_FIRST;
    // Not until it has actually gone does its answering mean anything: a node
    // told to restart answers normally for the second or two before it does.
    let mut went_down = false;
    // The last unmet condition, so the deadline can say which one it was.
    let mut stuck_on: Option<String> = None;

    loop {
        if tokio::time::Instant::now() >= deadline {
            let minutes = REJOIN_DEADLINE.as_secs() / 60;
            return Err(match stuck_on {
                Some(reason) => format!(
                    "it restarted but has not come back properly within {minutes} minutes: \
                     {reason}. It is out of service and nothing after it was attempted — its \
                     machines were moved off before it went down, so they are running elsewhere."
                ),
                None => format!(
                    "it was restarted and has not come back within {minutes} minutes. It is out \
                     of service and nothing after it was attempted — its machines were moved off \
                     before it went down, so they are running elsewhere."
                ),
            });
        }
        tokio::time::sleep(interval).await;
        interval = (interval * 2).min(POLL_MAX);

        let Ok(answer) = ask(state, node, false, false).await else {
            went_down = true;
            continue;
        };
        if !went_down {
            continue;
        }
        if answer.view.reboot.required {
            // Up, and running a kernel it no longer has. Remembered rather
            // than acted on — see the note above.
            stuck_on = Some(format!(
                "it is answering but still reports an outstanding restart ({}), and restarting it \
                 a second time would be guessing",
                answer
                    .view
                    .reboot
                    .reason
                    .as_deref()
                    .unwrap_or("its newest kernel is not the one running")
            ));
            continue;
        }
        if !online(state, node).await {
            stuck_on = Some(
                "it is answering and running its new kernel, but this node's cluster view does \
                 not have it online yet"
                    .to_string(),
            );
            continue;
        }
        return Ok(());
    }
}

/// Whether this node's cluster view has that member online.
///
/// A cluster that cannot be read at all answers `false`, and the deadline
/// decides from there. That is the conservative direction: the question being
/// asked is "may the next node go down", and "the cluster could not be asked"
/// is not a yes.
async fn online(state: &Arc<AppState>, node: &str) -> bool {
    let Ok(Some(membership)) = state.cluster.environment_record() else {
        return false;
    };
    let Some(cluster) = membership
        .node(state.cluster.node())
        .and_then(|local| local.cluster.clone())
    else {
        return false;
    };
    let Ok(view) = state.cluster.cluster(&cluster).await else {
        return false;
    };
    if view.error.is_some() {
        return false;
    }
    view.nodes
        .iter()
        .any(|member| member.node == node && member.online)
}

/// Put a member back into service after its leg failed.
///
/// Best effort by construction: the walk is already reporting a failure, and a
/// second one on top of it would bury the first. What this cannot do is left
/// in the log for an operator who goes looking.
async fn restore(state: &Arc<AppState>, node: &str, by: &str) {
    let Ok(member) = peer(state, node) else {
        return;
    };
    if let Err(err) = state.peers.exit_maintenance(&member, by).await {
        tracing::warn!(%node, "could not put it back into service after a failed update: {err}");
    }
}

/// Ask one member what it has, refreshing its repositories or not.
async fn ask(
    state: &Arc<AppState>,
    node: &str,
    is_local: bool,
    refresh: bool,
) -> Result<NodeUpdates, String> {
    if is_local {
        if refresh {
            state.updates.check().await.map_err(|err| err.to_string())?;
        }
        let view = state.updates.view().await.map_err(|err| err.to_string())?;
        return Ok(NodeUpdates {
            view,
            progress: state.update_job.get(),
        });
    }

    let member = peer(state, node)?;
    let asked = if refresh {
        state.peers.check_updates(&member).await
    } else {
        state.peers.updates(&member).await
    };
    asked.map_err(|err| err.to_string())
}

fn peer(state: &Arc<AppState>, node: &str) -> Result<lumen_cluster::EnvironmentNode, String> {
    let nodes = state
        .cluster
        .environment_nodes()
        .map_err(|err| err.to_string())?;
    nodes
        .into_iter()
        .find(|candidate| candidate.name == node)
        .ok_or_else(|| format!("\"{node}\" is no longer a node in this environment"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
