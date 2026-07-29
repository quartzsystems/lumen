//! Installing updates on this node, while the console watches.
//!
//! The decisions — what may be installed, what must move together, what the
//! kernel rule is — all live in [`lumen_update`]. What lives here is the one
//! thing a domain crate should not own: a transaction takes minutes, and an
//! HTTP request must not.
//!
//! So an apply is started, answered immediately with `202`, and watched through
//! [`UpdateHandle::get`] — the same shape a cluster create and a drain already
//! publish, and for the same reason.
//!
//! ## There are no steps
//!
//! A drain publishes one step per machine because it moves them one at a time
//! and knows which one it is on. A transaction is not like that: the package
//! manager is handed one set and runs it as a unit, and nothing it prints
//! comes back through the privileged runner until it has finished. Inventing
//! per-package steps and marking them all done at the end would be a progress
//! bar that lies.
//!
//! What the console gets instead is honest: whether it is running, how long it
//! has been running, and — when it is over — what changed, what the package
//! manager said, and whether the node now needs restarting.
//!
//! ## Nothing here restarts anything
//!
//! A platform update leaves a kernel installed and not running, and the fix is
//! a restart. That restart is *not* taken here, and not offered as a checkbox
//! either. It goes through the power route in `src/api/system.rs`, which
//! already refuses to take down a node its cluster cannot spare and already
//! points an operator at maintenance mode — where the machines are moved off
//! first. Duplicating any of that here would mean a second, weaker copy of the
//! guard that matters most.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use lumen_cluster::join::WorkflowPhase;
use lumen_update::{ApplyRequest, RebootState};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::AppState;

/// Which of the two decisions a transaction was.
///
/// An enum rather than the `&'static str` this used to be, because a
/// cluster-wide update reads a peer's transaction back off the wire and a
/// borrowed string cannot be deserialized. The JSON is identical —
/// `"ordinary"` and `"platform"`, the same two words the console already
/// switches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionKind {
    Ordinary,
    Platform,
}

impl TransactionKind {
    fn of(request: &ApplyRequest) -> Self {
        if request.platform {
            TransactionKind::Platform
        } else {
            TransactionKind::Ordinary
        }
    }
}

/// The whole of an update, as the console polls it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateProgress {
    pub node: String,
    /// `ordinary` or `platform` — which of the two decisions this was, so a
    /// console that reloaded mid-transaction still knows what it is watching.
    pub kind: TransactionKind,
    pub phase: WorkflowPhase,
    /// Unix seconds. The console shows elapsed time against the node's clock
    /// rather than counting locally, exactly as the power page does.
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    /// Who asked, as the `user@realm` principal.
    pub by: String,
    /// What the package manager reported changing. Empty until it has
    /// finished.
    #[serde(default)]
    pub changed: Vec<String>,
    /// The tail of its output. The whole of it is in the journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Read after the transaction: whether this node is now running a kernel
    /// it no longer has. `None` while it is still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reboot: Option<RebootState>,
}

/// Shared handle: the transaction writes it, the progress route reads it.
///
/// One at a time, and not merely as a matter of taste — the package manager
/// takes a lock of its own, so a second transaction would block on the first
/// and then report a failure that says nothing about what the operator asked
/// for. Refusing here produces a sentence they can act on.
#[derive(Clone, Default)]
pub struct UpdateHandle {
    inner: Arc<Mutex<Option<UpdateProgress>>>,
}

impl UpdateHandle {
    pub fn get(&self) -> Option<UpdateProgress> {
        self.inner.lock().expect("update progress lock").clone()
    }

    pub fn running(&self) -> bool {
        self.get()
            .is_some_and(|progress| progress.phase == WorkflowPhase::Running)
    }

    fn set(&self, progress: UpdateProgress) {
        *self.inner.lock().expect("update progress lock") = Some(progress);
    }

