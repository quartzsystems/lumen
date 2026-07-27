//! The backend for a node whose DRBD cannot be asked — the kernel module
//! missing, the utilities absent. The console still renders, and every
//! replicated-storage request explains itself instead of failing blankly.

use async_trait::async_trait;

use super::DrbdBackend;
use crate::error::{DrbdError, Result};
use crate::state::ResourceStatus;

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
        Err(DrbdError::Conflict(format!(
            "Replicated storage is unavailable on this node: {}",
            self.reason
        )))
    }
}

#[async_trait]
impl DrbdBackend for UnavailableBackend {
    async fn status(&self) -> Result<Vec<ResourceStatus>> {
        self.error()
    }

    async fn write_resource(&self, _resource: &str, _content: &str) -> Result<()> {
        self.error()
    }

    async fn remove_resource_file(&self, _resource: &str) -> Result<()> {
        self.error()
    }

    async fn create_metadata(&self, _resource: &str, _peers: usize) -> Result<()> {
        self.error()
    }

    async fn up(&self, _resource: &str) -> Result<()> {
        self.error()
    }

    async fn down(&self, _resource: &str) -> Result<()> {
        self.error()
    }

    async fn skip_initial_sync(&self, _resource: &str) -> Result<()> {
        self.error()
    }

    async fn resize(&self, _resource: &str) -> Result<()> {
        self.error()
    }

    async fn set_two_primaries(&self, _resource: &str, _allow: bool) -> Result<()> {
        self.error()
    }

    async fn invalidate_remote(&self, _resource: &str) -> Result<()> {
        self.error()
    }

    async fn reconnect(&self, _resource: &str, _discard: bool) -> Result<()> {
        self.error()
    }

    async fn read_resource(&self, _resource: &str) -> Result<String> {
        self.error()
    }

    async fn adjust(&self, _resource: &str) -> Result<()> {
        self.error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_call_explains_itself() {
        let backend = UnavailableBackend::new("the drbd kernel module is not loaded");
        let err = backend.status().await.unwrap_err();
        assert!(err.to_string().contains("unavailable"));
        assert!(err.to_string().contains("kernel module"));
        assert!(backend.up("x").await.is_err());
    }
}
