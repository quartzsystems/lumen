//! Errors the networking domain returns to its callers.
//!
//! Validation failures are NOT errors: they are data
//! ([`crate::validate::ValidationError`]) so the API can return the whole set
//! at once and the console can render each one against its own field.

use std::fmt;

use crate::validate::ValidationError;

pub type Result<T> = std::result::Result<T, NetError>;

#[derive(Debug)]
pub enum NetError {
    /// The desired state does not describe a configuration we will apply.
    /// Carries every problem found, not just the first.
    Invalid(Vec<ValidationError>),
    /// The request names something that is not there (a link, a checkpoint).
    NotFound(String),
    /// The request is well-formed but not allowed in the current state — an
    /// apply while a checkpoint is outstanding, a node that isn't this one.
    Conflict(String),
    /// The backend (NetworkManager, the state dir) failed.
    Backend(anyhow::Error),
}

impl NetError {
    pub fn backend(err: impl Into<anyhow::Error>) -> Self {
        NetError::Backend(err.into())
    }
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Invalid(errors) => {
                let joined: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
                write!(f, "{}", joined.join(" "))
            }
            NetError::NotFound(what) => write!(f, "{what}"),
            NetError::Conflict(why) => write!(f, "{why}"),
            NetError::Backend(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for NetError {}

impl From<anyhow::Error> for NetError {
    fn from(err: anyhow::Error) -> Self {
        NetError::Backend(err)
    }
}

impl From<zbus::Error> for NetError {
    fn from(err: zbus::Error) -> Self {
        NetError::Backend(err.into())
    }
}

impl From<std::io::Error> for NetError {
    fn from(err: std::io::Error) -> Self {
        NetError::Backend(err.into())
    }
}
