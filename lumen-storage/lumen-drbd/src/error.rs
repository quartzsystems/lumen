//! Errors the replicated-storage domain returns to its callers.
//!
//! The same four-arm shape as every other domain. Validation reuses
//! `lumen_cluster::ValidationError` — the volume codes live in the cluster
//! crate so the console matches on exactly one set — and the two domains
//! this one is built on fold in with their meanings preserved.

use std::fmt;

use lumen_cluster::ValidationError;

pub type Result<T> = std::result::Result<T, DrbdError>;

#[derive(Debug)]
pub enum DrbdError {
    /// The request does not describe a volume this cluster will build.
    /// Carries every problem found, not just the first.
    Invalid(Vec<ValidationError>),
    /// The request names a cluster, volume, or node that is not there.
    NotFound(String),
    /// Well-formed but not allowed in the current state: a shrink, a full
    /// port range, a destroy without its acknowledgement.
    Conflict(String),
    /// A command failed, or DRBD is not answering on this node.
    Backend(anyhow::Error),
}

impl DrbdError {
    pub fn backend(err: impl Into<anyhow::Error>) -> Self {
        DrbdError::Backend(err.into())
    }

    /// A single rejection, for the checks that only ever produce one.
    pub fn invalid(error: ValidationError) -> Self {
        DrbdError::Invalid(vec![error])
    }
}

impl fmt::Display for DrbdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DrbdError::Invalid(errors) => {
                let joined: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
                write!(f, "{}", joined.join(" "))
            }
            DrbdError::NotFound(what) => write!(f, "{what}"),
            DrbdError::Conflict(why) => write!(f, "{why}"),
            DrbdError::Backend(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for DrbdError {}

impl From<anyhow::Error> for DrbdError {
    fn from(err: anyhow::Error) -> Self {
        DrbdError::Backend(err)
    }
}

/// The storage domain underneath: a zvol refusal keeps its meaning — a pool
/// without space is a conflict here too, not a crash. Its validation arm
/// arrives as a conflict sentence: by the time this domain touches a zvol,
/// the volume request was already validated here, so a refusal from below is
/// a state problem, not a form problem.
impl From<lumen_zfs::ZfsError> for DrbdError {
    fn from(err: lumen_zfs::ZfsError) -> Self {
        match err {
            lumen_zfs::ZfsError::Invalid(errors) => {
                let joined: Vec<String> = errors.into_iter().map(|e| e.message).collect();
                DrbdError::Conflict(joined.join(" "))
            }
            lumen_zfs::ZfsError::NotFound(message) => DrbdError::NotFound(message),
            lumen_zfs::ZfsError::Conflict(message) => DrbdError::Conflict(message),
            lumen_zfs::ZfsError::Backend(err) => DrbdError::Backend(err),
        }
    }
}

/// The clustering domain underneath: its validation list passes through
/// intact, everything else keeps its arm.
impl From<lumen_cluster::ClusterError> for DrbdError {
    fn from(err: lumen_cluster::ClusterError) -> Self {
        match err {
            lumen_cluster::ClusterError::Invalid(errors) => DrbdError::Invalid(errors),
            lumen_cluster::ClusterError::NotFound(message) => DrbdError::NotFound(message),
            lumen_cluster::ClusterError::Conflict(message) => DrbdError::Conflict(message),
            lumen_cluster::ClusterError::Backend(err) => DrbdError::Backend(err),
        }
    }
}
