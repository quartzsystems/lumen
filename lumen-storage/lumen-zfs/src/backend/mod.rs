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
use crate::model::{BlockDevice, Dataset, Pool, PoolRequest, VolumeRequest};

#[async_trait]
pub trait ZfsBackend: Send + Sync {
    /// Every pool imported on this node.
    async fn pools(&self) -> Result<Vec<Pool>>;

    /// Every disk the node has, and what is already on each one.
    ///
    /// Here rather than in the service because the mock has to be able to
    /// answer it: a test for "the console refuses to build a pool on the disk
    /// the appliance boots from" needs a node with that disk on it.
    async fn block_devices(&self) -> Result<Vec<BlockDevice>>;

    /// Every device path an imported pool is currently built on, and the
    /// pool that has it.
    ///
    /// Exists because the disk scan cannot answer this. A pool member is a
    /// disk with a couple of partitions and nothing in `/proc/mounts`, which
    /// is indistinguishable from a disk somebody finished with — and the
    /// difference is whether clearing it destroys a running pool. `zpool` is
    /// the only thing that knows, so it is asked.
    ///
    /// Paths are as `zpool` reports them, which is whatever the pool was
    /// built on: a by-id path, a partition of one, or a bare kernel name.
    /// Matching is the caller's problem precisely because the shapes vary.
    async fn pool_members(&self) -> Result<Vec<(String, String)>>;

    /// Clear a disk: remove every filesystem and pool signature on it and the
    /// partition table with them, then re-read it.
    ///
    /// Destructive and not undoable, so every guard is the service's and is
    /// applied before this is reached. What this does *not* do is erase the
    /// data — the blocks are still there, the labels that named them are not.
    /// That is what makes the disk selectable again, which is the whole
    /// purpose; a console that promised more than it does would be lying
    /// about a disk somebody is about to hand back.
    /// The whole device rather than a path: clearing a disk means clearing
    /// its partitions too, and the kernel names of those are derived from the
    /// disk's own — which the by-id path the rest of this trait speaks in
    /// cannot give back.
    async fn wipe_disk(&self, device: &BlockDevice) -> Result<()>;

    /// Create a pool.
    ///
    /// The one operation in this trait that cannot happen inside the control
    /// plane's sandbox: it writes `/etc/zfs/zpool.cache`, which
    /// `ProtectSystem=strict` makes read-only. The real backend hands it to
    /// systemd; see `lumen_sys::exec` for why that is the arrangement rather
    /// than a relaxed unit.
    async fn create_pool(&self, request: &PoolRequest) -> Result<Pool>;

    /// Destroy one. Everything on it goes.
    async fn destroy_pool(&self, name: &str) -> Result<()>;

    /// Filesystems and volumes under one pool, the pool's own root included.
    async fn datasets(&self, pool: &str) -> Result<Vec<Dataset>>;

    /// Create a volume. The caller has already checked the path is inside the
    /// Lumen namespace; implementations check again, because a backend that
    /// trusts its caller is one refactor away from not being safe.
    async fn create_volume(&self, request: &VolumeRequest) -> Result<Dataset>;

    /// Remove a volume. Refuses anything that is not a Lumen volume.
    async fn destroy_volume(&self, path: &str) -> Result<()>;

    /// Grow a volume to a new size. Grow only — `zfs set volsize` can shrink,
    /// but a shrunk disk under a running guest is data loss with extra steps,
    /// so the refusal lives at every layer.
    async fn resize_volume(&self, path: &str, size: u64) -> Result<()>;

    /// Snapshot a volume: `zfs snapshot <path>@<name>`. Crash-consistent —
    /// the state a machine would find after a power cut at that instant.
    async fn snapshot_volume(&self, path: &str, snapshot: &str) -> Result<()>;

    /// Roll a volume back to a snapshot, discarding everything after it —
    /// later snapshots included (`zfs rollback -r`). The acknowledgement
    /// lives with the caller; this is the mechanism.
    async fn rollback_volume(&self, path: &str, snapshot: &str) -> Result<()>;

    /// Remove one snapshot.
    async fn destroy_snapshot(&self, path: &str, snapshot: &str) -> Result<()>;

    /// The snapshots of one volume, oldest first.
    async fn snapshots(&self, path: &str) -> Result<Vec<crate::model::SnapshotInfo>>;

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
