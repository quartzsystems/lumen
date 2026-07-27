//! The environment's state on disk: the membership record, the CA and this
//! node's identity, and the outstanding join tokens.
//!
//! Everything lives under `<state_dir>/environment/`, which exists only once
//! this node has bootstrapped or joined an environment — a standalone node
//! has no directory and no state, which is what makes "standalone" a real
//! state rather than an empty document.
//!
//! Writes are a temp file and a rename, so a crash mid-write leaves the old
//! record rather than half of the new one. The private material (CA key,
//! node key) is written `0600`; the directory itself is inside the control
//! plane's `0700` state directory either way.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::environment::EnvironmentMembership;
use crate::error::{ClusterError, Result};

/// One outstanding join token, as stored on the node that minted it. The
/// secret itself is never on disk — only its hash — so reading the file is
/// not the same as holding a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinTokenRecord {
    pub id: String,
    /// SHA-256 of the token's secret half, hex.
    pub secret_hash: String,
    /// Unix seconds. A token outlives its TTL only as a line in this file,
    /// and validation refuses it.
    pub expires_at: u64,
}

/// One machine's replicated definition: the domain document and the node it
/// was defined on — its home, which is what tells the HA manager whose
/// machines a dead node was carrying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDefinition {
    pub vmid: u32,
    /// The node the machine runs on — updated every time the definition
    /// replicates, so a migration or an HA restart moves it.
    pub node: String,
    pub xml: String,
}

/// The environment's credential material, held by every member: any node can
/// mint a token and sign a join, so every node carries the CA.
#[derive(Debug, Clone)]
pub struct Identity {
    pub ca_pem: String,
    pub ca_key_pem: String,
    pub node_cert_pem: String,
    pub node_key_pem: String,
}

pub struct EnvironmentStore {
    root: PathBuf,
}

impl EnvironmentStore {
    pub fn new(state_dir: &Path) -> Self {
        EnvironmentStore {
            root: state_dir.join("environment"),
        }
    }

    /// Whether this node holds any environment state at all.
    pub fn exists(&self) -> bool {
        self.root.join("membership.json").exists()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // --- membership -------------------------------------------------------

    pub fn load_membership(&self) -> Result<Option<EnvironmentMembership>> {
        self.read_json("membership.json")
    }

    pub fn save_membership(&self, membership: &EnvironmentMembership) -> Result<()> {
        self.write_json("membership.json", membership, false)
    }

    // --- identity ---------------------------------------------------------

    pub fn load_identity(&self) -> Result<Option<Identity>> {
        let ca_pem = match self.read_text("ca.pem")? {
            Some(pem) => pem,
            None => return Ok(None),
        };
        let missing = |what: &str| {
            ClusterError::Backend(anyhow::anyhow!(
                "the environment state under {} is incomplete: {what} is missing",
                self.root.display()
            ))
        };
        Ok(Some(Identity {
            ca_pem,
            ca_key_pem: self
                .read_text("ca-key.pem")?
                .ok_or_else(|| missing("ca-key.pem"))?,
            node_cert_pem: self
                .read_text("node.pem")?
                .ok_or_else(|| missing("node.pem"))?,
            node_key_pem: self
                .read_text("node-key.pem")?
                .ok_or_else(|| missing("node-key.pem"))?,
        }))
    }

    pub fn save_identity(&self, identity: &Identity) -> Result<()> {
        self.write_text("ca.pem", &identity.ca_pem, false)?;
        self.write_text("ca-key.pem", &identity.ca_key_pem, true)?;
        self.write_text("node.pem", &identity.node_cert_pem, false)?;
        self.write_text("node-key.pem", &identity.node_key_pem, true)
    }

    /// Where the node's serving certificate and key live, for the control
    /// plane's TLS listener to pick up in place of its self-signed pair.
    pub fn node_cert_path(&self) -> PathBuf {
        self.root.join("node.pem")
    }

    pub fn node_key_path(&self) -> PathBuf {
        self.root.join("node-key.pem")
    }

    // --- join tokens ------------------------------------------------------

    pub fn load_tokens(&self) -> Result<Vec<JoinTokenRecord>> {
        Ok(self.read_json("tokens.json")?.unwrap_or_default())
    }

    pub fn save_tokens(&self, tokens: &[JoinTokenRecord]) -> Result<()> {
        self.write_json("tokens.json", &tokens.to_vec(), false)
    }

    // --- replicated machine definitions -------------------------------------

    fn definitions_dir(&self) -> PathBuf {
        self.root.join("definitions")
    }

    /// Keep one machine's definition, so an HA restart can define the
    /// machine on a survivor — libvirt on a dead node cannot be asked for
    /// it. Overwrites: the newest definition is the only one worth having.
    pub fn save_definition(&self, definition: &StoredDefinition) -> Result<()> {
        let dir = self.definitions_dir();
        std::fs::create_dir_all(&dir).map_err(|err| {
            ClusterError::Backend(anyhow::anyhow!("could not create {}: {err}", dir.display()))
        })?;
        let vmid = definition.vmid;
        let path = dir.join(format!("{vmid}.json"));
        let tmp = dir.join(format!("{vmid}.json.tmp"));
        let text = serde_json::to_string_pretty(definition).map_err(ClusterError::backend)?;
        std::fs::write(&tmp, text).map_err(|err| {
            ClusterError::Backend(anyhow::anyhow!("could not write {}: {err}", tmp.display()))
        })?;
        std::fs::rename(&tmp, &path).map_err(|err| {
            ClusterError::Backend(anyhow::anyhow!(
                "could not move {} into place: {err}",
                path.display()
            ))
        })
    }

    /// Forget one. A definition that is already gone is the goal state, not
    /// an error.
    pub fn remove_definition(&self, vmid: u32) -> Result<()> {
        match std::fs::remove_file(self.definitions_dir().join(format!("{vmid}.json"))) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(ClusterError::Backend(anyhow::anyhow!(
                "could not remove the stored definition of {vmid}: {err}"
            ))),
        }
    }

