//! Errors the virtualization domain returns to its callers.
//!
//! The same shape as `lumen_net::NetError`, for the same reason: validation
//! failures are not errors, they are data ([`crate::validate::ValidationError`])
//! so the API can return the whole set at once and the console can render each
//! one against its own field.

use std::fmt;

use crate::validate::ValidationError;

pub type Result<T> = std::result::Result<T, VirtError>;

#[derive(Debug)]
pub enum VirtError {
    /// The request does not describe a machine we will define. Carries every
    /// problem found, not just the first.
    Invalid(Vec<ValidationError>),
    /// The request names something that is not there — a machine, a disk, an
    /// adapter.
    NotFound(String),
    /// Well-formed but not allowed in the current state: starting a machine
    /// that is already running, removing a disk from a machine that is, a node
    /// that isn't this one.
    Conflict(String),
    /// The hypervisor, the storage below it, or the document itself failed.
    Backend(anyhow::Error),
}

impl VirtError {
    pub fn backend(err: impl Into<anyhow::Error>) -> Self {
        VirtError::Backend(err.into())
    }

    /// A single rejection, for the checks that only ever produce one.
    pub fn invalid(error: ValidationError) -> Self {
        VirtError::Invalid(vec![error])
    }
}

impl fmt::Display for VirtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VirtError::Invalid(errors) => {
                let joined: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
                write!(f, "{}", joined.join(" "))
            }
            VirtError::NotFound(what) => write!(f, "{what}"),
            VirtError::Conflict(why) => write!(f, "{why}"),
            VirtError::Backend(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for VirtError {}

impl From<anyhow::Error> for VirtError {
    fn from(err: anyhow::Error) -> Self {
        VirtError::Backend(err)
    }
}

impl From<std::io::Error> for VirtError {
    fn from(err: std::io::Error) -> Self {
        VirtError::Backend(err.into())
    }
}

/// The storage domain's failures, carried through unchanged: "the pool is
/// full" is the same answer whether a disk or a machine asked for the space.
impl From<lumen_zfs::ZfsError> for VirtError {
    fn from(err: lumen_zfs::ZfsError) -> Self {
        match err {
            lumen_zfs::ZfsError::NotFound(message) => VirtError::NotFound(message),
            lumen_zfs::ZfsError::Conflict(message) => VirtError::Conflict(message),
            lumen_zfs::ZfsError::Backend(err) => VirtError::Backend(err),
            // Storage's validation arm is about *pools*, and nothing in the
            // compute domain creates one — a machine asks for a volume. If one
            // ever arrives here it is still a refusal with a reason in it, so
            // it is carried through as one rather than dropped.
            lumen_zfs::ZfsError::Invalid(errors) => VirtError::Conflict(
                errors
                    .iter()
                    .map(|e| e.message.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        }
    }
}

/// The replicated-storage domain, folded in the same way. "No pool here"
/// arrives as a conflict rather than a broken backend: it is a state the
/// caller can act on — create the pool, or give the machine a local disk.
impl From<lumen_pool::PoolError> for VirtError {
    fn from(err: lumen_pool::PoolError) -> Self {
        match err {
            lumen_pool::PoolError::NotFound(message) => VirtError::NotFound(message),
            lumen_pool::PoolError::Conflict(message) => VirtError::Conflict(message),
            lumen_pool::PoolError::Unavailable(message) => VirtError::Conflict(message),
            lumen_pool::PoolError::Backend(message) => VirtError::Backend(anyhow::anyhow!(message)),
        }
    }
}
