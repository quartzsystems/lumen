//! The backend for a node whose cluster stack cannot be asked.
//!
//! Constructed by the control plane when the real backend cannot come up, so
//! the console still renders and every clustering request explains itself
//! instead of failing blankly — an operator whose cluster is broken needs the
//! console more than usual.

use async_trait::async_trait;

use super::ClusterBackend;
use crate::environment::EnvironmentMembership;
use crate::error::{ClusterError, Result};
use crate::state::ClusterState;

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
        Err(ClusterError::Conflict(format!(
            "Clustering is unavailable on this node: {}",
            self.reason
        )))
    }
}

#[async_trait]
impl ClusterBackend for UnavailableBackend {
    async fn membership(&self) -> Result<Option<EnvironmentMembership>> {
        self.error()
    }

    async fn cluster_state(&self, _name: &str) -> Result<ClusterState> {
        self.error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_call_explains_itself() {
        let backend = UnavailableBackend::new("the state directory is unreadable");
        let err = backend.membership().await.unwrap_err();
        assert!(err.to_string().contains("unavailable"));
        assert!(err.to_string().contains("unreadable"));
        assert!(backend.cluster_state("alpha").await.is_err());
    }
}
