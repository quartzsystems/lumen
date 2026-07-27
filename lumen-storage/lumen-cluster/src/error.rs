//! Errors the clustering domain returns to its callers.
//!
//! The same shape as `lumen_zfs::ZfsError` and `lumen_net::NetError`. The
//! validation arm carries every problem found, not just the first, because a
//! cluster definition is the kind of request an operator assembles across a
//! whole wizard — telling them about one mistake per round trip would make the
//! wizard a guessing game.

use std::fmt;

use crate::validate::ValidationError;

pub type Result<T> = std::result::Result<T, ClusterError>;

#[derive(Debug)]
pub enum ClusterError {
    /// The request does not describe a cluster this environment will build.
    /// Carries every problem found, not just the first.
    Invalid(Vec<ValidationError>),
    /// The request names an environment, cluster, or node that is not there.
    NotFound(String),
    /// Well-formed but not allowed in the current state: a node that is
    /// already a member, an operation aimed at a cluster this node cannot
    /// speak for.
    Conflict(String),
    /// The command failed, or the cluster stack is not answering on this node.
    Backend(anyhow::Error),
}

impl ClusterError {
    pub fn backend(err: impl Into<anyhow::Error>) -> Self {
        ClusterError::Backend(err.into())
    }

    /// A single rejection, for the checks that only ever produce one.
    pub fn invalid(error: ValidationError) -> Self {
        ClusterError::Invalid(vec![error])
    }
}

impl fmt::Display for ClusterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClusterError::Invalid(errors) => {
                let joined: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
                write!(f, "{}", joined.join(" "))
            }
            ClusterError::NotFound(what) => write!(f, "{what}"),
            ClusterError::Conflict(why) => write!(f, "{why}"),
            ClusterError::Backend(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for ClusterError {}

impl From<anyhow::Error> for ClusterError {
    fn from(err: anyhow::Error) -> Self {
        ClusterError::Backend(err)
    }
}

impl From<std::io::Error> for ClusterError {
    fn from(err: std::io::Error) -> Self {
        ClusterError::Backend(err.into())
    }
}
