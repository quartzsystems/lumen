//! There is no package manager to ask, and every call says why.
//!
//! Swapped in when the package manager could not be found at startup, exactly
//! as `lumen_sys`, `lumen_net`, and `lumen_virt` do. A control plane that comes
//! up and explains itself is more useful than one that refuses to start: an
//! operator whose node is misbehaving needs the console more than usual, and
//! Updates is not the only page on it.

use async_trait::async_trait;

use crate::backend::UpdateBackend;
use crate::error::{Result, UpdateError};
use crate::model::{ApplyPlan, ApplyReport, KernelState, Resolution, Update};

pub struct UnavailableUpdates {
    reason: String,
}

impl UnavailableUpdates {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    fn refuse<T>(&self, what: &str) -> Result<T> {
        Err(UpdateError::backend(anyhow::anyhow!(
            "cannot {what}: this node's package manager is unavailable ({})",
            self.reason
        )))
    }
}

#[async_trait]
impl UpdateBackend for UnavailableUpdates {
    async fn check(&self) -> Result<Vec<Update>> {
        self.refuse("check for updates")
    }

    async fn resolve(&self, _packages: &[String]) -> Result<Resolution> {
        self.refuse("work out whether these updates can be installed together")
    }

    async fn apply(&self, _plan: &ApplyPlan) -> Result<ApplyReport> {
        self.refuse("install updates")
    }

    /// The one call that answers rather than refusing, and it does so from
    /// `uname` alone — the running kernel is a fact about this node that no
    /// package manager is needed to establish. Reporting it means the Updates
    /// page still tells an operator whether a restart is outstanding on a node
    /// whose package manager is broken, which is a node likely to have had one
    /// half-finished.
    async fn kernel(&self) -> Result<KernelState> {
        Ok(KernelState {
            running: running_release(),
            newest: None,
        })
    }
}

/// `uname -r`, without running it: the kernel release is in `/proc`, which the
/// daemon's sandbox leaves readable.
pub(crate) fn running_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_call_says_what_could_not_be_done_and_why() {
        let node = UnavailableUpdates::new("dnf not found");
        let err = node.check().await.unwrap_err();
        assert!(err.to_string().contains("check for updates"), "{err}");
        assert!(err.to_string().contains("dnf not found"), "{err}");

        assert!(node.resolve(&["kernel".into()]).await.is_err());
        assert!(node.apply(&ApplyPlan::ordinary()).await.is_err());

        // Reading the running kernel is the exception: it is a fact about the
        // node, not about the package manager.
        let kernel = node.kernel().await.unwrap();
        assert_eq!(kernel.newest, None);
        assert!(!kernel.stale(), "nothing known cannot imply a stale kernel");
    }
}
