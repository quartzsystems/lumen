//! lumen-pool: the orchestration domain over LumenFS.
//!
//! docs/lumenfs.md's compute seam, implemented a second time — the same
//! `VmVolumes` the DRBD path implements, over vdisks and writer leases
//! instead of resources and roles. `VirtService` holds the seam as
//! `Arc<dyn VmVolumes>`, so this crate slots in underneath without one
//! change to the compute domain.
//!
//! ```text
//!   model.rs    names, ids, device paths — and why no vdisk record exists
//!   fleet.rs    the pool's members as something callable, plus a mock
//!   socket.rs   the fleet for real: a control connection per member
//!   service.rs  the five verbs, as naming and fan-out
//!   error.rs    what goes wrong, and how the compute domain reads it
//! ```
//!
//! Nothing here can corrupt data: the engine (`lumen-fs`) owns every byte
//! and the daemon (`lumen-fsd`) owns the sockets. This crate holds no state
//! at all — it derives what it needs from the name it was given and asks
//! the daemon for the rest, which is why there is no record to fall out of
//! step with the pool.
//!
//! **What is not here yet**: the console's render and state modules, and
//! the HA materialization step described on
//! `PoolService::migration_window` — the one thing that keeps a pooled disk
//! from failing over.

pub mod error;
pub mod fleet;
pub mod model;
pub mod service;
pub mod socket;

pub use error::{PoolError, Result};
pub use fleet::{MockFleet, PoolFleet};
pub use model::{device_path, vdisk_of_device, DiskName, DISKS_PER_MACHINE};
pub use service::PoolService;
pub use socket::SocketFleet;
