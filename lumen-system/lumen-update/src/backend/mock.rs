//! An in-memory package manager: what is waiting, what a transaction would do
//! to it, and what was asked.
//!
//! Every test in this crate runs against this, and so does every control-plane
//! API test. It is a real model rather than a set of canned answers — applying
//! updates removes them from what is waiting, and applying a kernel makes the
//! running one stale — because the properties worth testing are sequences: a
//! kernel is installed, *therefore* a restart is outstanding; the platform set
//! was excluded, *therefore* it is still waiting afterwards.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::backend::UpdateBackend;
use crate::error::{Result, UpdateError};
use crate::model::{ApplyPlan, ApplyReport, KernelState, Resolution, Update};

#[derive(Default)]
struct State {
    available: Vec<Update>,
    running_kernel: String,
    newest_kernel: Option<String>,
    /// The kernel release a kernel upgrade would land on, so an applied
    /// transaction can make the running kernel stale the way a real one does.
    kernel_after: Option<String>,
    resolve_failure: Option<String>,
    apply_failure: Option<String>,
    /// Every plan this backend was handed, in order.
    applied: Vec<ApplyPlan>,
    checks: usize,
}

pub struct MockUpdates {
    inner: Mutex<State>,
}

impl Default for MockUpdates {
    fn default() -> Self {
        Self::new()
    }
}

impl MockUpdates {
    /// A node with nothing waiting and a kernel it is already running.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(State {
                running_kernel: "6.12.0-211.7.3.el10_2.x86_64".to_string(),
                newest_kernel: Some("6.12.0-211.7.3.el10_2.x86_64".to_string()),
                ..State::default()
            }),
        }
    }

    pub fn with_updates(self, updates: Vec<Update>) -> Self {
        self.inner.lock().expect("mock lock").available = updates;
        self
    }

    /// The kernel a platform transaction would land on. Setting it is what
    /// makes "installed a kernel, so a restart is outstanding" testable.
    pub fn landing_on_kernel(self, release: &str) -> Self {
        self.inner.lock().expect("mock lock").kernel_after = Some(release.to_string());
        self
    }

    pub fn with_kernel(self, running: &str, newest: Option<&str>) -> Self {
        let mut state = self.inner.lock().expect("mock lock");
        state.running_kernel = running.to_string();
        state.newest_kernel = newest.map(str::to_string);
        drop(state);
        self
    }

    /// Make the dry run refuse — a kernel whose modules have not caught up.
    pub fn blocking_resolution(self, why: &str) -> Self {
        self.inner.lock().expect("mock lock").resolve_failure = Some(why.to_string());
        self
    }

    pub fn failing_apply(self, why: &str) -> Self {
        self.inner.lock().expect("mock lock").apply_failure = Some(why.to_string());
        self
    }

    /// Every plan handed to this backend, in order.
    pub fn applied(&self) -> Vec<ApplyPlan> {
        self.inner.lock().expect("mock lock").applied.clone()
    }

    /// How many times the repositories were asked.
    pub fn checks(&self) -> usize {
        self.inner.lock().expect("mock lock").checks
    }

    /// What is still waiting.
    pub fn available(&self) -> Vec<Update> {
        self.inner.lock().expect("mock lock").available.clone()
    }
}

#[async_trait]
impl UpdateBackend for MockUpdates {
    async fn check(&self) -> Result<Vec<Update>> {
        let mut state = self.inner.lock().expect("mock lock");
        state.checks += 1;
        Ok(state.available.clone())
    }

    async fn resolve(&self, packages: &[String]) -> Result<Resolution> {
        let state = self.inner.lock().expect("mock lock");
        if packages.is_empty() {
            return Ok(Resolution {
                ok: true,
                detail: "Nothing to resolve.".into(),
            });
        }
        Ok(match &state.resolve_failure {
            Some(why) => Resolution {
                ok: false,
                detail: why.clone(),
            },
            None => Resolution {
                ok: true,
                detail: "These can be installed together.".into(),
            },
        })
    }

    async fn apply(&self, plan: &ApplyPlan) -> Result<ApplyReport> {
        let mut state = self.inner.lock().expect("mock lock");
        state.applied.push(plan.clone());
        if let Some(why) = state.apply_failure.clone() {
            return Err(UpdateError::backend(anyhow::anyhow!("{why}")));
        }

        // What this transaction covers: the named packages, or everything not
        // excluded when nothing is named.
        let covered: Vec<Update> = state
            .available
            .iter()
            .filter(|update| covers(plan, &update.name))
            .cloned()
            .collect();
        state.available.retain(|update| !covers(plan, &update.name));

        // A kernel that was installed becomes the newest one, exactly as it
        // does on a real node — and the running one does not change until the
        // node restarts, which is the whole point.
        if covered
            .iter()
            .any(|update| update.name.starts_with("kernel"))
        {
            if let Some(after) = state.kernel_after.clone() {
                state.newest_kernel = Some(after);
            }
        }

        Ok(ApplyReport {
            changed: covered.into_iter().map(|update| update.name).collect(),
            log: "mock transaction complete\n".to_string(),
        })
    }

    async fn kernel(&self) -> Result<KernelState> {
        let state = self.inner.lock().expect("mock lock");
        Ok(KernelState {
            running: state.running_kernel.clone(),
            newest: state.newest_kernel.clone(),
        })
    }
}

/// Whether a plan would touch a package: named explicitly, or not excluded
/// when the plan names nothing. The only globs in play are `prefix*`, which is
/// what [`crate::model::ApplyPlan::ordinary`] builds.
fn covers(plan: &ApplyPlan, name: &str) -> bool {
    if !plan.packages.is_empty() {
        return plan.packages.iter().any(|package| package == name);
    }
    !plan
        .exclude
        .iter()
        .any(|glob| match glob.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => glob == name,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_ordinary_transaction_leaves_the_platform_set_alone() {
        let mock = MockUpdates::new().with_updates(vec![
            Update::new("lumen-controlplane", "0.4.0-1.el10", "lumen"),
            Update::new("kernel-core", "6.12.0-212.el10", "baseos"),
            Update::new("kmod-zfs-2.3", "2.3.4-1.el10", "zfs-2.3-kmod"),
        ]);

        let report = mock.apply(&ApplyPlan::ordinary()).await.unwrap();
        assert_eq!(report.changed, vec!["lumen-controlplane"]);

        // The kernel and its module are still waiting.
        let left: Vec<String> = mock.available().into_iter().map(|u| u.name).collect();
        assert_eq!(left, vec!["kernel-core", "kmod-zfs-2.3"]);
    }

    #[tokio::test]
    async fn installing_a_kernel_makes_the_running_one_stale() {
        let mock = MockUpdates::new()
            .with_updates(vec![
                Update::new("kernel-core", "6.12.0-212.el10", "baseos"),
                Update::new("kmod-zfs-2.3", "2.3.4-1.el10", "zfs-2.3-kmod"),
            ])
            .landing_on_kernel("6.12.0-212.el10.x86_64");

        assert!(!mock.kernel().await.unwrap().stale());
        let plan = ApplyPlan::platform(&mock.available());
        mock.apply(&plan).await.unwrap();

        let kernel = mock.kernel().await.unwrap();
        assert!(kernel.stale(), "a new kernel is installed but not running");
        assert_eq!(kernel.running, "6.12.0-211.7.3.el10_2.x86_64");
    }
}
