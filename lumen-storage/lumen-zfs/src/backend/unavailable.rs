//! A backend that reports why there is no storage.
//!
//! Same reasoning as `lumen_net::backend::unavailable`: if the storage
//! software is not installed, or the tools cannot be run, the control plane
//! must still come up. An operator whose storage is broken needs the console
//! more than usual, and "the console will not start" is a far worse failure
//! than "Storage is unavailable on this node: <reason>".

use async_trait::async_trait;

use crate::backend::ZfsBackend;
use crate::error::{Result, ZfsError};
use crate::model::{Dataset, Pool, VolumeRequest};

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
        Err(ZfsError::Conflict(format!(
            "Storage is unavailable on this node: {}",
            self.reason
        )))
    }
}

#[async_trait]
impl ZfsBackend for UnavailableBackend {
    async fn pools(&self) -> Result<Vec<Pool>> {
        self.error()
    }
    async fn datasets(&self, _pool: &str) -> Result<Vec<Dataset>> {
        self.error()
    }
    async fn create_volume(&self, _request: &VolumeRequest) -> Result<Dataset> {
        self.error()
    }
    async fn destroy_volume(&self, _path: &str) -> Result<()> {
        self.error()
    }
    async fn ensure_namespace(&self, _pool: &str) -> Result<()> {
        self.error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_call_explains_itself() {
        let backend = UnavailableBackend::new("the storage tools are not installed");
        let err = backend.pools().await.unwrap_err();
        assert!(
            err.to_string()
                .contains("the storage tools are not installed"),
            "{err}"
        );
    }
}
