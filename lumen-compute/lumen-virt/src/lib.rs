//! Lumen virtualization: the machine model, the domain document it renders to,
//! validation, and the hypervisor backend that holds the result.
//!
//! ```text
//!   model.rs        what a machine is
//!   state.rs        what the hypervisor says about the machines it holds
//!   domain_xml.rs   the domain document, both directions, with round-trip tests
//!   validate.rs     pure rules over a machine and the node it would run on
//!   backend/        the hypervisor's own library, plus mock/ and unavailable/
//!   service.rs      the one entry point the control plane calls
//! ```
//!
//! `lumen-controlplane` depends on this crate and contributes only HTTP: its
//! handlers deserialize a request, call one [`service::VirtService`] method,
//! and serialize the answer.
//!
//! ## There is no database
//!
//! The hypervisor is the source of truth. It already stores every machine's
//! definition durably and hands it back verbatim, so Lumen keeps nothing of
//! its own on disk: the VMID, the description, and the tags live inside the
//! domain document's `<metadata>` element in a Lumen namespace, which the
//! hypervisor preserves across define and undefine. An operator can read the
//! whole truth with the hypervisor's own tools, and there is nothing to keep
//! in sync.
//!
//! This mirrors networking, where the network daemon's own profiles hold the
//! committed state and only the staged delta is persisted. Machines have no
//! staged delta at all — see the note at the top of `service.rs` for why the
//! checkpoint engine is deliberately not copied here.

pub mod backend;
pub mod domain_caps;
pub mod domain_xml;
pub mod error;
pub mod model;
pub mod osinfo;
pub mod service;
pub mod state;
pub mod validate;

pub use domain_caps::{CpuModelInfo, CpuModels};
pub use error::{Result, VirtError};
pub use model::{
    generate_mac, valid_tag, valid_vm_name, BootDevice, CacheMode, CpuModel, CpuTopology, DiskBus,
    Firmware, NicModel, VmCdrom, VmConfig, VmDisk, VmNic, CDROM_BUS, FIRST_VMID, LAST_VMID,
};
pub use osinfo::{OsCatalog, OsFamily, OsVariant, OSINFO_DB_ROOT, WINDOWS_FAMILY};
pub use service::{ConsoleProtocol, ConsoleTarget, VirtService};
pub use state::{DomainRuntime, DomainState, HostInfo, ObservedDomain};
pub use validate::{Acknowledgements, ValidationCode, ValidationError};
