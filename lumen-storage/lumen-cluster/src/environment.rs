//! The environment: one administrative trust domain over every node, whether
//! or not any of them is in a cluster yet.
//!
//! The environment is Lumen's own construct — corosync is never involved.
//! What holds it together is this membership record: a small document every
//! node's control plane keeps a copy of and gossips to its peers over the
//! environment's mTLS channel. A node that has never joined anything holds no
//! record at all, and that is the ordinary single-appliance case, not an
//! error.
//!
//! ## The reconciliation rule
//!
//! Two copies of the record are reconciled by a version counter:
//! last-writer-wins, where "last" is the higher `version`. Every mutation —
//! a join, a cluster assignment, a removal — happens on one node, bumps the
//! counter, and gossips outward, so at this scale (five nodes per cluster, a
//! handful of clusters) genuine concurrent writes are rare and losing one is
//! a re-run of a workflow, not data loss — the record holds definitions,
//! never the data itself. A tie on the counter is broken by comparing the
//! serialized records; arbitrary, but every node computes the same answer,
//! which is the property that actually matters. This is deliberately not a
//! CRDT and not Raft: the record is too small and changes too rarely to be
//! worth either.

use serde::{Deserialize, Serialize};

use crate::error::{ClusterError, Result};
use crate::model::ClusterDefinition;
use crate::networks::ClusterNetworks;

/// How long a join token is honoured, in seconds. Long enough to walk to the
/// other console and paste it; short enough that a token left on a whiteboard
/// is a curiosity rather than a credential.
pub const TOKEN_TTL_SECS: u64 = 900;

/// One node's standing in the environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentNode {
    /// The node's hostname.
    pub name: String,
    /// The management address its control plane answers on — where peers
    /// reach it for gossip and for proxied operations.
    pub address: String,
    /// The control plane version it reported when last heard from. Cluster
    /// preflight refuses a version mismatch, so it is recorded here rather
    /// than asked for again.
    pub controlplane_version: String,
    /// The cluster this node belongs to, or `None` for an unassigned node —
    /// a valid standalone hypervisor, not a node in a broken state.
    pub cluster: Option<String>,
}

/// One node's seat on a replicated volume: which pool on that node carries
/// the backing zvol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSeat {
    pub node: String,
    pub pool: String,
}

/// One replicated volume as the record remembers it. The model lives here —
/// below `lumen-drbd` in the dependency order — because volume placement is
/// membership's business too: a node holding replicas cannot leave its
/// cluster, and every console must describe every volume whether or not its
/// cluster answers. Everything operational (rendering, sizing, lifecycle,
/// observed state) is `lumen-drbd`'s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeRecord {
    pub name: String,
    /// The usable bytes the operator asked for.
    pub size_bytes: u64,
    /// The byte-exact backing zvol size — usable bytes plus DRBD's internal
    /// metadata, rounded to the volblocksize — computed once, before anything
    /// was created on any node, so every replica is identical by
    /// construction.
    pub zvol_bytes: u64,
    /// The TCP port replication rides on the Core network, allocated from
    /// the cluster's range.
    pub port: u16,
    /// The DRBD device minor: the volume appears as `/dev/drbd<minor>`.
    pub minor: u32,
    pub replicas: Vec<VolumeSeat>,
}

impl VolumeRecord {
    pub fn seat(&self, node: &str) -> Option<&VolumeSeat> {
        self.replicas.iter().find(|s| s.node == node)
    }
}

/// One cluster's stored shape: the definition the operator built and the
/// networks it rides. Replicated with the membership record so every node's
/// console can describe every cluster, reachable or not; the assignment
/// authority stays `EnvironmentNode::cluster`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterRecord {
    pub definition: ClusterDefinition,
    pub networks: ClusterNetworks,
    /// The last guarded live fence test per direction, keyed by target node.
    /// Lumen state, not Pacemaker's — crm_mon cannot say whether a device was
    /// ever proven, only whether its monitor passes — and it rides the
    /// membership record so a test run from one console clears the untested
    /// warning on every console.
    #[serde(default)]
    pub fence_tests: std::collections::BTreeMap<String, crate::state::FenceTest>,
    /// The cluster's replicated volumes. Definitions only — observed
    /// replication state is read live by `lumen-drbd` and never stored.
    #[serde(default)]
    pub volumes: Vec<VolumeRecord>,
}

