//! The backend for a node whose cluster stack cannot be asked.
//!
//! Constructed by the control plane when the real backend cannot come up, so
//! the console still renders and every clustering request explains itself
//! instead of failing blankly — an operator whose cluster is broken needs the
//! console more than usual.

use async_trait::async_trait;

use super::{ClusterBackend, LocalPreflight};
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
    async fn cluster_state(&self, _name: &str) -> Result<ClusterState> {
        self.error()
    }

    async fn local_preflight(&self) -> Result<LocalPreflight> {
        self.error()
    }

    async fn write_cluster_config(&self, _conf: &str, _authkey: &str) -> Result<()> {
        self.error()
    }

    async fn enable_stack(&self) -> Result<()> {
        self.error()
    }

    async fn disable_stack(&self) -> Result<()> {
        self.error()
    }

    async fn remove_cluster_config(&self) -> Result<()> {
        self.error()
    }

    async fn set_pacemaker_properties(&self, _properties: &[(String, String)]) -> Result<()> {
        self.error()
    }

    async fn create_vip(
        &self,
        _cluster: &str,
        _address: std::net::Ipv4Addr,
        _prefix: u8,
    ) -> Result<()> {
        self.error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_call_explains_itself() {
        let backend = UnavailableBackend::new("the state directory is unreadable");
        let err = backend.cluster_state("alpha").await.unwrap_err();
        assert!(err.to_string().contains("unavailable"));
        assert!(err.to_string().contains("unreadable"));
        assert!(backend.local_preflight().await.is_err());
        assert!(backend.enable_stack().await.is_err());
    }
}
