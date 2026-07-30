//! What a control plane needs to know about the pool on its own node.
//!
//! There is deliberately no pool record and no console-side configuration
//! here. The daemon's drop-in — `/etc/lumen/fsd.conf`, the `EnvironmentFile`
//! the shipped unit reads — is already the one place that says whether this
//! node carries a pool, and duplicating that into a second file or a
//! replicated record is how the two come to disagree. The same reasoning
//! that made the engine's node id something to *ask* the daemon for rather
//! than configure twice.
//!
//! So: **the file's presence is the answer to "is there a pool here"**, and
//! the rest is read out of it. Membership is not in it and does not belong
//! in it — a pool spans its cluster, so the members are the cluster's
//! members, which the control plane already knows.
//!
//! Written by hand today. The workflow that creates a pool will write it,
//! and that is phase 4's drive wizard, where choosing which disks become
//! bricks belongs.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// The control address the shipped unit passes. A constant rather than a
/// setting because it is one in the unit too: `--control 127.0.0.1:7799`,
/// loopback, deliberately not reachable from off-box.
pub const DEFAULT_CONTROL: &str = "127.0.0.1:7799";

/// Where the daemon's drop-in lives.
pub const FSD_CONF: &str = "/etc/lumen/fsd.conf";

/// This node's pool daemon, as its configuration describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// The brick this node's daemon serves from.
    pub brick: PathBuf,
    /// The engine node id the unit starts it with. Note this is *not* what
    /// the orchestration layer trusts — it asks the daemon, which cannot
    /// disagree with itself. It is here because the file carries it and
    /// silently dropping a field invites the next reader to assume it is
    /// absent.
    pub node: u8,
    /// The daemon's control surface. Loopback.
    pub control: SocketAddr,
}

/// What went wrong reading it. A missing file is not one of these — that is
/// `Ok(None)`, the standalone appliance, which is the common case and not an
/// error anywhere.
#[derive(Debug)]
pub enum ConfigError {
    Unreadable(String),
    Malformed(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Unreadable(why) => write!(f, "{why}"),
            ConfigError::Malformed(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl PoolConfig {
    /// Read the drop-in at the standard path.
    pub fn load() -> Result<Option<PoolConfig>, ConfigError> {
        PoolConfig::load_from(Path::new(FSD_CONF))
    }

    /// Read one, or `Ok(None)` if there is no pool on this node.
    pub fn load_from(path: &Path) -> Result<Option<PoolConfig>, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(ConfigError::Unreadable(format!(
                    "{} could not be read: {err}",
                    path.display()
                )))
            }
        };
        PoolConfig::parse(&text).map(Some)
    }

    /// Parse the `KEY=value` shape systemd's `EnvironmentFile` takes.
    ///
    /// A file that exists but does not say what it must is an **error**, not
    /// an absent pool: somebody wrote it, and reading it as "no pool here"
    /// would hide a broken deployment behind a console page that cheerfully
    /// says there is nothing to show.
    pub fn parse(text: &str) -> Result<PoolConfig, ConfigError> {
        let mut brick = None;
        let mut node = None;
        let mut control = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            // systemd tolerates quoting; strip it rather than baking it into
            // a path.
            let value = value.trim().trim_matches(['"', '\'']);
            match key.trim() {
                "LUMEN_FSD_BRICK" => brick = Some(PathBuf::from(value)),
                "LUMEN_FSD_NODE" => {
                    node = Some(value.parse().map_err(|_| {
                        ConfigError::Malformed(format!("LUMEN_FSD_NODE is not a node id: {value}"))
                    })?)
                }
                "LUMEN_FSD_CONTROL" => {
                    control = Some(value.parse().map_err(|_| {
                        ConfigError::Malformed(format!(
                            "LUMEN_FSD_CONTROL is not an address: {value}"
                        ))
                    })?)
                }
                _ => {}
            }
        }
        Ok(PoolConfig {
            brick: brick.ok_or_else(|| {
                ConfigError::Malformed("no LUMEN_FSD_BRICK: this names the pool's brick".into())
            })?,
            node: node.ok_or_else(|| {
                ConfigError::Malformed("no LUMEN_FSD_NODE: this names the engine's node id".into())
            })?,
            // The unit hardcodes the control address, so a file without one
            // is ordinary rather than incomplete.
            control: control.unwrap_or_else(|| DEFAULT_CONTROL.parse().expect("a valid constant")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drop_in_the_unit_actually_reads_is_what_this_parses() {
        // The shape packages/lumen-fsd.spec ships and a pool workflow will
        // write: systemd EnvironmentFile syntax, comments and all.
        let config = PoolConfig::parse(
            "# written by the pool create workflow\n\
             LUMEN_FSD_BRICK=/dev/disk/by-id/nvme-eui.0001\n\
             LUMEN_FSD_NODE=1\n\
             LUMEN_FSD_PEER=--dial 10.10.0.1:7800\n",
        )
        .unwrap();
        assert_eq!(config.brick, PathBuf::from("/dev/disk/by-id/nvme-eui.0001"));
        assert_eq!(config.node, 1);
        // Not in the file, because the unit passes it: the default is the
        // unit's own constant, and loopback on purpose.
        assert_eq!(config.control.to_string(), DEFAULT_CONTROL);
    }

    #[test]
    fn a_quoted_value_is_a_value_not_a_path_with_quotes_in_it() {
        let config =
            PoolConfig::parse("LUMEN_FSD_BRICK=\"/var/lib/lumen/brick\"\nLUMEN_FSD_NODE='0'\n")
                .unwrap();
        assert_eq!(config.brick, PathBuf::from("/var/lib/lumen/brick"));
        assert_eq!(config.node, 0);
    }

    #[test]
    fn a_control_override_is_honored_because_the_tests_need_one() {
        let config = PoolConfig::parse(
            "LUMEN_FSD_BRICK=/b\nLUMEN_FSD_NODE=0\nLUMEN_FSD_CONTROL=127.0.0.1:7741\n",
        )
        .unwrap();
        assert_eq!(config.control.to_string(), "127.0.0.1:7741");
    }

    #[test]
    fn no_file_is_no_pool_and_that_is_not_an_error() {
        let missing = std::env::temp_dir().join("lumen-pool-no-such-fsd.conf");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(PoolConfig::load_from(&missing).unwrap(), None);
    }

    #[test]
    fn a_file_that_exists_but_says_nothing_useful_is_an_error_not_an_absent_pool() {
        // The failure this prevents: a half-written drop-in reading as "no
        // pool on this node", so the console shows an empty page instead of
        // saying the deployment is broken.
        for wrong in [
            "",
            "# only a comment\n",
            "LUMEN_FSD_NODE=0\n",
            "LUMEN_FSD_BRICK=/b\n",
            "LUMEN_FSD_BRICK=/b\nLUMEN_FSD_NODE=not-a-number\n",
            "LUMEN_FSD_BRICK=/b\nLUMEN_FSD_NODE=0\nLUMEN_FSD_CONTROL=nonsense\n",
        ] {
            assert!(
                PoolConfig::parse(wrong).is_err(),
                "{wrong:?} was accepted anyway"
            );
        }
    }
}