    /// Every stored definition — the HA manager's restart inventory.
    pub fn definitions(&self) -> Result<Vec<StoredDefinition>> {
        let dir = self.definitions_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(ClusterError::Backend(anyhow::anyhow!(
                    "could not read {}: {err}",
                    dir.display()
                )))
            }
        };
        let mut definitions: Vec<StoredDefinition> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|err| {
                ClusterError::Backend(anyhow::anyhow!("could not read {}: {err}", path.display()))
            })?;
            let definition = serde_json::from_str(&text).map_err(|err| {
                ClusterError::Backend(anyhow::anyhow!(
                    "{} does not hold what it should: {err}",
                    path.display()
                ))
            })?;
            definitions.push(definition);
        }
        definitions.sort_by_key(|d| d.vmid);
        Ok(definitions)
    }

    // --- plumbing ---------------------------------------------------------

    fn read_text(&self, name: &str) -> Result<Option<String>> {
        match std::fs::read_to_string(self.root.join(name)) {
            Ok(text) => Ok(Some(text)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(ClusterError::Backend(anyhow::anyhow!(
                "could not read {}: {err}",
                self.root.join(name).display()
            ))),
        }
    }

    fn read_json<T: serde::de::DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let Some(text) = self.read_text(name)? else {
            return Ok(None);
        };
        // A corrupt record is an error that names the file, never a silent
        // fall back to "no environment" — losing the environment quietly
        // would be far worse than an error message.
        serde_json::from_str(&text).map(Some).map_err(|err| {
            ClusterError::Backend(anyhow::anyhow!(
                "{} does not hold what it should: {err}",
                self.root.join(name).display()
            ))
        })
    }

    fn write_text(&self, name: &str, text: &str, secret: bool) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(|err| {
            ClusterError::Backend(anyhow::anyhow!(
                "could not create {}: {err}",
                self.root.display()
            ))
        })?;
        let path = self.root.join(name);
        let tmp = self.root.join(format!("{name}.tmp"));
        std::fs::write(&tmp, text).map_err(|err| {
            ClusterError::Backend(anyhow::anyhow!("could not write {}: {err}", tmp.display()))
        })?;
        #[cfg(unix)]
        if secret {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        #[cfg(not(unix))]
        let _ = secret;
        std::fs::rename(&tmp, &path).map_err(|err| {
            ClusterError::Backend(anyhow::anyhow!(
                "could not move {} into place: {err}",
                path.display()
            ))
        })
    }

    fn write_json<T: Serialize>(&self, name: &str, value: &T, secret: bool) -> Result<()> {
        let text = serde_json::to_string_pretty(value).map_err(ClusterError::backend)?;
        self.write_text(name, &text, secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::EnvironmentNode;

    fn store() -> (EnvironmentStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "lumen-cluster-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (EnvironmentStore::new(&dir), dir)
    }

    fn membership() -> EnvironmentMembership {
        EnvironmentMembership {
            id: "env-test".into(),
            version: 3,
            nodes: vec![EnvironmentNode {
                name: "alpha-1".into(),
                address: "192.168.10.1".into(),
                controlplane_version: "0.3.0".into(),
                cluster: None,
            }],
            clusters: Vec::new(),
        }
    }

    #[test]
    fn a_node_with_no_state_is_standalone_not_broken() {
        let (store, dir) = store();
        assert!(!store.exists());
        assert_eq!(store.load_membership().unwrap(), None);
        assert!(store.load_identity().unwrap().is_none());
        assert_eq!(store.load_tokens().unwrap(), Vec::new());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_record_round_trips_and_survives_a_reload() {
        let (store, dir) = store();
        store.save_membership(&membership()).unwrap();
        assert!(store.exists());
        assert_eq!(store.load_membership().unwrap(), Some(membership()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_record_is_an_error_that_names_the_file() {
        let (store, dir) = store();
        store.save_membership(&membership()).unwrap();
        std::fs::write(store.root().join("membership.json"), "not json").unwrap();
        let err = store.load_membership().unwrap_err();
        assert!(err.to_string().contains("membership.json"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn identity_is_all_or_nothing() {
        let (store, dir) = store();
        store
            .save_identity(&Identity {
                ca_pem: "CA".into(),
                ca_key_pem: "CAKEY".into(),
                node_cert_pem: "CERT".into(),
                node_key_pem: "KEY".into(),
            })
            .unwrap();
        let identity = store.load_identity().unwrap().unwrap();
        assert_eq!(identity.ca_pem, "CA");
        assert_eq!(identity.node_key_pem, "KEY");

        // A half-written identity is reported, not returned.
        std::fs::remove_file(store.root().join("node-key.pem")).unwrap();
        let err = store.load_identity().unwrap_err();
        assert!(err.to_string().contains("node-key.pem"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tokens_round_trip_with_hashes_only() {
        let (store, dir) = store();
        let tokens = vec![JoinTokenRecord {
            id: "tok-1".into(),
            secret_hash: "ab".repeat(32),
            expires_at: 1_753_000_000,
        }];
        store.save_tokens(&tokens).unwrap();
        assert_eq!(store.load_tokens().unwrap(), tokens);
        std::fs::remove_dir_all(&dir).ok();
    }
}
