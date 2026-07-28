//! Taking a node out of service, and draining it first.
//!
//! Maintenance mode is two facts and one long-running job. The facts are the
//! membership record's flag — which is what stops every member's HA manager
//! treating this node's absence as a failure — and Pacemaker standby, which
//! moves the resources Pacemaker owns. Both are [`lumen_cluster`]'s, set
//! together, and this module does not reimplement either.
//!
//! The job is the drain, and it lives here because it is the one part that
//! spans domains: the machines are `virt`'s, where they may legally run is
//! `drbd`'s, and who is eligible to receive them is `cluster`'s. Nothing below
//! the control plane can see all three.
//!
//! ## The order, and why
//!
//! Out of service **first**, then drain. The flag has to be true before a
//! single machine moves, or another node's HA sweep may place work onto the
//! node being emptied while it is being emptied. It also means a node that
//! dies half-way through a drain is a node the cluster leaves alone, which is
//! the conservative failure — its machines stay where they are rather than
//! being restarted by a sweep that thinks it lost a member.
//!
//! The cost is that standby moves the Management VIP off this node at the
//! start of the window rather than the end. That is the right end state — the
//! node is about to be rebooted — but an operator who reached the console
//! *through* the VIP will find the rest of the drain reported by a different
//! node, which has no progress to show. Work on a node through its own
//! address.
//!
//! ## What a drain will not do
//!
//! It never shuts a machine down to get it moved, and it never moves one onto
//! a replica that is not current. A machine that cannot migrate — no
//! replicated disk, media attached, no eligible target — is left running and
//! **named** in the result. A node whose drain ends with machines still on it
//! is out of service but not empty, and saying so plainly is the difference
//! between an operator who reboots and one who knows not to yet.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lumen_cluster::join::{StepProgress, StepState, WorkflowPhase};
use lumen_drbd::VmVolumes;
use lumen_virt::DomainState;
use serde::Serialize;

use crate::AppState;

/// The step name for the one thing that is not a machine.
const OUT_OF_SERVICE: &str = "out-of-service";

/// A machine that is still running on the node after the drain, and the
/// sentence explaining why it did not move.
#[derive(Debug, Clone, Serialize)]
pub struct Stranded {
    pub vmid: u32,
    pub name: String,
    pub reason: String,
}

/// The whole of a drain, as the console polls it — the same shape a cluster
/// create publishes, because it is the same kind of thing: a multi-step job
/// with one step per unit of work.
#[derive(Debug, Clone, Serialize)]
pub struct MaintenanceProgress {
    pub node: String,
    pub cluster: String,
    pub phase: WorkflowPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub steps: Vec<StepProgress>,
    /// Machines still running here when the drain finished. Empty is the
    /// answer that means "this node is safe to reboot".
    pub stranded: Vec<Stranded>,
}

impl MaintenanceProgress {
    /// Whether the node was actually emptied — what the console turns into
    /// "safe to restart" and what the power guard wants to know.
    pub fn drained(&self) -> bool {
        self.phase == WorkflowPhase::Complete && self.stranded.is_empty()
    }
}

/// Shared handle: the drain writes it, the progress endpoint reads it. One at
/// a time — a second drain of the same node would fight the first over the
/// same machines.
#[derive(Clone, Default)]
pub struct DrainHandle {
    inner: Arc<Mutex<Option<MaintenanceProgress>>>,
}

impl DrainHandle {
    pub fn get(&self) -> Option<MaintenanceProgress> {
        self.inner.lock().expect("drain progress lock").clone()
    }

    pub fn running(&self) -> bool {
        self.get()
            .is_some_and(|progress| progress.phase == WorkflowPhase::Running)
    }

    fn set(&self, progress: MaintenanceProgress) {
        *self.inner.lock().expect("drain progress lock") = Some(progress);
    }

