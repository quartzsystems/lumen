//! An in-memory node, for tests that must not restart the machine running
//! them.
//!
//! Records what it was asked to do rather than doing it, so a test can assert
//! on the node's state as well as on what the API said — the same shape as
//! `lumen_virt::backend::mock`.

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::backend::{PowerBackend, ScheduledPower};
use crate::error::{Result, SysError};
use crate::model::PowerAction;

#[derive(Default)]
pub struct MockPower {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Every immediate restart or shutdown asked for, in order. A test asserts
    /// on this instead of on the machine having gone away.
    performed: Vec<PowerAction>,
    scheduled: Option<ScheduledPower>,
    /// Refuse everything with this reason, for the "the node said no" path.
    refuse: Option<String>,
}

impl MockPower {
    pub fn new() -> Self {
        Self::default()
    }

    /// A node that does what it is told, which is what most tests want.
    pub fn appliance() -> Self {
        Self::new()
    }

    /// Make every subsequent call fail, the way a node with a policy that
    /// forbids it would.
    pub async fn refuse(&self, reason: &str) {
        self.state.lock().await.refuse = Some(reason.to_string());
    }

    /// Everything that would have happened to the node, in order.
    pub async fn performed(&self) -> Vec<PowerAction> {
        self.state.lock().await.performed.clone()
    }

    /// Pretend somebody ran `shutdown -r +30` at the keyboard.
    pub async fn preset_schedule(&self, action: PowerAction, at: u64) {
        self.state.lock().await.scheduled = Some(ScheduledPower { action, at });
    }

    async fn allowed(&self) -> Result<()> {
        match self.state.lock().await.refuse.clone() {
            Some(reason) => Err(SysError::backend(anyhow::anyhow!("{reason}"))),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl PowerBackend for MockPower {
    async fn power(&self, action: PowerAction) -> Result<()> {
        self.allowed().await?;
        self.state.lock().await.performed.push(action);
        Ok(())
    }

    async fn schedule(&self, action: PowerAction, at: u64) -> Result<()> {
        self.allowed().await?;
        // logind holds exactly one schedule, and a second call replaces the
        // first rather than adding to it. A mock that queued them would let a
        // test pass against behaviour the node does not have.
        self.state.lock().await.scheduled = Some(ScheduledPower { action, at });
        Ok(())
    }

    async fn cancel(&self) -> Result<bool> {
        self.allowed().await?;
        Ok(self.state.lock().await.scheduled.take().is_some())
    }

    async fn scheduled(&self) -> Result<Option<ScheduledPower>> {
        self.allowed().await?;
        Ok(self.state.lock().await.scheduled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_second_schedule_replaces_the_first_the_way_logind_does() {
        let node = MockPower::appliance();
        node.schedule(PowerAction::Reboot, 1_000).await.unwrap();
        node.schedule(PowerAction::PowerOff, 2_000).await.unwrap();
        assert_eq!(
            node.scheduled().await.unwrap(),
            Some(ScheduledPower {
                action: PowerAction::PowerOff,
                at: 2_000
            })
        );
    }

    #[tokio::test]
    async fn cancelling_says_whether_there_was_anything_to_cancel() {
        let node = MockPower::appliance();
        assert!(!node.cancel().await.unwrap());
        node.schedule(PowerAction::Reboot, 1_000).await.unwrap();
        assert!(node.cancel().await.unwrap());
        assert_eq!(node.scheduled().await.unwrap(), None);
    }

    #[tokio::test]
    async fn nothing_actually_restarts_the_machine_running_the_tests() {
        let node = MockPower::appliance();
        node.power(PowerAction::Reboot).await.unwrap();
        assert_eq!(node.performed().await, vec![PowerAction::Reboot]);
    }
}
