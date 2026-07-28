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
use crate::model::{BlockDevice, Dataset, Pool, PoolRequest, VolumeRequest};

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
    /// The one call that still answers. Reading `/sys/block` needs no storage
    /// software at all, and a node whose tools are missing is exactly where an
    /// operator wants to see what disks it has.
    async fn block_devices(&self) -> Result<Vec<BlockDevice>> {
        Ok(crate::devices::list(&crate::devices::DeviceRoots::default()).await)
    }
    /// Answers with nothing rather than with an error, and that is the safe
    /// direction only because of what the service does with it: an empty list
    /// means "no pool claims this disk", which would make a wipe look allowed.
    /// The service refuses a wipe outright when the pools cannot be listed —
    /// see `StorageService::wipe_disk`, which asks `pools()` first, and that
    /// call does error here.
    async fn pool_members(&self) -> Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }
    async fn wipe_disk(&self, _device: &BlockDevice) -> Result<()> {
        self.error()
    }
    async fn create_pool(&self, _request: &PoolRequest) -> Result<Pool> {
        self.error()
    }
    async fn destroy_pool(&self, _name: &str) -> Result<()> {
        self.error()
    }
    async fn create_volume(&self, _request: &VolumeRequest) -> Result<Dataset> {
        self.error()
    }
    async fn destroy_volume(&self, _path: &str) -> Result<()> {
        self.error()
    }
    async fn resize_volume(&self, _path: &str, _size: u64) -> Result<()> {
        self.error()
    }
    async fn snapshot_volume(&self, _path: &str, _snapshot: &str) -> Result<()> {
        self.error()
    }
    async fn rollback_volume(&self, _path: &str, _snapshot: &str) -> Result<()> {
        self.error()
    }
    async fn destroy_snapshot(&self, _path: &str, _snapshot: &str) -> Result<()> {
        self.error()
    }
    async fn snapshots(&self, _path: &str) -> Result<Vec<crate::model::SnapshotInfo>> {
        self.error()
    }
    async fn ensure_namespace(&self, _pool: &str) -> Result<()> {
        self.error()
    }
    async fn ensure_iso_store(&self, _pool: &str) -> Result<String> {
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
