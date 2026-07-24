//! What the networking domain needs from the box, and nothing more.
//!
//! Two implementations: [`nm`] drives NetworkManager over the system bus,
//! [`mock`] is an in-memory model used by every test in this crate and every
//! control-plane API test. CI never touches the runner's NetworkManager.
//!
//! This mirrors `lumen_controlplane::realm::Realm` and its mock in
//! `tests/auth_flow.rs`: a narrow async trait, a real implementation
//! constructed only in `main`, and a deterministic stand-in injected by tests.
//! Unlike that mock, this one is compiled unconditionally and exported — a
//! `#[cfg(test)]` item is invisible to another crate's integration tests, and
//! the control plane's tests need it.

pub mod mock;
pub mod nm;
pub mod unavailable;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::plan::Op;
use crate::state::ObservedState;

/// Handle to an outstanding NetworkManager checkpoint. Opaque to callers: on
/// the real backend it is a D-Bus object path, on the mock a counter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointId(pub String);

impl std::fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[async_trait]
pub trait NetworkBackend: Send + Sync {
    /// What is on the box right now.
    async fn observe(&self) -> Result<ObservedState>;

    /// Run the plan in order, stopping at the first failure. The caller is
    /// expected to hold a checkpoint, so a partial apply is recoverable.
    async fn apply(&self, ops: &[Op]) -> Result<()>;

    /// Take a rollback point covering every device. `rollback_secs` is the
    /// deadline after which the backend restores it on its own.
    async fn checkpoint_create(&self, rollback_secs: u32) -> Result<CheckpointId>;

    /// Discard the rollback point — the change becomes permanent.
    async fn checkpoint_destroy(&self, id: &CheckpointId) -> Result<()>;

    /// Restore the rollback point now.
    async fn checkpoint_rollback(&self, id: &CheckpointId) -> Result<()>;

    /// Push the automatic-rollback deadline out by `add_secs`.
    async fn checkpoint_extend(&self, id: &CheckpointId, add_secs: u32) -> Result<()>;

    /// Checkpoints the backend knows about — used at daemon start to adopt or
    /// clear one left behind by a crash mid-apply.
    async fn checkpoints(&self) -> Result<Vec<CheckpointId>>;
}
