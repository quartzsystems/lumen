//! Errors the storage domain returns to its callers.
//!
//! The same three-plus-one shape as `lumen_net::NetError`, minus the
//! validation arm: this stage reads pools and creates the volumes a virtual
//! machine's disks live on, and the rules worth a machine-readable code live
//! in `lumen_virt::validate` where the disk is actually being asked for.

use std::fmt;

pub type Result<T> = std::result::Result<T, ZfsError>;

#[derive(Debug)]
pub enum ZfsError {
    /// The request names a pool, dataset, or volume that is not there.
    NotFound(String),
    /// Well-formed but not allowed: a destroy aimed outside the Lumen
    /// namespace, a node that isn't this one.
    Conflict(String),
    /// The command failed, or storage is not available on this node at all.
    Backend(anyhow::Error),
}

impl ZfsError {
    pub fn backend(err: impl Into<anyhow::Error>) -> Self {
        ZfsError::Backend(err.into())
    }
}

impl fmt::Display for ZfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZfsError::NotFound(what) => write!(f, "{what}"),
            ZfsError::Conflict(why) => write!(f, "{why}"),
            ZfsError::Backend(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for ZfsError {}

impl From<anyhow::Error> for ZfsError {
    fn from(err: anyhow::Error) -> Self {
        ZfsError::Backend(err)
    }
}

impl From<std::io::Error> for ZfsError {
    fn from(err: std::io::Error) -> Self {
        ZfsError::Backend(err.into())
    }
}
