//! Lumen updates: what this node could install, and installing it.
//!
//! ```text
//!   model.rs        what an update is, and which of four kinds it belongs to
//!   backend/        the package manager, plus mock/ and unavailable/
//!   service.rs      the one entry point the control plane calls
//! ```
//!
//! `lumen-controlplane` depends on this crate and contributes only HTTP and
//! orchestration: its handlers deserialize a request, call one
//! [`service::UpdateService`] method, and serialize the answer, exactly as they
//! do for every other domain.
//!
//! ## The kernel does not move by accident
//!
//! This is the whole reason the domain exists rather than the console simply
//! running `dnf upgrade`.
//!
//! Lumen's root file system is ZFS, and ZFS on this appliance is an
//! out-of-tree kernel module that tracks the kernel ABI. So is DRBD. Both are
//! pinned against one exact kernel at ISO build time, and both have a real
//! history of lagging AlmaLinux point-release kernels by days or weeks —
//! iso/pins.env records that in so many words, and the ISO build gates on it.
//!
//! A node that ran an unguarded `dnf upgrade` would sooner or later install a
//! kernel with no matching `kmod-zfs`, reboot, and fail to import its root
//! pool. There is no console to fix that from; it is a drive to the rack.
//!
//! So the domain splits every pending update in two:
//!
//! - **The ordinary ones** — Lumen's own packages, and everything in userland.
//!   These are what an operator installs on a Tuesday, and applying them can
//!   never move the kernel, because the transaction excludes the platform set
//!   by name ([`model::PLATFORM_PREFIXES`]).
//! - **The platform set** — kernel, `kmod-*`, ZFS, DRBD. These move *together
//!   or not at all*, and only after the package manager has been asked, in a
//!   dry run, whether it can resolve them as one transaction. If it cannot,
//!   the console says so and the button is not offered; there is no way to
//!   half-apply the set.
//!
//! The gate is the depsolver's own answer rather than a version comparison
//! Lumen keeps, for the same reason the ISO build resolves its target set
//! offline instead of subtracting package names by hand: the solver is right
//! about its own repositories and a hand-maintained rule is right until the
//! next point release.
//!
//! ## Reboots are reported, never taken
//!
//! Nothing here restarts the node. A kernel that was installed is not a kernel
//! that is running, and the difference is [`model::RebootState`] — computed by
//! comparing the running release against the newest installed one, which needs
//! no package-manager plugin and cannot disagree with `uname`. Restarting is
//! the operator's decision, on the page that already owns it, through
//! `lumen-sys` — and in a cluster it goes through maintenance mode first.
//!
//! ## There is no database
//!
//! The list of available updates is the package manager's answer, read fresh.
//! Nothing is cached to disk, so nothing can disagree with `dnf check-update`
//! typed at the keyboard. The one piece of state is the in-memory result of
//! the last check, which exists so the console can show a badge without making
//! every page load refresh repository metadata over the network.

pub mod backend;
pub mod error;
pub mod model;
pub mod service;

pub use backend::{
    dnf::DnfBackend, mock::MockUpdates, unavailable::UnavailableUpdates, UpdateBackend,
};
pub use error::{Result, UpdateError};
pub use model::{
    is_platform, ApplyPlan, ApplyReport, KernelState, PlatformPlan, RebootState, Resolution,
    Update, UpdateKind, PLATFORM_PREFIXES,
};
pub use service::{ApplyRequest, UpdateService, UpdateView};