    /// Rewrite one step in place, leaving the rest of the feed alone.
    fn step(&self, name: &str, state: StepState, node: Option<&str>, detail: Option<String>) {
        let mut guard = self.inner.lock().expect("drain progress lock");
        let Some(progress) = guard.as_mut() else {
            return;
        };
        if let Some(step) = progress.steps.iter_mut().find(|s| s.step == name) {
            step.state = state;
            step.node = node.map(str::to_string);
            step.detail = detail;
        }
    }

    fn finish(&self, phase: WorkflowPhase, error: Option<String>, stranded: Vec<Stranded>) {
        let mut guard = self.inner.lock().expect("drain progress lock");
        let Some(progress) = guard.as_mut() else {
            return;
        };
        progress.phase = phase;
        progress.error = error;
        progress.stranded = stranded;
    }
}

/// Take this node out of service, and move its running machines off it unless
/// the operator said not to.
///
/// Answers once the node is flagged and the feed is published; a drain runs on
/// from there and is watched through [`DrainHandle::get`]. One shape either
/// way — whether the machines were moved or deliberately left, the question
/// "what is still running here" has one place to be answered.
pub async fn begin(
    state: &Arc<AppState>,
    by: &str,
    evacuate: bool,
) -> Result<MaintenanceProgress, crate::error::ApiError> {
    if state.drain.running() {
        return Err(crate::error::ApiError::Conflict(
            "This node is already being taken out of service. Watch that one finish rather than \
             starting a second — they would fight over the same machines."
                .to_string(),
        ));
    }

    let node = state.cluster.node().to_string();
    // Out of service first: the guards, the record, and standby all live in
    // the cluster domain, and a refusal here means nothing has moved.
    let view = state.cluster.set_maintenance(&node, true, by).await?;

    // One step per machine that has to move, named before anything is
    // attempted so the console shows the whole of the work up front.
    let running = running_machines(state).await?;
    let mut steps = vec![StepProgress {
        step: OUT_OF_SERVICE.to_string(),
        node: Some(node.clone()),
        state: StepState::Done,
        detail: Some(if view.quorum_safe {
            "The cluster stays quorate without this node.".to_string()
        } else {
            "This node's vote is load-bearing — the cluster loses quorum if it goes down."
                .to_string()
        }),
    }];
    if evacuate {
        steps.extend(running.iter().map(|machine| StepProgress {
            step: machine.name.clone(),
            node: None,
            state: StepState::Pending,
            detail: None,
        }));
    }

    let mut progress = MaintenanceProgress {
        node: node.clone(),
        cluster: view.cluster.clone(),
        phase: WorkflowPhase::Running,
        error: None,
        steps,
        stranded: Vec::new(),
    };

    if !evacuate {
        // Nothing to wait for, so the answer is the final one. The machines
        // still running here are named anyway: "out of service" and "empty"
        // are different facts and an operator about to reboot needs both.
        progress.phase = WorkflowPhase::Complete;
        progress.stranded = running
            .iter()
            .map(|machine| Stranded {
                vmid: machine.vmid,
                name: machine.name.clone(),
                reason: "Still running here — the machines were not asked to move.".to_string(),
            })
            .collect();
        state.drain.set(progress.clone());
        return Ok(progress);
    }

    state.drain.set(progress.clone());
    let background = state.clone();
    let owner = by.to_string();
    tokio::spawn(async move {
        drain(&background, running, &owner).await;
    });
    Ok(progress)
}

/// Put the node back into service. The drain is not undone — machines that
/// moved stay where they are, exactly as an HA restart does, because failback
/// that fires on its own turns one window into two moves.
pub async fn end(
    state: &Arc<AppState>,
    by: &str,
) -> Result<lumen_cluster::MaintenanceView, crate::error::ApiError> {
    let node = state.cluster.node().to_string();
    Ok(state.cluster.set_maintenance(&node, false, by).await?)
}

/// One running machine and what it needs to move.
struct Machine {
    vmid: u32,
    name: String,
    devices: Vec<String>,
}

