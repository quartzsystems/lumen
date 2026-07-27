//! The seam between the clustering domain and the machinery that answers for
//! it.
//!
//! Three implementations, and always exactly three: the supported command
//! line (`cli`), an in-memory simulation (`mock`), and a stand-in that
//! explains itself (`unavailable`). The mock is `pub` and compiled
//! unconditionally, never `#[cfg(test)]`: a `cfg(test)` item is invisible to
//! another crate's integration tests, and the control plane's tests need it.
//!
//! The backend answers only for **this node and its cluster stack** —
//! observed state, and the local operations a workflow delegates. The
//! environment's own state (membership, CA, tokens) is the service's, held
//! in [`crate::store`]; it is Lumen state, not something corosync can be
//! asked about.
//!
//! Reads are unprivileged — `crm_mon`, `corosync-quorumtool`, and
//! `corosync-cfgtool` answer root without leaving the sandbox. The writes
//! are privileged and go through `lumen_sys::exec` as transient units,
//! exactly as `zpool create` does.

pub mod cli;
pub mod mock;
pub mod unavailable;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::state::ClusterState;

/// What this node can say about its own readiness to join a cluster. The
/// service adds what it already knows (name, version) and the control plane
/// adds the links; this is only the part that must be asked of the node.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LocalPreflight {
    /// chrony reports the clock synchronized.
    pub time_synchronized: bool,
    /// Offset from chrony's reference, milliseconds.
    pub time_offset_ms: Option<i64>,
    /// A corosync configuration is already on the node.
    pub already_clustered: bool,
}

#[async_trait]
pub trait ClusterBackend: Send + Sync {
    /// Observed state of one cluster by name. The command-line backend can
    /// only answer for the cluster this node is a member of; the mock
    /// answers for every cluster it simulates, which is what lets the
    /// environment view be tested whole.
    async fn cluster_state(&self, name: &str) -> Result<ClusterState>;

    /// The node's own preflight answers: clock health and whether a cluster
    /// configuration already exists here.
    async fn local_preflight(&self) -> Result<LocalPreflight>;

    /// Write `/etc/corosync/corosync.conf` and the authentication key —
    /// privileged, because the sandbox keeps `/etc` read-only.
    async fn write_cluster_config(&self, conf: &str, authkey: &str) -> Result<()>;

    /// `systemctl enable --now corosync pacemaker`. Enabled here and only
    /// here: the packages ship presets that keep both off, because a node
    /// that is not in a cluster must not start half of one at boot.
    async fn enable_stack(&self) -> Result<()>;

    /// `systemctl disable --now pacemaker corosync` — the unwind's half.
    async fn disable_stack(&self) -> Result<()>;

    /// Remove the corosync configuration and key, returning the node to
    /// exactly what a fresh install has.
    async fn remove_cluster_config(&self) -> Result<()>;

    /// Set cluster-wide Pacemaker properties, `stonith-enabled=true` among
    /// them. Local pcs: the CIB is cluster-wide from any member.
    async fn set_pacemaker_properties(&self, properties: &[(String, String)]) -> Result<()>;

    /// Create the Management VIP resource — a floating address that follows
    /// the cluster's survivors.
    async fn create_vip(
        &self,
        cluster: &str,
        address: std::net::Ipv4Addr,
        prefix: u8,
    ) -> Result<()>;

    /// Create one `fence_ipmilan` STONITH device. The password travels
    /// separately from the rendered device because it must never enter the
    /// membership record, a journal line, or an argument vector — the CLI
    /// backend pipes it inside CIB XML over a transient unit's stdin, and
    /// after that it lives only where Pacemaker keeps it.
    async fn create_fence_device(
        &self,
        device: &crate::topology::FenceDevice,
        password: &str,
    ) -> Result<()>;

    /// Fence a node for real — the guarded live test's teeth. Pacemaker
    /// routes the action to a member that can run the device, the target
    /// power-cycles, and the call answers only when the fence operation has
    /// a result.
    async fn fence_node(&self, target: &str) -> Result<()>;

    /// Break-glass: tell Pacemaker an unfenced-unreachable node is verified
    /// down, so recovery proceeds without a successful fence. The guards —
    /// only an unclean node, never this node, an explicit acknowledgement —
    /// are the service's; this is just the confirmation.
    async fn confirm_node_dead(&self, target: &str) -> Result<()>;
}
