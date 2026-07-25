//! What the storage domain needs from the box, and nothing more.
//!
//! Three implementations, exactly as `lumen_net::backend`: [`cli`] runs the
//! real tools, [`mock`] is an in-memory model used by every test in this crate
//! and every control-plane API test, and [`unavailable`] reports why there is
//! no storage rather than panicking.
//!
//! Like `lumen_net`'s, the mock is compiled unconditionally and exported
//! rather than hidden behind `#[cfg(test)]`: a `cfg(test)` item is invisible to
//! another crate's integration tests, and the control plane's tests need it.

pub mod cli;
pub mod mock;
pub mod unavailable;

use async_trait::async_trait;

use crate::error::Result;
use crate::model::{Dataset, Pool, VolumeRequest};

#[async_trait]
pub trait ZfsBackend: Send + Sync {
    /// Every pool imported on this node.
    async fn pools(&self) -> Result<Vec<Pool>>;

    /// Filesystems and volumes under one pool, the pool's own root included.
    async fn datasets(&self, pool: &str) -> Result<Vec<Dataset>>;

    /// Create a volume. The caller has already checked the path is inside the
    /// Lumen namespace; implementations check again, because a backend that
    /// trusts its caller is one refactor away from not being safe.
    async fn create_volume(&self, request: &VolumeRequest) -> Result<Dataset>;

    /// Remove a volume. Refuses anything that is not a Lumen volume.
    async fn destroy_volume(&self, path: &str) -> Result<()>;

    /// Create the `<pool>/lumen` parent if it is not there yet. Idempotent.
    async fn ensure_namespace(&self, pool: &str) -> Result<()>;

    /// Create the pool's media library — `<pool>/lumen/iso`, mounted at
    /// [`crate::model::iso_mountpoint`] — if it is not there yet. Idempotent,
    /// and returns the mount point either way.
    ///
    /// Unlike every other write here this one produces a *mount*, and a mount
    /// made while the control plane is running does not necessarily appear
    /// inside its namespace. The service checks whether it can actually see
    /// the directory afterwards rather than assuming; see
    /// [`crate::iso::IsoLibrary`].
    async fn ensure_iso_store(&self, pool: &str) -> Result<String>;
}
