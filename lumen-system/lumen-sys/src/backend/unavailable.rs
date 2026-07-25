//! There is no logind to ask, and every call says why.
//!
//! Swapped in when the system bus is unreachable at startup, exactly as
//! `lumen_net` and `lumen_virt` do. A control plane that comes up and explains
//! itself is more useful than one that refuses to start: an operator whose node
//! is misbehaving needs the console more than usual, and Maintenance is not the
//! only page on it.

use async_trait::async_trait;

use crate::backend::{PowerBackend, ScheduledPower};
use crate::error::{Result, SysError};
use crate::model::PowerAction;

pub struct UnavailablePower {
    reason: String,
}

impl UnavailablePower {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    fn refuse<T>(&self, what: &str) -> Result<T> {
        Err(SysError::backend(anyhow::anyhow!(
            "cannot {what}: this node's login manager is unreachable ({})",
            self.reason
        )))
    }
}

#[async_trait]
impl PowerBackend for UnavailablePower {
    async fn power(&self, action: PowerAction) -> Result<()> {
        self.refuse(action.as_sentence())
    }

    async fn schedule(&self, action: PowerAction, _at: u64) -> Result<()> {
        self.refuse(&format!("schedule a {}", action.as_sentence()))
    }

    async fn cancel(&self) -> Result<bool> {
        self.refuse("cancel a scheduled restart")
    }

    /// The one call that answers rather than refusing. "Nothing is scheduled"
    /// is the truth as far as this appliance can tell, and a Maintenance page
    /// that renders with its controls disabled is better than one that shows
    /// an error where the countdown goes.
    async fn scheduled(&self) -> Result<Option<ScheduledPower>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_action_says_what_could_not_be_done_and_why() {
        let node = UnavailablePower::new("no system bus");
        let err = node.power(PowerAction::Reboot).await.unwrap_err();
        assert!(err.to_string().contains("restart"), "{err}");
        assert!(err.to_string().contains("no system bus"), "{err}");

        assert!(node.schedule(PowerAction::PowerOff, 1).await.is_err());
        assert!(node.cancel().await.is_err());
        // Reading is the exception: nothing scheduled is a true answer.
        assert_eq!(node.scheduled().await.unwrap(), None);
    }
}