async fn running_machines(state: &Arc<AppState>) -> Result<Vec<Machine>, crate::error::ApiError> {
    let listed = state.virt.list().await?;
    Ok(listed
        .nodes
        .into_iter()
        .flat_map(|node| node.vms)
        .filter(|vm| vm.state == DomainState::Running)
        .map(|vm| Machine {
            vmid: vm.vmid,
            name: vm.name,
            devices: vm.disks.into_iter().map(|disk| disk.source).collect(),
        })
        .collect())
}

/// The drain proper. Every machine is attempted; one that cannot move does
/// not stop the ones after it, because an operator needs the whole list of
/// what is stuck, not the first entry of it.
async fn drain(state: &Arc<AppState>, machines: Vec<Machine>, by: &str) {
    let node = state.cluster.node().to_string();
    let mut stranded: Vec<Stranded> = Vec::new();
    // How many machines this drain has already sent to each target, so a node
    // with three machines and two survivors does not put all three in one
    // place. Real capacity lives on the target and cannot be read from here;
    // spreading the count is the honest approximation.
    let mut placed: HashMap<String, usize> = HashMap::new();

    for machine in machines {
        state
            .drain
            .step(&machine.name, StepState::Running, None, None);

        let target = match choose_target(state, &node, &machine, &placed).await {
            Ok(target) => target,
            Err(reason) => {
                state
                    .drain
                    .step(&machine.name, StepState::Failed, None, Some(reason.clone()));
                stranded.push(Stranded {
                    vmid: machine.vmid,
                    name: machine.name,
                    reason,
                });
                continue;
            }
        };

        let destination = match core_uri_of(state, &target) {
            Ok(uri) => uri,
            Err(reason) => {
                state.drain.step(
                    &machine.name,
                    StepState::Failed,
                    Some(&target),
                    Some(reason.clone()),
                );
                stranded.push(Stranded {
                    vmid: machine.vmid,
                    name: machine.name,
                    reason,
                });
                continue;
            }
        };

        let result = state
            .virt
            .migrate(machine.vmid, &target, &destination)
            .await;
        state.tasks.record(
            machine.vmid,
            "migrate",
            format!("Evacuated to {target} for maintenance"),
            by.to_string(),
            result.as_ref().err().map(|err| err.to_string()),
        );
        match result {
            Ok(_) => {
                *placed.entry(target.clone()).or_default() += 1;
                state
                    .drain
                    .step(&machine.name, StepState::Done, Some(&target), None);
                tracing::info!(vmid = machine.vmid, %target, "evacuated for maintenance");
            }
            Err(err) => {
                let reason = err.to_string();
                state.drain.step(
                    &machine.name,
                    StepState::Failed,
                    Some(&target),
                    Some(reason.clone()),
                );
                tracing::warn!(vmid = machine.vmid, %target, "a machine would not evacuate: {err}");
                stranded.push(Stranded {
                    vmid: machine.vmid,
                    name: machine.name,
                    reason,
                });
            }
        }
    }

    // Complete either way: the node *is* out of service, which is what was
    // asked for. Whether it is empty is the `stranded` list's job to say, and
    // calling a drain "failed" because one machine has a CD-ROM attached
    // would bury that distinction.
    state.drain.finish(WorkflowPhase::Complete, None, stranded);
}

