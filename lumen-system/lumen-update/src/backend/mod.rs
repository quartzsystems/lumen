//! What the update domain needs from the node, and nothing more.
//!
//! Three implementations, exactly as `lumen_sys::backend`: [`dnf`] drives the
//! real node's package manager, [`mock`] is an in-memory model used by every
//! test in this crate and every control-plane API test, and [`unavailable`]
//! reports why there is nothing to drive rather than panicking.
//!
//! Like the others, the mock is compiled unconditionally and exported rather
//! than hidden behind `#[cfg(test)]`: a `cfg(test)` item is invisible to
//! another crate's integration tests, and the control plane's need this one.
//!
//! ## Why the reads are behind the trait too
//!
//! In `lumen_sys` only power is, because accounts are a file a test can point
//! elsewhere. Nothing here is a file. Listing updates refreshes repository
//! metadata over the network and writes a cache; resolving a transaction asks
//! a solver. A test that ran either would depend on what AlmaLinux published
//! this morning, which is the definition of a test that fails for reasons
//! nobody changed.

pub mod dnf;
pub mod mock;
pub mod unavailable;

use async_trait::async_trait;

use crate::error::Result;
use crate::model::{ApplyPlan, ApplyReport, KernelState, Resolution, Update};

#[async_trait]
pub trait UpdateBackend: Send + Sync {
    /// Refresh repository metadata and list every package with a newer build
    /// waiting.
    ///
    /// Advisory fields are filled in when the repositories publish advisory
    /// metadata and it can be read. An [`Update`] with no advisory means
    /// *nothing said there was one* — not that the package carries no security
    /// fix — and the console is worded accordingly.
    async fn check(&self) -> Result<Vec<Update>>;

    /// Ask whether a set of packages can be upgraded as one transaction,
    /// without doing it.
    ///
    /// This is the platform gate. It exists because a kernel whose kABI-tracking
    /// modules have not caught up resolves to nothing, and finding that out
    /// after the reboot means a drive to the rack.
    async fn resolve(&self, packages: &[String]) -> Result<Resolution>;

    /// Do it.
    async fn apply(&self, plan: &ApplyPlan) -> Result<ApplyReport>;

    /// The running kernel against the newest installed one — what
    /// [`crate::model::RebootState`] is computed from.
    async fn kernel(&self) -> Result<KernelState>;
}
