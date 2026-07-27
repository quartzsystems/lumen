//! The seam between the clustering domain and the machinery that answers for
//! it.
//!
//! Three implementations, and always exactly three: the supported command
//! line (`cli`), an in-memory simulation (`mock`), and a stand-in that
//! explains itself (`unavailable`). The mock is `pub` and compiled
//! unconditionally, never `#[cfg(test)]`: a `cfg(test)` item is invisible to
//! another crate's integration tests, and the control plane's tests need it.
//!
//! Reads are unprivileged — `crm_mon`, `corosync-quorumtool`, and
//! `corosync-cfgtool` answer any local user — so nothing in this trait
//! touches `lumen_sys::exec` yet. The privileged verbs (`pcs cluster setup`,
//! fence tests) arrive with the join and fencing workflows and will go
//! through a transient unit exactly as `zpool create` does.

pub mod cli;
pub mod mock;
pub mod unavailable;

use async_trait::async_trait;

use crate::environment::EnvironmentMembership;
use crate::error::Result;
use crate::state::ClusterState;

#[async_trait]
pub trait ClusterBackend: Send + Sync {
    /// The environment membership record this node holds, or `None` for a
    /// node that never joined one — the ordinary standalone appliance, not
    /// an error.
    async fn membership(&self) -> Result<Option<EnvironmentMembership>>;

    /// Observed state of one cluster by name. The command-line backend can
    /// only answer for the cluster this node is a member of; the mock
    /// answers for every cluster it simulates, which is what lets the
    /// environment view be tested whole.
    async fn cluster_state(&self, name: &str) -> Result<ClusterState>;
}
