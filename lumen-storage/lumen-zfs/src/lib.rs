//! Lumen storage: pools, datasets, and the volumes a virtual machine's disks
//! live on.
//!
//! Read-only to the console at this stage, with one write: `lumen-virt` asks
//! for a volume when a machine gets a disk and asks for it back when the disk
//! goes away. Everything Lumen creates lives under `<pool>/lumen/`, and
//! nothing outside that prefix is removable by anything in this crate.
//!
//! ```text
//!   model.rs      pools, datasets, volumes — and the namespace rule
//!   state.rs      what the box actually has
//!   devices.rs    what disks the node has, and what is already on each
//!   validate.rs   pure rules over a pool and the disks it would be built on
//!   iso.rs        the installation media library, one directory per pool
//!   backend/      the supported command line (cli/), plus mock/ and unavailable/
//!   service.rs    the one entry point the control plane calls
//! ```
//!
//! `lumen-controlplane` depends on this crate and contributes only HTTP: its
//! handlers deserialize, call one [`service::StorageService`] method, and
//! serialize the answer.
//!
//! ## The one thing that leaves the sandbox
//!
//! Everything here reaches the kernel through `/dev/zfs`, which
//! `ProtectSystem=strict` does not cover — except `zpool create` and `zpool
//! destroy`, which write `/etc/zfs/zpool.cache`. Those two are handed to
//! systemd through [`lumen_sys::exec`] and run outside the control plane's
//! namespace, which is why this crate depends on the system domain. See
//! docs/system.md.

pub mod backend;
pub mod devices;
pub mod error;
pub mod iso;
pub mod model;
pub mod service;
pub mod state;
pub mod validate;

pub use devices::DeviceRoots;
pub use error::{Result, ZfsError};
pub use iso::{IsoLibrary, IsoStoreView, IsoUpload, IsoView};
pub use model::{
    device_path, is_lumen_volume, is_reserved_leaf, iso_dataset, iso_mountpoint, lumen_root,
    valid_device_path, valid_iso_name, valid_new_pool_name, valid_pool_name, vm_disk_path,
    BlockDevice, Compression, Dataset, DatasetKind, LumenBrick, Pool, PoolHealth, PoolRequest,
    SnapshotInfo, VdevKind, VolumeRequest, DEFAULT_ASHIFT, ISO_MOUNT_ROOT, LUMEN_PREFIX,
};
pub use service::{DevicesResponse, PoolView, StorageService};
pub use state::{PoolContents, StorageState};
pub use validate::{Acknowledgements, PoolCreate, ValidationCode, ValidationError};
