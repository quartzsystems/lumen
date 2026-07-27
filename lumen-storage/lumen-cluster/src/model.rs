//! What a cluster is: its name, its members, its preferred node, and the
//! per-member reach into the BMC that fences it.
//!
//! Pure data and pure rules — nothing here talks to corosync, and nothing here
//! is observed state (that is `state.rs`). The topology engine in
//! `topology.rs` renders this model into corosync.conf, fence devices, and
//! Pacemaker properties; `validate.rs` is what decides whether a definition is
//! buildable at all.

use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

/// The fewest nodes a cluster can have. Two is a real regime of its own —
/// witness-less, with `two_node`, `wait_for_all`, and a fence-race delay —
/// not a degenerate case of three.
pub const MIN_CLUSTER_NODES: usize = 2;

/// The most nodes a cluster can have. Five is a product decision, not a
/// corosync limit: the appliance is a hyperconverged cluster, not a fabric.
pub const MAX_CLUSTER_NODES: usize = 5;

/// The UDP port corosync's first knet link listens on. The second ring is
/// this plus one, which is corosync's own default spacing.
pub const COROSYNC_PORT: u16 = 5405;

/// The delay handed to the fence device that targets the preferred node of a
/// two-node cluster, in seconds. The delay protects its *target*: a device
/// with a delay is a node the peer must wait to kill, so the preferred node
/// gets its shot in first and survives a partition where both sides are alive
/// and racing to fence.
pub const FENCE_RACE_DELAY_SECS: u32 = 10;

/// Which of the two topology regimes a member count lands in. There is
/// exactly one boundary and everything special about clustering sits on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Regime {
    /// Two nodes, witness-less: corosync `two_node` + `wait_for_all`,
    /// asymmetric fence delays, DRBD `fencing resource-and-stonith`.
    TwoNode,
    /// Three to five nodes: plain majority quorum, `no-quorum-policy=stop`,
    /// DRBD quorum for three-replica volumes. None of the two-node mechanisms.
    Quorum,
}

impl Regime {
    pub fn of(nodes: usize) -> Regime {
        if nodes <= 2 {
            Regime::TwoNode
        } else {
            Regime::Quorum
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Regime::TwoNode => "two_node",
            Regime::Quorum => "quorum",
        }
    }
}

/// How a member's BMC is reached for fencing. The credential is deliberately
/// not here: a password rides in a request once, is handed to the fence
/// device's configuration, and is never part of the model this crate shows
/// back to anyone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BmcConfig {
    /// Address of the BMC's own network interface — not an address of the
    /// node. A BMC answers when the node cannot, which is the whole point.
    pub address: String,
    pub username: String,
}

/// One member of a cluster, as the definition sees it: a name and where its
/// two rings and its BMC live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberNode {
    /// The node's hostname, which is also its corosync node name.
    pub name: String,
    /// Ring 0 — the Core network address. Storage replication and cluster
    /// heartbeat share this dedicated subnet.
    pub ring0: Ipv4Addr,
    /// Ring 1 — the Management network address, usually the address the node
    /// already answers the console on.
    pub ring1: Ipv4Addr,
    pub bmc: BmcConfig,
}

/// A cluster as the operator defined it: the input to the topology engine.
///
/// This is desired state. What corosync and Pacemaker actually report is
/// `state::ClusterState`, and the two are deliberately separate types — a
/// definition exists before the first `pcs cluster setup` and survives a node
/// being down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterDefinition {
    pub name: String,
    pub nodes: Vec<MemberNode>,
    /// The node that should win a two-node fence race. Meaningless at three
    /// nodes and above, where majority quorum decides who survives — the
    /// validator refuses it there rather than letting it look load-bearing.
    pub preferred_node: Option<String>,
}

impl ClusterDefinition {
    pub fn regime(&self) -> Regime {
        Regime::of(self.nodes.len())
    }

    pub fn node(&self, name: &str) -> Option<&MemberNode> {
        self.nodes.iter().find(|n| n.name == name)
    }

    pub fn node_names(&self) -> Vec<String> {
        self.nodes.iter().map(|n| n.name.clone()).collect()
    }
}

/// A usable cluster name: what corosync's `cluster_name` accepts without
/// quoting games, and what a DRBD resource prefix and a firewall zone can be
/// built from later. Lowercase letters, digits, and hyphens, starting with a
/// letter — the same shape as a hostname label, because it ends up inside
/// several files that were designed around one.
pub fn valid_cluster_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A usable node name: a hostname, because that is what corosync node names
/// and Pacemaker node attributes are matched against. Same label rules as a
/// cluster name, but longer, and dots are allowed for a fully qualified name.
pub fn valid_node_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, last_octet: u8) -> MemberNode {
        MemberNode {
            name: name.into(),
            ring0: Ipv4Addr::new(10, 10, 0, last_octet),
            ring1: Ipv4Addr::new(192, 168, 10, last_octet),
            bmc: BmcConfig {
                address: format!("10.20.0.{last_octet}"),
                username: "ADMIN".into(),
            },
        }
    }

    #[test]
    fn the_regime_boundary_is_between_two_and_three() {
        assert_eq!(Regime::of(2), Regime::TwoNode);
        assert_eq!(Regime::of(3), Regime::Quorum);
        assert_eq!(Regime::of(5), Regime::Quorum);
    }

    #[test]
    fn a_cluster_name_is_a_hostname_label() {
        assert!(valid_cluster_name("alpha"));
        assert!(valid_cluster_name("rack-2"));
        assert!(!valid_cluster_name(""));
        assert!(!valid_cluster_name("Alpha"));
        assert!(!valid_cluster_name("2fast"));
        assert!(!valid_cluster_name("trailing-"));
        assert!(!valid_cluster_name("has space"));
        assert!(!valid_cluster_name(&"a".repeat(33)));
    }

    #[test]
    fn a_node_name_is_a_hostname_dots_included() {
        assert!(valid_node_name("alpha-1"));
        assert!(valid_node_name("alpha-1.lab.example.com"));
        assert!(!valid_node_name(""));
        assert!(!valid_node_name("alpha..1"));
        assert!(!valid_node_name("-alpha"));
        assert!(!valid_node_name("alpha_1"));
    }

    #[test]
    fn a_definition_answers_for_its_own_members() {
        let definition = ClusterDefinition {
            name: "alpha".into(),
            nodes: vec![member("alpha-1", 1), member("alpha-2", 2)],
            preferred_node: Some("alpha-1".into()),
        };
        assert_eq!(definition.regime(), Regime::TwoNode);
        assert!(definition.node("alpha-2").is_some());
        assert!(definition.node("beta-1").is_none());
        assert_eq!(definition.node_names(), vec!["alpha-1", "alpha-2"]);
    }
}
