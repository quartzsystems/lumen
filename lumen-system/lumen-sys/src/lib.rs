//! Lumen system: the node itself — its local accounts, its power state, and
//! the one thing every other domain needs and none of them should own.
//!
//! ```text
//!   model.rs        what a local account is; what a power request is
//!   state.rs        what /etc/passwd, /etc/shadow, and /etc/group say
//!   validate.rs     pure rules over an account and the node it would live on
//!   exec.rs         running a privileged command OUTSIDE this daemon's sandbox
//!   backend/        logind over the system bus, plus mock/ and unavailable/
//!   service.rs      the one entry point the control plane calls
//! ```
//!
//! `lumen-controlplane` depends on this crate and contributes only HTTP: its
//! handlers deserialize a request, call one [`service::SysService`] method, and
//! serialize the answer.
//!
//! ## This is the most basic domain in the tree
//!
//! It depends on none of the others, and `lumen-zfs` depends on it — creating a
//! pool is a privileged command on the node before it is anything to do with
//! storage. The full order is:
//!
//! ```text
//!   lumen-sys  <-  lumen-zfs  <-  lumen-virt
//!                  lumen-net  <-------^
//! ```
//!
//! ## There is no database
//!
//! The account database is `/etc/passwd`, `/etc/shadow`, and `/etc/group` — the
//! same three files every other tool on the node reads. Nothing is cached, so
//! nothing can disagree with `getent`, and an account somebody made at the
//! keyboard appears here without anything being told about it. This mirrors
//! `lumen-virt`, where the hypervisor's own domain document is the only stored
//! state.
//!
//! ## The sandbox does not move
//!
//! `lumen-controlplane.service` runs with `ProtectSystem=strict`, and creating
//! an account needs `/etc` writable. Rather than relaxing the unit, the command
//! is handed to systemd, which runs it as a transient unit of its own — outside
//! this daemon's namespace, with none of its restrictions. [`exec`] is where
//! that happens and why; docs/system.md works through the alternative that was
//! rejected.

pub mod backend;
pub mod error;
pub mod exec;
pub mod model;
pub mod service;
pub mod state;
pub mod validate;

pub use backend::{PowerBackend, ScheduledPower};
pub use error::{Result, SysError};
pub use exec::{Exec, MockExec, Outcome, Request, SystemdRun, UnavailableExec};
pub use model::{
    valid_user_name, LocalUser, LoginState, NewUser, PowerAction, UserPatch, ADMIN_GROUP,
    DEFAULT_SHELL, FIRST_HUMAN_UID, LAST_HUMAN_UID, MIN_PASSWORD_LEN,
};
pub use service::{
    Action, DeleteUserResponse, PowerView, SysService, UserActions, UserView, UsersResponse,
    SCHEDULE_HORIZON_SECS,
};
pub use state::{AccountFiles, Accounts};
pub use validate::{Acknowledgements, NodeFacts, ValidationCode, ValidationError};
