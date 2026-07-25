//! Errors the storage domain returns to its callers.
//!
//! The same shape as `lumen_net::NetError` and `lumen_virt::VirtError`. The
//! validation arm arrived with pool creation and covers only that: the rules
//! about a *volume* still live in `lumen_virt::validate`, where the disk is
//! actually being asked for, because a volume is created for a machine. A pool
//! is not created for anything — it is a decision about the node's own disks,
//! and it is the one operation here with no undo.

use std::fmt;

use crate::validate::ValidationError;

pub type Result<T> = std::result::Result<T, ZfsError>;

#[derive(Debug)]
pub enum ZfsError {
    /// The request does not describe a pool this appliance will build. Carries
    /// every problem found, not just the first.
    Invalid(Vec<ValidationError>),
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

    /// A single rejection, for the checks that only ever produce one.
    pub fn invalid(error: ValidationError) -> Self {
        ZfsError::Invalid(vec![error])
    }
}

impl fmt::Display for ZfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZfsError::Invalid(errors) => {
                let joined: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
                write!(f, "{}", joined.join(" "))
            }
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
