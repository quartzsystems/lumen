//! Lumen replicated volumes: DRBD 9 resources over ZFS zvols.
//!
//! A replicated volume is one DRBD resource whose backing devices are thick
//! zvols under each member pool's lumen namespace — 2 or 3 replicas inside
//! one cluster, protocol C, internal metadata, `auto-promote`. The device
//! promotes when libvirt opens it on the node starting the machine, DRBD
//! itself refuses a second writer, and Pacemaker manages no promotion at
//! all: the regime's quorum or fencing options (decided by `lumen-cluster`'s
//! topology engine, never here) are what make that safe.
//!
//! ```text
//!   model.rs    names, byte-exact sizing, port and minor allocation
//!   render.rs   the resource file, rendered whole from the record
//!   state.rs    what `drbdsetup status --json` actually reports
//!   peers.rs    what one control plane asks another to do to its replicas
//!   backend/    the supported command line (cli/), plus mock/ and unavailable/
//!   service.rs  the one entry point the control plane calls
//! ```
//!
//! The stored shape — [`lumen_cluster::VolumeRecord`] — lives one crate
//! down, riding the environment's membership record: volume placement is
//! membership's business (a node holding replicas cannot leave its
//! cluster), and every console must describe every volume whether or not
//! its cluster answers. This crate owns everything operational about one.
//!
//! `lumen-controlplane` depends on this crate and contributes only HTTP:
//! its handlers deserialize, call one [`service::DrbdService`] method, and
//! serialize the answer.

pub mod backend;
pub mod error;
pub mod model;
pub mod peers;
pub mod render;
pub mod service;
pub mod state;

pub use error::{DrbdError, Result};
pub use model::{
    backing_size, device_path, resource_name, valid_volume_name, zvol_path, DRBD_PORT_MAX,
    DRBD_PORT_MIN, MAX_REPLICAS, MIN_REPLICAS, VOLBLOCKSIZE,
};
pub use peers::{MockVolumePeers, VolumePeers, VolumePrepare, VolumeResizeBacking, VolumeTeardown};
pub use service::{
    ClusterVolumes, DrbdService, ReplicaView, ReplicatedVolumesResponse, VolumeCreate,
    VolumeHealth, VolumeMemberCreate, VolumeView,
};
pub use state::{DeviceStatus, PeerDeviceStatus, PeerStatus, ResourceStatus};
