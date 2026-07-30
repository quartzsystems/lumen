//! What can go wrong reaching a pool, and how it reads to the compute
//! domain.
//!
//! The seam returns `lumen_drbd::Result`, so every failure here has to
//! arrive as a `DrbdError` the console already knows how to render. That
//! mapping is the whole of this module, and it is deliberately narrow:
//! `NotFound` for a thing that is not there, `Conflict` for a state that
//! refuses, and `Backend` for a daemon that could not be reached or did not
//! make sense.

use anyhow::anyhow;
use lumen_drbd::DrbdError;

pub type Result<T> = std::result::Result<T, PoolError>;

#[derive(Debug)]
pub enum PoolError {
    /// A vdisk, member, or export that does not exist.
    NotFound(String),
    /// A state that refuses: a pen another member holds, a window nobody
    /// opened, a name already taken.
    Conflict(String),
    /// There is no pool on this node — the standalone appliance, or a node
    /// whose daemon is not running.
    Unavailable(String),
    /// The daemon answered badly, or not at all.
    Backend(String),
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::NotFound(what) => write!(f, "{what}"),
            PoolError::Conflict(what) => write!(f, "{what}"),
            PoolError::Unavailable(what) => write!(f, "{what}"),
            PoolError::Backend(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for PoolError {}

impl From<PoolError> for DrbdError {
    fn from(err: PoolError) -> DrbdError {
        match err {
            PoolError::NotFound(what) => DrbdError::NotFound(what),
            PoolError::Conflict(what) => DrbdError::Conflict(what),
            // "No pool here" is a state the caller can act on — add one,
            // or run the machine elsewhere — not a broken backend.
            PoolError::Unavailable(what) => DrbdError::Conflict(what),
            PoolError::Backend(what) => DrbdError::Backend(anyhow!(what)),
        }
    }
}
