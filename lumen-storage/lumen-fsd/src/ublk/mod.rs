//! The ublk export: the vdisk as a real block device, `/dev/ublkb<id>`.
//!
//! Split by what can be verified where: [`uapi`] is the kernel interface
//! transcribed, layout-tested on every platform; [`uring`] and [`server`]
//! are the Linux plumbing that uses it, compiled only where they can run.
//! The smoke test lives on lumen1 — WSL's kernel has no ublk_drv, so the
//! only honest runtime check is the real appliance kernel this targets.

pub mod uapi;

#[cfg(target_os = "linux")]
mod server;
#[cfg(target_os = "linux")]
mod uring;

#[cfg(target_os = "linux")]
pub use server::{delete_device, start, Export};
