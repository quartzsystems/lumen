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
//! [`PoolConfig::render`] is the writer the pool create workflow uses, and
//! it round-trips through [`PoolConfig::parse`] by test — one shape,
//! written and read by the same code. The file carries *addresses*: which
//! paths, which node id, which way the peer link points. The facts —
//! tiers, identities, the set's roster — live on the platters, and a conf
//! naming a different set than the anchors roster is refused at the
//! daemon's own open.

use std::fmt::Write as _;
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
    /// The bricks this node's daemon serves from — one path per disk,
    /// space-separated in the file so the unit's `$LUMEN_FSD_BRICK`
    /// word-splits them straight into argv. Paths with whitespace are
    /// refused where they enter (the daemon and the render alike).
    pub bricks: Vec<PathBuf>,
    /// The engine node id the unit starts it with. Note this is *not* what
    /// the orchestration layer trusts — it asks the daemon, which cannot
    /// disagree with itself. It is here because the file carries it and
    /// silently dropping a field invites the next reader to assume it is
    /// absent.
    pub node: u8,
    /// This node's ends of the mesh: at most one listener, one dial per
    /// lower-id member.
    pub peers: Vec<PeerRole>,
    /// The pool's member ids, for placement. Empty is the pre-placement
    /// legacy: the daemon opens unplaced and every member holds
    /// everything. Written for every pool the workflow creates now.
    pub members: Vec<u8>,
    /// The daemon's control surface. Loopback.
    pub control: SocketAddr,
}

