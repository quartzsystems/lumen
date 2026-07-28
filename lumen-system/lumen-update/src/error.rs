//! Errors the update domain returns to its callers.
//!
//! The same shape as `lumen_sys::SysError` and `lumen_net::NetError`, minus the
//! validation arm: there is no document to reject here. An update request names
//! nothing an operator typed — it is a button — so the ways it can fail are
//! "not that, not now, or the package manager said no".

use std::fmt;

pub type Result<T> = std::result::Result<T, UpdateError>;

#[derive(Debug)]
pub enum UpdateError {
    /// The request names something that is not waiting to be installed.
    NotFound(String),
    /// Well-formed but not allowed: a second transaction while one is running,
    /// or the platform set without the acknowledgement it requires.
    Conflict(String),
    /// The package manager failed, or could not be reached at all.
    Backend(anyhow::Error),
}

impl UpdateError {
    pub fn backend(err: impl Into<anyhow::Error>) -> Self {
        UpdateError::Backend(err.into())
    }

    pub fn conflict(why: impl Into<String>) -> Self {
        UpdateError::Conflict(why.into())
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateError::NotFound(what) => write!(f, "{what}"),
            UpdateError::Conflict(why) => write!(f, "{why}"),
            UpdateError::Backend(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<anyhow::Error> for UpdateError {
    fn from(err: anyhow::Error) -> Self {
        UpdateError::Backend(err)
    }
}

impl From<lumen_sys::SysError> for UpdateError {
    fn from(err: lumen_sys::SysError) -> Self {
        UpdateError::Backend(anyhow::anyhow!("{err}"))
    }
}
