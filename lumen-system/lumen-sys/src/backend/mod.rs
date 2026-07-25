//! What the system domain needs from the node, and nothing more.
//!
//! Three implementations, exactly as `lumen_net::backend`: [`logind`] drives
//! the real node, [`mock`] is an in-memory model used by every test in this
//! crate and every control-plane API test, and [`unavailable`] reports why
//! there is nothing to drive rather than panicking.
//!
//! Like the others, the mock is compiled unconditionally and exported rather
//! than hidden behind `#[cfg(test)]`: a `cfg(test)` item is invisible to
//! another crate's integration tests, and the control plane's need this one.
//!
//! ## Why only power is behind the trait
//!
//! Accounts are not. Reading them is [`crate::state`] over a path that a test
//! simply points somewhere else, and writing them is [`crate::exec`], which is
//! already a trait with a mock of its own. Putting a second seam in front of
//! those would be a layer that only ever forwards.
//!
//! Power is different: there is no file to point elsewhere and no command to
//! record. Restarting the node is a method call on logind, and a test that ran
//! the real one would restart the machine running it.

pub mod logind;
pub mod mock;
pub mod unavailable;

use async_trait::async_trait;

use crate::error::Result;
use crate::model::PowerAction;

/// A restart or shutdown the node is already committed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledPower {
    pub action: PowerAction,
    /// Seconds since the epoch.
    pub at: u64,
}

#[async_trait]
pub trait PowerBackend: Send + Sync {
    /// Do it now.
    async fn power(&self, action: PowerAction) -> Result<()>;

    /// Do it at a moment in the future, seconds since the epoch.
    ///
    /// This is logind's own scheduling rather than a timer of Lumen's: it
    /// survives the control plane restarting, it is what `shutdown +30` sets,
    /// and it is what every signed-in user is warned about on their terminal.
    /// A schedule Lumen kept itself would do none of those things.
    async fn schedule(&self, action: PowerAction, at: u64) -> Result<()>;

    /// Call it off. `false` when there was nothing scheduled.
    async fn cancel(&self) -> Result<bool>;

    /// What is scheduled, if anything.
    async fn scheduled(&self) -> Result<Option<ScheduledPower>>;
}