/// One end of a peer link: listen for higher-id members, dial a lower-id
/// one. The conf carries these as the daemon's own flags, verbatim.
/// Serialized because the create workflow's prepare payload carries a
/// member's set to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", content = "addr", rename_all = "kebab-case")]
pub enum PeerRole {
    Listen(SocketAddr),
    Dial(SocketAddr),
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
        let mut bricks: Option<Vec<PathBuf>> = None;
        let mut node = None;
        let mut peers: Vec<PeerRole> = Vec::new();
        let mut members: Vec<u8> = Vec::new();
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
                "LUMEN_FSD_BRICK" => {
                    let paths: Vec<PathBuf> = value.split_whitespace().map(PathBuf::from).collect();
                    if paths.is_empty() {
                        return Err(ConfigError::Malformed(
                            "LUMEN_FSD_BRICK names no paths".into(),
                        ));
                    }
                    bricks = Some(paths);
                }
                "LUMEN_FSD_NODE" => {
                    node = Some(value.parse().map_err(|_| {
                        ConfigError::Malformed(format!("LUMEN_FSD_NODE is not a node id: {value}"))
                    })?)
                }
                "LUMEN_FSD_PEER" => {
                    // The value is the daemon's own peer-topology flags,
                    // verbatim: `[--listen <addr>] [--dial <addr>]...
                    // [--members <id,id,...>]` — the unit word-splits it
                    // straight into argv. Parsed rather than carried as
                    // prose, because a workflow that writes it must also
                    // be able to read back what it wrote.
                    let tokens: Vec<&str> = value.split_whitespace().collect();
                    let mut at = 0;
                    while at < tokens.len() {
                        let flag = tokens[at];
                        let arg = tokens.get(at + 1).ok_or_else(|| {
                            ConfigError::Malformed(format!(
                                "LUMEN_FSD_PEER flag {flag} has no value: {value}"
                            ))
                        })?;
                        match flag {
                            "--listen" | "--dial" => {
                                let addr: SocketAddr = arg.parse().map_err(|_| {
                                    ConfigError::Malformed(format!(
                                        "LUMEN_FSD_PEER address does not parse: {arg}"
                                    ))
                                })?;
                                peers.push(if flag == "--listen" {
                                    PeerRole::Listen(addr)
                                } else {
                                    PeerRole::Dial(addr)
                                });
                            }
                            "--members" => {
                                members = arg
                                    .split(',')
                                    .map(|id| {
                                        id.parse().map_err(|_| {
                                            ConfigError::Malformed(format!(
                                                "LUMEN_FSD_PEER member id does not parse: {id}"
                                            ))
                                        })
                                    })
                                    .collect::<Result<Vec<u8>, _>>()?;
                            }
                            _ => {
                                return Err(ConfigError::Malformed(format!(
                                    "LUMEN_FSD_PEER flag is not one of --listen/--dial/--members: \
                                     {flag}"
                                )))
                            }
                        }
                        at += 2;
                    }
                    if peers
                        .iter()
                        .filter(|role| matches!(role, PeerRole::Listen(_)))
                        .count()
                        > 1
                    {
                        return Err(ConfigError::Malformed(
                            "LUMEN_FSD_PEER names two listeners; a daemon has one door".into(),
                        ));
                    }
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
            bricks: bricks.ok_or_else(|| {
                ConfigError::Malformed("no LUMEN_FSD_BRICK: this names the pool's bricks".into())
            })?,
            node: node.ok_or_else(|| {
                ConfigError::Malformed("no LUMEN_FSD_NODE: this names the engine's node id".into())
            })?,
            // Empty is legal: a one-node bring-up has no peers yet, and a
            // pre-placement pool names no members.
            peers,
            members,
            // The unit hardcodes the control address, so a file without one
            // is ordinary rather than incomplete.
            control: control.unwrap_or_else(|| DEFAULT_CONTROL.parse().expect("a valid constant")),
        })
    }

    /// The file as the pool create workflow writes it. Round-trips through
    /// [`PoolConfig::parse`] — the test that keeps writer and reader one
    /// contract. A brick path containing whitespace is refused: the file's
    /// own format could not carry it.
    pub fn render(&self) -> Result<String, ConfigError> {
        for path in &self.bricks {
            if path.to_string_lossy().chars().any(char::is_whitespace) {
                return Err(ConfigError::Malformed(format!(
                    "brick path contains whitespace: {}",
                    path.display()
                )));
            }
        }
        if self.bricks.is_empty() {
            return Err(ConfigError::Malformed("a pool with no bricks".into()));
        }
        let mut out = String::new();
        out.push_str(
            "# Written by the pool create workflow. Do not edit: destroy and\n\
             # recreate the pool instead.\n",
        );
        let paths: Vec<String> = self
            .bricks
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let _ = writeln!(out, "LUMEN_FSD_BRICK={}", paths.join(" "));
        let _ = writeln!(out, "LUMEN_FSD_NODE={}", self.node);
        if !self.peers.is_empty() || !self.members.is_empty() {
            // Canonical order — listener first, dials after, members last
            // — so writer and reader converge on one spelling.
            let mut flags: Vec<String> = Vec::new();
            for role in &self.peers {
                if let PeerRole::Listen(addr) = role {
                    flags.push(format!("--listen {addr}"));
                }
            }
            for role in &self.peers {
                if let PeerRole::Dial(addr) = role {
                    flags.push(format!("--dial {addr}"));
                }
            }
            if !self.members.is_empty() {
                let ids: Vec<String> = self.members.iter().map(u8::to_string).collect();
                flags.push(format!("--members {}", ids.join(",")));
            }
            let _ = writeln!(out, "LUMEN_FSD_PEER={}", flags.join(" "));
        }
        if self.control.to_string() != DEFAULT_CONTROL {
            let _ = writeln!(out, "LUMEN_FSD_CONTROL={}", self.control);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drop_in_the_unit_actually_reads_is_what_this_parses() {
        // The shape packages/lumen-fsd.spec ships and the pool workflow
        // writes: systemd EnvironmentFile syntax, comments and all.
        let config = PoolConfig::parse(
            "# written by the pool create workflow\n\
             LUMEN_FSD_BRICK=/dev/disk/by-id/nvme-eui.0001 /dev/disk/by-id/ata-slow.0002\n\
             LUMEN_FSD_NODE=1\n\
             LUMEN_FSD_PEER=--dial 10.10.0.1:7800\n",
        )
        .unwrap();
        assert_eq!(
            config.bricks,
            vec![
                PathBuf::from("/dev/disk/by-id/nvme-eui.0001"),
                PathBuf::from("/dev/disk/by-id/ata-slow.0002"),
            ]
        );
        assert_eq!(config.node, 1);
        assert_eq!(
            config.peers,
            vec![PeerRole::Dial("10.10.0.1:7800".parse().unwrap())]
        );
        assert_eq!(config.members, Vec::<u8>::new());
        // Not in the file, because the unit passes it: the default is the
        // unit's own constant, and loopback on purpose.
        assert_eq!(config.control.to_string(), DEFAULT_CONTROL);
    }

    #[test]
    fn a_quoted_value_is_a_value_not_a_path_with_quotes_in_it() {
        let config =
            PoolConfig::parse("LUMEN_FSD_BRICK=\"/var/lib/lumen/brick\"\nLUMEN_FSD_NODE='0'\n")
                .unwrap();
        assert_eq!(config.bricks, vec![PathBuf::from("/var/lib/lumen/brick")]);
        assert_eq!(config.node, 0);
        assert!(config.peers.is_empty(), "a one-node bring-up has no peers");
    }

    #[test]
    fn a_control_override_is_honored_because_the_tests_need_one() {
        let config = PoolConfig::parse(
            "LUMEN_FSD_BRICK=/b\nLUMEN_FSD_NODE=0\nLUMEN_FSD_CONTROL=127.0.0.1:7741\n",
        )
        .unwrap();
        assert_eq!(config.control.to_string(), "127.0.0.1:7741");
    }

    /// The writer and the parser are one contract: what render says, parse
    /// reads back identically — every mesh shape, both control shapes.
    #[test]
    fn what_render_writes_parse_reads_back_exactly() {
        let listen = PeerRole::Listen("10.10.0.1:7800".parse().unwrap());
        let dial_a = PeerRole::Dial("10.10.0.2:7800".parse().unwrap());
        let dial_b = PeerRole::Dial("10.10.0.3:7800".parse().unwrap());
        for (peers, members) in [
            (vec![listen], vec![0u8, 1]),
            (vec![dial_a], vec![0, 1]),
            // The mesh's middle seat: listening for the higher id,
            // dialing the lower.
            (vec![listen, dial_a], vec![0, 1, 2]),
            (vec![dial_a, dial_b], vec![0, 1, 2]),
            (Vec::new(), Vec::new()),
        ] {
            for control in [DEFAULT_CONTROL, "127.0.0.1:7741"] {
                let config = PoolConfig {
                    bricks: vec![
                        PathBuf::from("/dev/disk/by-id/nvme-eui.0001"),
                        PathBuf::from("/dev/disk/by-id/ata-slow.0002"),
                    ],
                    node: 1,
                    peers: peers.clone(),
                    members: members.clone(),
                    control: control.parse().unwrap(),
                };
                let text = config.render().unwrap();
                assert_eq!(
                    PoolConfig::parse(&text).unwrap(),
                    config,
                    "did not round-trip:\n{text}"
                );
            }
        }
        // Two listeners is a malformed file, not a topology.
        assert!(PoolConfig::parse(
            "LUMEN_FSD_BRICK=/b\nLUMEN_FSD_NODE=0\n\
             LUMEN_FSD_PEER=--listen 10.10.0.1:7800 --listen 10.10.0.2:7800\n"
        )
        .is_err());
    }

    /// A path with whitespace could not survive the file's own format, so
    /// the writer refuses it instead of writing a lie.
    #[test]
    fn render_refuses_what_the_format_cannot_carry() {
        let config = PoolConfig {
            bricks: vec![PathBuf::from("/dev/disk/by-id/has a space")],
            node: 0,
            peers: Vec::new(),
            members: Vec::new(),
            control: DEFAULT_CONTROL.parse().unwrap(),
        };
        assert!(config.render().is_err());
        let empty = PoolConfig {
            bricks: Vec::new(),
            node: 0,
            peers: Vec::new(),
            members: Vec::new(),
            control: DEFAULT_CONTROL.parse().unwrap(),
        };
        assert!(empty.render().is_err());
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
            "LUMEN_FSD_BRICK=\nLUMEN_FSD_NODE=0\n",
            "LUMEN_FSD_BRICK=/b\nLUMEN_FSD_NODE=0\nLUMEN_FSD_PEER=--sideways 1.2.3.4:7800\n",
            "LUMEN_FSD_BRICK=/b\nLUMEN_FSD_NODE=0\nLUMEN_FSD_PEER=--dial nonsense\n",
        ] {
            assert!(
                PoolConfig::parse(wrong).is_err(),
                "{wrong:?} was accepted anyway"
            );
        }
    }
}
