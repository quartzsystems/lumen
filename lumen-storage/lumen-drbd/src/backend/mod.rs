//! The seam between the replicated-storage domain and the machinery that
//! answers for it.
//!
//! Three implementations, always exactly three: the supported command line
//! (`cli`), an in-memory simulation (`mock`), and a stand-in that explains
//! itself (`unavailable`). The mock is `pub` and compiled unconditionally —
//! the control plane's integration tests build on it, and a `cfg(test)` item
//! would be invisible to them.
//!
//! The backend answers only for **this node's DRBD**: the resources it
//! participates in, and the local operations a workflow delegates. The
//! backing zvols are deliberately not here — they belong to `lumen-zfs`, and
//! the service composes the two rather than teaching DRBD about datasets.
//!
//! Reads are unprivileged in shape (`drbdsetup status --json` over netlink,
//! run directly like `crm_mon`); every write is a `drbdadm` or file
//! operation and goes through `lumen_sys::exec` as a transient unit, exactly
//! as pcs and zpool do.

pub mod cli;
pub mod mock;
pub mod unavailable;

use async_trait::async_trait;

use crate::error::Result;
use crate::state::ResourceStatus;

#[async_trait]
pub trait DrbdBackend: Send + Sync {
    /// Every resource this node participates in, as DRBD reports it. A node
    /// with no replicated volumes answers with an empty list.
    async fn status(&self) -> Result<Vec<ResourceStatus>>;

    /// Write `/etc/drbd.d/<resource>.res` — privileged, because the sandbox
    /// keeps `/etc` read-only, and over stdin, because the file carries the
    /// replication shared-secret.
    async fn write_resource(&self, resource: &str, content: &str) -> Result<()>;

    /// Remove the resource file, returning the node to what a fresh install
    /// has.
    async fn remove_resource_file(&self, resource: &str) -> Result<()>;

    /// `drbdadm create-md` on the backing device, sized for `peers` peer
    /// slots.
    async fn create_metadata(&self, resource: &str, peers: usize) -> Result<()>;

    /// `drbdadm up` — attach the backing device and start connecting.
    async fn up(&self, resource: &str) -> Result<()>;

    /// `drbdadm down` — the teardown half.
    async fn down(&self, resource: &str) -> Result<()>;

    /// `drbdadm new-current-uuid --clear-bitmap` on the fresh resource: both
    /// backing zvols read as zeros, so there is nothing to copy and the
    /// initial full sync is skipped. Run once, on one member, after every
    /// member is up.
    async fn skip_initial_sync(&self, resource: &str) -> Result<()>;

    /// `drbdadm resize` — after every member's backing device has grown,
    /// run once anywhere to let the resource take the new size.
    async fn resize(&self, resource: &str) -> Result<()>;
}
