//! The guest-facing exports: a vdisk presented as a block device.
//!
//! Two shapes over the same [`crate::daemon::GuestHandle`]: [`nbd`] is the
//! bootstrap and debugging export, a userspace socket protocol that runs
//! anywhere; [`ublk`] is the VM path, the in-kernel block device with the
//! stable per-member id the compute seam's device-path promise rides on.
//! NBD is never the VM path — docs/lumenfs.md's position, kept.

pub mod nbd;
pub mod ublk;