impl ClusterRecord {
    pub fn new(definition: ClusterDefinition, networks: ClusterNetworks) -> Self {
        ClusterRecord {
            definition,
            networks,
            fence_tests: Default::default(),
            volumes: Vec::new(),
        }
    }

    pub fn volume(&self, name: &str) -> Option<&VolumeRecord> {
        self.volumes.iter().find(|v| v.name == name)
    }
}

/// The replicated membership record. See the module documentation for how two
/// copies reconcile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentMembership {
    /// Opaque identifier minted when the first node bootstraps the
    /// environment. Two records with different ids are different
    /// environments and are never merged.
    pub id: String,
    /// The last-writer-wins counter. Every mutation bumps it by one.
    pub version: u64,
    pub nodes: Vec<EnvironmentNode>,
    /// The clusters' definitions, riding the same record — one replicated
    /// document, one counter, one reconciliation rule.
    #[serde(default)]
    pub clusters: Vec<ClusterRecord>,
}

impl EnvironmentMembership {
    pub fn node(&self, name: &str) -> Option<&EnvironmentNode> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// The clusters this record knows about, sorted, each name once.
    pub fn cluster_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .nodes
            .iter()
            .filter_map(|n| n.cluster.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Nodes not assigned to any cluster.
    pub fn unassigned(&self) -> impl Iterator<Item = &EnvironmentNode> {
        self.nodes.iter().filter(|n| n.cluster.is_none())
    }

    /// Members of one cluster, in record order.
    pub fn members_of(&self, cluster: &str) -> Vec<&EnvironmentNode> {
        self.nodes
            .iter()
            .filter(|n| n.cluster.as_deref() == Some(cluster))
            .collect()
    }

    /// The stored shape of one cluster, when the record carries it.
    pub fn cluster_record(&self, cluster: &str) -> Option<&ClusterRecord> {
        self.clusters
            .iter()
            .find(|record| record.definition.name == cluster)
    }

    /// The reconciliation rule, as a pure function both gossip directions
    /// call: the record with the higher version wins whole. Records from
    /// different environments are never merged — the local one is kept,
    /// because adopting a stranger's membership over gossip would be a
    /// takeover, not a sync.
    pub fn reconcile(local: EnvironmentMembership, remote: EnvironmentMembership) -> Self {
        if local.id != remote.id {
            return local;
        }
        match local.version.cmp(&remote.version) {
            std::cmp::Ordering::Less => remote,
            std::cmp::Ordering::Greater => local,
            std::cmp::Ordering::Equal => {
                // Same version, possibly different content: a genuine
                // concurrent write. Both sides must pick the same winner
                // without talking, so compare the serialized records.
                let a = serde_json::to_string(&local).unwrap_or_default();
                let b = serde_json::to_string(&remote).unwrap_or_default();
                if a >= b {
                    local
                } else {
                    remote
                }
            }
        }
    }
}

// --- the join token ---------------------------------------------------------

/// A join token, as minted on an environment node and pasted into the console
/// of the node that wants in. One string carries everything the joining side
/// needs: where to call, which token this is, the secret that proves the
/// paste, and the fingerprint of the certificate the issuer will present —
/// so the joining node verifies **who it is talking to before anything
/// secret moves**, kubeadm-style. The secret is stored hashed on the issuer
/// and the whole token is one-time and short-lived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinToken {
    /// `host:port` of the node that minted it.
    pub issuer: String,
    pub id: String,
    pub secret: String,
    /// SHA-256 of the DER certificate the issuer serves, hex.
    pub fingerprint: String,
}

const TOKEN_PREFIX: &str = "lumen-join/v1/";

