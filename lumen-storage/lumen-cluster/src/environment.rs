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
//! a re-run of a workflow, not data loss — the record holds membership, never
//! volumes or machines. A tie on the counter is broken by comparing the
//! serialized records; arbitrary, but every node computes the same answer,
//! which is the property that actually matters. This is deliberately not a
//! CRDT and not Raft: the record is too small and changes too rarely to be
//! worth either.

use serde::{Deserialize, Serialize};

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
        }
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