    fn finish(
        &self,
        phase: WorkflowPhase,
        error: Option<String>,
        changed: Vec<String>,
        log: Option<String>,
        reboot: Option<RebootState>,
    ) {
        let mut guard = self.inner.lock().expect("update progress lock");
        let Some(progress) = guard.as_mut() else {
            return;
        };
        progress.phase = phase;
        progress.finished_at = Some(now());
        progress.error = error;
        progress.changed = changed;
        progress.log = log;
        progress.reboot = reboot;
    }
}

/// Start installing, and answer as soon as it is under way.
///
/// The guards are [`lumen_update`]'s and run inside the transaction task —
/// with one exception. "Something is already running" is checked here, before
/// anything is published, because it is the only refusal that is about this
/// daemon rather than about the packages.
///
/// Every other refusal — nothing waiting, the platform set will not resolve,
/// the acknowledgement is missing — is the domain's, and would be lost if it
/// only ever appeared in a background task. So the domain is asked to validate
/// *first*, synchronously, by way of a check; the operator gets a real error
/// on the request they made rather than a `202` followed by a failed job.
pub async fn begin(
    state: &Arc<AppState>,
    by: &str,
    request: ApplyRequest,
) -> Result<UpdateProgress, ApiError> {
    if state.update_job.running() {
        return Err(ApiError::Conflict(
            "This node is already installing updates. Watch that one finish rather than starting \
             a second — the package manager takes a lock, and the second would only wait for the \
             first and then fail."
                .to_string(),
        ));
    }

    // Validate before publishing anything. This costs a repository refresh,
    // which is the same one the transaction would do first anyway.
    let view = state.updates.check().await?;
    if request.platform {
        if !view.platform.pending() {
            return Err(ApiError::NotFound(
                "There is no kernel or storage-module update waiting.".to_string(),
            ));
        }
        if !view.platform.resolves {
            return Err(ApiError::Conflict(format!(
                "These cannot be installed together yet, so Lumen will not install any of them. \
                 The kernel and the storage modules built against it have to move as one set — \
                 installing the kernel alone is what leaves a node unable to import its pool at \
                 the next restart. The package manager said: {}",
                view.platform
                    .detail
                    .as_deref()
                    .unwrap_or("no reason was given")
            )));
        }
        if !request.i_understand_the_kernel_moves {
            return Err(ApiError::Conflict(
                "Installing these replaces the kernel and the storage modules built against it. \
                 The node keeps running the old kernel until it is restarted. Acknowledge \
                 \"i_understand_the_kernel_moves\" to go ahead."
                    .to_string(),
            ));
        }
    } else if view.updates.is_empty() {
        return Err(ApiError::NotFound(
            "There are no updates waiting to be installed.".to_string(),
        ));
    }

    let progress = UpdateProgress {
        node: state.updates.node().to_string(),
        kind: TransactionKind::of(&request),
        phase: WorkflowPhase::Running,
        started_at: now(),
        finished_at: None,
        by: by.to_string(),
        changed: Vec::new(),
        log: None,
        error: None,
        reboot: None,
    };
    state.update_job.set(progress.clone());

    let background = state.clone();
    tokio::spawn(async move {
        run(&background, request).await;
    });
    Ok(progress)
}

/// The transaction proper.
async fn run(state: &Arc<AppState>, request: ApplyRequest) {
    let handle = state.update_job.clone();
    match state.updates.apply(request).await {
        Ok(report) => {
            // Read the kernel state afterwards rather than assuming: whether a
            // restart is outstanding is the one thing the operator will act on
            // next, and it is a fact about the node, not about the report.
            let reboot = state.updates.view().await.ok().map(|view| view.reboot);
            tracing::info!(
                changed = report.changed.len(),
                "update finished on {}",
                state.updates.node()
            );
            handle.finish(
                WorkflowPhase::Complete,
                None,
                report.changed,
                Some(report.log),
                reboot,
            );
        }
        Err(err) => {
            tracing::error!("update failed: {err}");
            let reboot = state.updates.view().await.ok().map(|view| view.reboot);
            handle.finish(
                WorkflowPhase::Failed,
                Some(err.to_string()),
                Vec::new(),
                None,
                reboot,
            );
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
