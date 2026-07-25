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
//!   iso.rs        the installation media library, one directory per pool
//!   backend/      the supported command line (cli/), plus mock/ and unavailable/
//!   service.rs    the one entry point the control plane calls
//! ```
//!
//! `lumen-controlplane` depends on this crate and contributes only HTTP: its
//! handlers deserialize, call one [`service::StorageService`] method, and
//! serialize the answer. Pool creation, import, and destroy are deliberately
//! out of scope — they are the operations with no privileged daemon to
//! delegate to. See docs/compute.md.

pub mod backend;
pub mod error;
pub mod iso;
pub mod model;
pub mod service;
pub mod state;

pub use error::{Result, ZfsError};
pub use iso::{IsoLibrary, IsoStoreView, IsoUpload, IsoView};
pub use model::{
    device_path, is_lumen_volume, is_reserved_leaf, iso_dataset, iso_mountpoint, lumen_root,
    valid_iso_name, valid_pool_name, vm_disk_path, Dataset, DatasetKind, Pool, PoolHealth,
    VolumeRequest, ISO_MOUNT_ROOT, LUMEN_PREFIX,
};
pub use service::StorageService;
pub use state::{PoolContents, StorageState};
