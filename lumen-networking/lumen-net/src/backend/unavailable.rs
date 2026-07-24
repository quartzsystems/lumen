//! A backend that reports why there is no backend.
//!
//! If NetworkManager is not reachable at startup, the control plane must
//! still come up: an operator whose networking is broken needs the console
//! more than usual, and "the console will not start" is a far worse failure
//! than "Networking is unavailable: <reason>". Every call returns the reason,
//! so the console shows it instead of an empty table.

use async_trait::async_trait;

use crate::backend::{CheckpointId, NetworkBackend};
use crate::error::{NetError, Result};
use crate::plan::Op;
use crate::state::ObservedState;

pub struct UnavailableBackend {
    reason: String,
}

impl UnavailableBackend {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    fn error<T>(&self) -> Result<T> {
        Err(NetError::Conflict(format!(
            "Networking is unavailable on this node: {}",
            self.reason
        )))
    }
}

#[async_trait]
impl NetworkBackend for UnavailableBackend {
    async fn observe(&self) -> Result<ObservedState> {
        self.error()
    }
    async fn apply(&self, _ops: &[Op]) -> Result<()> {
        self.error()
    }
    async fn checkpoint_create(&self, _rollback_secs: u32) -> Result<CheckpointId> {
        self.error()
    }
    async fn checkpoint_destroy(&self, _id: &CheckpointId) -> Result<()> {
        self.error()
    }
    async fn checkpoint_rollback(&self, _id: &CheckpointId) -> Result<()> {
        self.error()
    }
    async fn checkpoint_extend(&self, _id: &CheckpointId, _add_secs: u32) -> Result<()> {
        self.error()
    }
    async fn checkpoints(&self) -> Result<Vec<CheckpointId>> {
        self.error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_call_explains_itself() {
        let backend = UnavailableBackend::new("could not connect to the system bus");
        let err = backend.observe().await.unwrap_err();
        assert!(
            err.to_string()
                .contains("could not connect to the system bus"),
            "{err}"
        );
    }
}
