//! Lumen clustering: the environment, its clusters, and the machinery that
//! keeps a cluster honest — quorum, membership, and fencing.
//!
//! Two tiers, two mechanisms. The **environment** is one administrative trust
//! domain over every node — membership, a CA, a shared session secret, one
//! federated console — held by Lumen itself with no corosync anywhere. A
//! **cluster** is where corosync and Pacemaker exist: an independent 2–5 node
//! quorum/fencing/replication domain inside the environment. A node belongs
//! to exactly one environment and at most one cluster; an environment node in
//! no cluster is a valid standalone hypervisor, not an error.
//!
//! ```text
//!   model.rs        what a cluster is: name, members, preferred node, BMCs
//!   environment.rs  the membership record and its reconciliation rule
//!   topology.rs     one renderer, two regimes: corosync.conf and fencing
//!   networks.rs     typed cluster networks — Core, Management, External
//!   state.rs        what corosync and Pacemaker actually report
//!   validate.rs     pure rules over a definition and the environment
//!   backend/        the supported command line (cli/), plus mock/ and unavailable/
//!   service.rs      the one entry point the control plane calls
//! ```
//!
//! `lumen-controlplane` depends on this crate and contributes only HTTP: its
//! handlers deserialize, call one [`service::ClusterService`] method, and
//! serialize the answer.
//!
//! ## Division of labor with Pacemaker
//!
//! corosync and Pacemaker own membership, quorum, and fencing — the things
//! that must be correct and already are, there. Lumen owns everything the
//! operator sees: state is read from `crm_mon --output-as=xml`,
//! `corosync-quorumtool`, and `corosync-cfgtool`, and presented through this
//! crate's own model. Pacemaker never manages a virtual machine's lifecycle;
//! libvirt stays the source of truth for machines, and Lumen's HA manager
//! acts only after Pacemaker confirms a dead node was fenced. See
//! docs/cluster.md.

pub mod backend;
pub mod environment;
pub mod error;
pub mod join;
pub mod model;
pub mod networks;
pub mod service;
pub mod state;
pub mod store;
pub mod topology;
pub mod validate;

pub use environment::{
    ClusterRecord, EnvironmentMembership, EnvironmentNode, JoinToken, Maintenance,
};
pub use error::{ClusterError, Result};
pub use join::{
    CreateProgress, JoinGrant, JoinRequest, MockPeers, PeerChannel, PreflightReport, PreflightView,
    PreparePayload, ProgressHandle, TeardownPayload,
};
pub use model::{
    valid_cluster_name, valid_node_name, BmcConfig, ClusterDefinition, MemberNode, Regime,
    COROSYNC_PORT, FENCE_RACE_DELAY_SECS, MAX_CLUSTER_NODES, MIN_CLUSTER_NODES,
};
pub use networks::{
    valid_bridge_name, AddressedMember, ClusterNetworks, CoreNetwork, ExternalNetwork,
    ManagementNetwork, Subnet, Uplink, VlanMode,
};
pub use service::{
    quorum_survives_loss, AddNodePlan, ClusterHealth, ClusterNodeView, ClusterService, ClusterView,
    EnvironmentResponse, EnvironmentView, FenceSummaryView, FenceTestView, JoinOutcome,
    MaintenanceView, MintedToken, UnassignedNodeView, VipView,
};
pub use state::{
    ClusterState, FenceDeviceState, FenceTest, NodeState, QuorumState, RingLink, VipState,
};
pub use store::{EnvironmentStore, Identity, StoredDefinition};
pub use topology::{ClusterTopology, FenceDevice};
pub use validate::{
    Acknowledgements, ClusterCreate, MemberCreate, ValidationCode, ValidationError,
};
