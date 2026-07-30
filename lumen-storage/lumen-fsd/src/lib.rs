//! lumen-fsd: the LumenFS data-plane daemon, as a library.
//!
//! docs/lumenfs.md's split, honored: everything that can corrupt data is a
//! deterministic state machine in lumen-fs, exercised by simulation; this
//! crate is "a thin binding of that core to io_uring and sockets". This
//! slice is the sockets half — the peer replication link, the guest I/O
//! path, and the NBD bootstrap export — std-only, blocking I/O, threads.
//! io_uring and the ublk export are the Linux refinements that follow, and
//! they will slot in under the same [`daemon::GuestHandle`] without the
//! engine noticing.
//!
//! ```text
//!   wire.rs     PeerMessage ⇄ frames — the bytes the engine never owns
//!   daemon.rs   the harness: engine under a lock, ordered effect drain,
//!               session incarnations, suspension made real, maintenance
//!   nbd.rs      the bootstrap guest export over the daemon's guest path
//!   ublk/       the real guest export: a vdisk as /dev/ublkb<id>
//!   control.rs  the operator/orchestrator surface, and its typed client
//! ```
//!
//! The library shape exists for the tests: two whole daemons can run
//! in-process over loopback, which is how the two-node contract gets
//! checked against real sockets, real files, and real threads under plain
//! `cargo test` — the same contract the simulation pins, one layer closer
//! to the metal.

pub mod control;
pub mod daemon;
pub mod nbd;
pub mod ublk;
pub mod wire;

pub use control::Client;
pub use daemon::{format_brick, Config, Daemon, GuestHandle, Status};
