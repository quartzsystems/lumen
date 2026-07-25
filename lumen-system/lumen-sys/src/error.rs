//! Errors the system domain returns to its callers.
//!
//! The same shape as `lumen_net::NetError` and `lumen_virt::VirtError`, for the
//! same reason: a rejected account is not an error, it is data
//! ([`crate::validate::ValidationError`]) so the API can return the whole set at
//! once and the console can render each one against its own field.

use std::fmt;

use crate::validate::ValidationError;

pub type Result<T> = std::result::Result<T, SysError>;

#[derive(Debug)]
pub enum SysError {
    /// The request does not describe an account this appliance will create.
    /// Carries every problem found, not just the first.
    Invalid(Vec<ValidationError>),
    /// The request names something that is not there.
    NotFound(String),
    /// Well-formed but not allowed: removing the account you are signed in as,
    /// scheduling a shutdown for a moment that has already passed.
    Conflict(String),
    /// The node, the account database, or systemd failed.
    Backend(anyhow::Error),
}

impl SysError {
    pub fn backend(err: impl Into<anyhow::Error>) -> Self {
        SysError::Backend(err.into())
    }

    /// A single rejection, for the checks that only ever produce one.
    pub fn invalid(error: ValidationError) -> Self {
        SysError::Invalid(vec![error])
    }
}

impl fmt::Display for SysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SysError::Invalid(errors) => {
                let joined: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
                write!(f, "{}", joined.join(" "))
            }
            SysError::NotFound(what) => write!(f, "{what}"),
            SysError::Conflict(why) => write!(f, "{why}"),
            SysError::Backend(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for SysError {}

impl From<anyhow::Error> for SysError {
    fn from(err: anyhow::Error) -> Self {
        SysError::Backend(err)
    }
}

impl From<std::io::Error> for SysError {
    fn from(err: std::io::Error) -> Self {
        SysError::Backend(err.into())
    }
}
