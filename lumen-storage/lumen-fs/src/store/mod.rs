//! Content-addressed extent storage: the on-disk formats, one disk's
//! store, and one node's set of them.
//!
//! Layered bottom-up: [`format`] is the bytes-on-platter contract
//! (superblock, anchor, segment and record headers), [`brick`] is one
//! disk's append-only segment store recovered by scan, and [`brick_set`]
//! presents a node's bricks — one per disk, per tier — as a single store
//! with per-tier routing and allocation. Everything above (the WAL ring,
//! the map trees, the pool) consumes these through [`brick::BlockRead`] /
//! [`brick::BlockWrite`] and the brick's own inherent surface.

pub mod brick;
pub mod brick_set;
pub mod format;