impl JoinToken {
    /// The one string an operator copies. Base64 over JSON rather than
    /// separator games, because an address already contains `:`.
    pub fn encode(&self) -> String {
        use base64::Engine;
        let json = serde_json::to_string(self).expect("token serializes");
        format!(
            "{TOKEN_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        )
    }

    pub fn decode(text: &str) -> Result<JoinToken> {
        use base64::Engine;
        let refused = || {
            ClusterError::Conflict(
                "That is not a join token. Mint one on a node that is already in the \
                 environment and paste the whole string."
                    .to_string(),
            )
        };
        let body = text.trim().strip_prefix(TOKEN_PREFIX).ok_or_else(refused)?;
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| refused())?;
        serde_json::from_slice(&json).map_err(|_| refused())
    }
}

/// A fresh token secret or environment id: 32 random bytes, hex.
pub fn random_hex() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of a token secret, hex — what the issuer stores instead of the
/// secret, so reading the token file is not the same as holding a token.
pub fn hash_secret(secret: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(secret.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// --- the environment CA -----------------------------------------------------

/// The CA's parameters, rebuilt the same way every time. The subject is a
/// pure function of the environment id, which is what lets a node
/// reconstruct the issuer from the id and the stored key alone — chain
/// verification cares about the subject and the signature, not the serial.
fn ca_params(environment_id: &str) -> Result<rcgen::CertificateParams> {
    let mut params =
        rcgen::CertificateParams::new(Vec::<String>::new()).map_err(ClusterError::backend)?;
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        format!(
            "Lumen environment {} CA",
            &environment_id[..environment_id.len().min(12)]
        ),
    );
    Ok(params)
}

/// Mint the environment's CA. Every member holds it, key included, because a
/// token can be minted on any node and the issuer is the one who signs.
pub fn mint_ca(environment_id: &str) -> Result<(String, String)> {
    let key = rcgen::KeyPair::generate().map_err(ClusterError::backend)?;
    let cert = ca_params(environment_id)?
        .self_signed(&key)
        .map_err(ClusterError::backend)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Issue a node's serving certificate, signed by the environment CA. The SANs
/// carry the name and the management address, because that address is how
/// peers dial and verify it.
pub fn issue_node_certificate(
    environment_id: &str,
    ca_key_pem: &str,
    node: &str,
    address: &str,
) -> Result<(String, String)> {
    let ca_key = rcgen::KeyPair::from_pem(ca_key_pem).map_err(ClusterError::backend)?;
    let ca_cert = ca_params(environment_id)?
        .self_signed(&ca_key)
        .map_err(ClusterError::backend)?;

    let host = address
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(address);
    let mut names = vec![node.to_string(), "localhost".to_string()];
    if host != node {
        names.push(host.to_string());
    }
    let mut params = rcgen::CertificateParams::new(names).map_err(ClusterError::backend)?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, node);
    let key = rcgen::KeyPair::generate().map_err(ClusterError::backend)?;
    let cert = params
        .signed_by(&key, &ca_cert, &ca_key)
        .map_err(ClusterError::backend)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// SHA-256 over a certificate's DER bytes, hex — what the peer client
/// computes off the wire when it checks a pinned join.
pub fn der_fingerprint(der: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(der);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The same fingerprint, from the PEM form the store holds.
pub fn pem_fingerprint(pem: &str) -> Result<String> {
    use base64::Engine;
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|_| {
            ClusterError::Backend(anyhow::anyhow!("that is not a certificate in PEM form"))
        })?;
    Ok(der_fingerprint(&der))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, version: u64, nodes: &[(&str, Option<&str>)]) -> EnvironmentMembership {
        EnvironmentMembership {
            id: id.into(),
            version,
            nodes: nodes
                .iter()
                .map(|(name, cluster)| EnvironmentNode {
                    name: (*name).into(),
                    address: "192.168.10.1".into(),
                    controlplane_version: "0.3.0".into(),
                    cluster: cluster.map(str::to_string),
                })
                .collect(),
            clusters: Vec::new(),
        }
    }

    #[test]
    fn a_token_round_trips_through_its_pasted_form() {
        let token = JoinToken {
            issuer: "192.168.10.1:8443".into(),
            id: "tok-1".into(),
            secret: random_hex(),
            fingerprint: "ab".repeat(32),
        };
        let pasted = token.encode();
        assert!(pasted.starts_with("lumen-join/v1/"));
        assert_eq!(JoinToken::decode(&pasted).unwrap(), token);
        // Whitespace from a clumsy paste is forgiven; garbage is refused
        // with a sentence, not a parse error.
        assert_eq!(JoinToken::decode(&format!("  {pasted}\n")).unwrap(), token);
        let err = JoinToken::decode("not a token").unwrap_err();
        assert!(err.to_string().contains("join token"), "{err}");
    }

    #[test]
    fn the_secret_hash_is_stable_and_not_the_secret() {
        let secret = random_hex();
        let hash = hash_secret(&secret);
        assert_eq!(hash, hash_secret(&secret));
        assert_ne!(hash, secret);
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn the_ca_signs_a_node_certificate_that_fingerprints() {
        let (ca_pem, ca_key) = mint_ca("abcdef0123456789").unwrap();
        assert!(ca_pem.contains("BEGIN CERTIFICATE"));
        let (cert, key) =
            issue_node_certificate("abcdef0123456789", &ca_key, "alpha-2", "192.168.10.2:8443")
                .unwrap();
        assert!(cert.contains("BEGIN CERTIFICATE"));
        assert!(key.contains("PRIVATE KEY"));
        let print = pem_fingerprint(&cert).unwrap();
        assert_eq!(print.len(), 64);
        // The fingerprint is of the certificate, not of the encoding around
        // it: the CA and the leaf must not collide.
        assert_ne!(print, pem_fingerprint(&ca_pem).unwrap());
    }

    #[test]
    fn the_higher_version_wins_whole() {
        let old = record("env", 3, &[("a-1", None)]);
        let new = record("env", 4, &[("a-1", Some("alpha"))]);
        assert_eq!(
            EnvironmentMembership::reconcile(old.clone(), new.clone()),
            new
        );
        assert_eq!(EnvironmentMembership::reconcile(new.clone(), old), new);
    }

    /// Both nodes must pick the same winner without talking to each other —
    /// the rule is only a rule if it commutes.
    #[test]
    fn a_tie_resolves_the_same_way_from_both_sides() {
        let a = record("env", 5, &[("a-1", Some("alpha"))]);
        let b = record("env", 5, &[("a-1", Some("beta"))]);
        let from_a = EnvironmentMembership::reconcile(a.clone(), b.clone());
        let from_b = EnvironmentMembership::reconcile(b, a);
        assert_eq!(from_a, from_b);
    }

    #[test]
    fn a_record_from_another_environment_is_never_adopted() {
        let mine = record("env-one", 1, &[("a-1", None)]);
        let theirs = record("env-two", 99, &[("x-1", None)]);
        assert_eq!(EnvironmentMembership::reconcile(mine.clone(), theirs), mine);
    }

    #[test]
    fn reconcile_is_idempotent() {
        let a = record("env", 2, &[("a-1", None), ("a-2", Some("alpha"))]);
        assert_eq!(EnvironmentMembership::reconcile(a.clone(), a.clone()), a);
    }

    #[test]
    fn the_record_answers_the_grouping_questions_the_console_asks() {
        let membership = record(
            "env",
            1,
            &[
                ("alpha-1", Some("alpha")),
                ("alpha-2", Some("alpha")),
                ("beta-1", Some("beta")),
                ("spare-1", None),
            ],
        );
        assert_eq!(membership.cluster_names(), vec!["alpha", "beta"]);
        assert_eq!(
            membership
                .unassigned()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>(),
            vec!["spare-1"]
        );
        assert_eq!(membership.members_of("alpha").len(), 2);
        assert!(membership.node("beta-1").is_some());
        assert!(membership.node("ghost").is_none());
    }
}
