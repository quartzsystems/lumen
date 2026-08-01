//! What can go wrong reaching a pool, and how it reads to the compute
//! domain.
//!
//! Deliberately narrow: `NotFound` for a thing that is not there,
//! `Conflict` for a state that refuses, and `Backend` for a daemon that
//! could not be reached or did not make sense. The seam in `vm.rs` returns
//! this type directly — there is no second error for the console to learn.

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
