//! Lumen networking: bridges, bonds, VLANs, and per-NIC settings.
//!
//! The whole networking domain lives here — desired state, observed state,
//! validation, planning, and the backend that applies plans. `lumen-
//! controlplane` depends on this crate and contributes only HTTP: its handlers
//! deserialize a request, call one [`service::NetworkService`] method, and
//! serialize the answer.
//!
//! ```text
//!   model.rs      what the operator wants
//!   state.rs      what the box actually has
//!   validate.rs   pure rules over the two
//!   plan.rs       the two -> an ordered list of backend operations
//!   backend/      NetworkManager over D-Bus, and an in-memory mock
//!   store.rs      the committed, staged, and in-flight state on disk
//!   service.rs    stage -> validate -> checkpoint -> apply -> confirm
//! ```
//!
//! Applying is never a one-way door: every change runs inside a
//! NetworkManager checkpoint that reverts itself if nobody confirms it. See
//! docs/networking.md.

pub mod backend;
pub mod error;
pub mod model;
pub mod plan;
pub mod service;
pub mod state;
pub mod store;
pub mod validate;

pub use error::{NetError, Result};
pub use model::{
    Bond, BondMode, Bridge, Duplex, IpConfig, LacpRate, LinkKind, ManagementRef,
    NetworkDesiredState, Nic, Vlan, XmitHashPolicy,
};
pub use service::NetworkService;
pub use state::{LinkState, ObservedLink, ObservedState};
pub use validate::{Acknowledgements, ValidationCode, ValidationError};
