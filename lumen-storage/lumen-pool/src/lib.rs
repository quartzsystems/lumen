//! lumen-pool: the orchestration domain over LumenFS.
//!
//! docs/lumenfs.md's compute seam, implemented a second time — the same
//! `VmVolumes` the DRBD path implements, over vdisks and writer leases
//! instead of resources and roles. `VirtService` holds the seam as
//! `Arc<dyn VmVolumes>`, so this crate slots in underneath without one
//! change to the compute domain.
//!
//! ```text
//!   config.rs   whether this node carries a pool, read from the daemon's
//!               own drop-in rather than remembered a second time
//!   model.rs    names, ids, device paths — and why no vdisk record exists
//!   fleet.rs    the pool's members as something callable, plus a mock
//!   socket.rs   control connections by address — loopback-reachable only
//!   peers.rs    the fleet for real: this node's daemon over loopback, and
//!               every other member through its own control plane
//!   service.rs  the five verbs, as naming and fan-out
//!   state.rs    observed pool state: the shapes a console page renders
//!   error.rs    what goes wrong, and how the compute domain reads it
//! ```
//!
//! Nothing here can corrupt data: the engine (`lumen-fs`) owns every byte
//! and the daemon (`lumen-fsd`) owns the sockets. This crate holds no state
//! at all — it derives what it needs from the name it was given and asks
//! the daemon for the rest, which is why there is no record to fall out of
//! step with the pool.
//!
//! **What is not here yet**: the controlplane routes that serve
//! [`PoolService::state`] and the console pages that render it, and the
//! pool-creation workflow — nothing yet writes `/etc/lumen/fsd.conf`, so a
//! pool is still brought up by hand.

pub mod config;
pub mod error;
pub mod fleet;
pub mod model;
pub mod peers;
pub mod service;
pub mod socket;
pub mod state;

pub use config::PoolConfig;
pub use error::{PoolError, Result};
pub use fleet::{MockFleet, PoolFleet};
pub use model::{device_path, vdisk_of_device, DiskName, DISKS_PER_MACHINE};
pub use peers::{execute, PeeredFleet, PoolAnswer, PoolPeers, PoolVerb};
pub use service::PoolService;
pub use socket::SocketFleet;
pub use state::{
    LeaseSeen, MemberStatus, MemberView, PoolHealth, PoolMember, PoolState, Replication,
    SnapshotView, VdiskView,
};