/// Where one machine may go: a member that holds a **current** replica of
/// every disk, is online, and is not itself out of service — carrying the
/// fewest machines this drain has placed so far.
///
/// `common_members` answers where a machine *may* run, which is a question
/// about the record. Whether the replica there is current is a question about
/// DRBD, and migrating onto an `Inconsistent` or `SyncTarget` replica would
/// leave the machine serving its disks over the network from the node being
/// rebooted.
async fn choose_target(
    state: &Arc<AppState>,
    node: &str,
    machine: &Machine,
    placed: &HashMap<String, usize>,
) -> Result<String, String> {
    if machine.devices.is_empty() {
        return Err("This machine has no disks, so nothing names where it could run.".to_string());
    }
    let members = state
        .drbd
        .common_members(&machine.devices)
        .await
        .map_err(|err| format!("Where this machine can run could not be worked out: {err}"))?;

    let eligible = eligible_members(state, node).await?;
    let current = current_replicas(state, &machine.devices).await;

    let mut candidates: Vec<&String> = members
        .iter()
        .filter(|m| *m != node && eligible.contains(*m) && current.contains(*m))
        .collect();
    if candidates.is_empty() {
        let known = members
            .iter()
            .filter(|m| *m != node)
            .cloned()
            .collect::<Vec<_>>();
        return Err(if known.is_empty() {
            "No other member holds a replica of every one of this machine's disks.".to_string()
        } else {
            format!(
                "No member that could take this machine is ready for it — {} hold replicas, but \
                 none is both in service and fully in sync.",
                known.join(", ")
            )
        });
    }
    // Fewest placed first, then by name so two consoles watching the same
    // drain describe it the same way.
    candidates.sort_by_key(|name| (placed.get(*name).copied().unwrap_or(0), (*name).clone()));
    Ok(candidates[0].clone())
}

/// Members of this node's cluster that are online and not out of service.
async fn eligible_members(state: &Arc<AppState>, node: &str) -> Result<Vec<String>, String> {
    let membership = state
        .cluster
        .environment_record()
        .map_err(|err| err.to_string())?
        .ok_or_else(|| "This node has not joined an environment.".to_string())?;
    let cluster = membership
        .node(node)
        .and_then(|n| n.cluster.clone())
        .ok_or_else(|| "This node is not in a cluster.".to_string())?;
    let view = state
        .cluster
        .cluster(&cluster)
        .await
        .map_err(|err| err.to_string())?;
    Ok(view
        .nodes
        .into_iter()
        .filter(|n| n.online && n.maintenance.is_none())
        .map(|n| n.node)
        .collect())
}

/// Nodes whose replica of **every** one of these devices reads `UpToDate`.
///
/// A volume this node cannot see the state of contributes nothing, which
/// makes the answer conservative by construction: an unreadable DRBD means no
/// candidate is confirmed current, and the drain says so rather than moving a
/// machine onto a replica it knows nothing about.
async fn current_replicas(state: &Arc<AppState>, devices: &[String]) -> Vec<String> {
    let Ok(volumes) = state.drbd.volumes().await else {
        return Vec::new();
    };
    let all: Vec<&lumen_drbd::VolumeView> = volumes
        .clusters
        .iter()
        .flat_map(|cluster| cluster.volumes.iter())
        .collect();

    let mut current: Option<Vec<String>> = None;
    for device in devices {
        let Some(volume) = all.iter().find(|volume| &volume.device == device) else {
            return Vec::new();
        };
        let up_to_date: Vec<String> = volume
            .replicas
            .iter()
            .filter(|replica| replica.disk_state.as_deref() == Some("UpToDate"))
            .map(|replica| replica.node.clone())
            .collect();
        current = Some(match current {
            None => up_to_date,
            Some(previous) => previous
                .into_iter()
                .filter(|node| up_to_date.contains(node))
                .collect(),
        });
    }
    current.unwrap_or_default()
}

/// The migration destination: the target's seat on its cluster's Core
/// network, the same link its disks already replicate over. Shared with the
/// single-machine migrate handler.
pub fn core_uri_of(state: &AppState, target: &str) -> Result<String, String> {
    let membership = state
        .cluster
        .environment_record()
        .map_err(|err| err.to_string())?
        .ok_or_else(|| "This node has not joined an environment.".to_string())?;
    let address = membership
        .clusters
        .iter()
        .find_map(|record| {
            record
                .networks
                .core
                .members
                .iter()
                .find(|m| m.node == target)
                .map(|m| m.address)
        })
        .ok_or_else(|| {
            format!(
                "\"{target}\" has no seat on a cluster's Core network, so there is no path to \
                 migrate over."
            )
        })?;
    Ok(format!("qemu+tcp://{address}/system"))
}
