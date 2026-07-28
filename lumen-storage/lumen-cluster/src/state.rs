//! Observed state: what corosync and Pacemaker actually report.
//!
//! Deliberately independent of the definition — a node the operator removed
//! from the record still shows up here until the cluster agrees it is gone,
//! and a fenced node appears as exactly that. The console renders straight
//! from this, so every field a row needs is present in one round trip.
//!
//! Membership, quorum, and fencing are corosync and Pacemaker's answers, not
//! Lumen's: this struct is filled from `crm_mon --output-as=xml`,
//! `corosync-quorumtool`, and `corosync-cfgtool -s`, parsed in
//! `backend::cli`. Lumen presents; it does not second-guess.

use serde::{Deserialize, Serialize};

/// Everything observed about one cluster.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterState {
    pub name: String,
    pub quorum: QuorumState,
    pub nodes: Vec<NodeState>,
    pub fence_devices: Vec<FenceDeviceState>,
    /// The cluster's floating address, when its definition asked for one.
    ///
    /// Read from Pacemaker rather than inferred from the record, because the
    /// two can disagree and the disagreement is the whole point: a VIP the
    /// definition names and Pacemaker has stopped is an address nobody
    /// answers on, and a console that reported it from the definition alone
    /// would show a healthy cluster with an unreachable console.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vip: Option<VipState>,
}

impl ClusterState {
    pub fn node(&self, name: &str) -> Option<&NodeState> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// The fence device protecting one node, when Pacemaker has one.
    pub fn fence_for(&self, node: &str) -> Option<&FenceDeviceState> {
        self.fence_devices.iter().find(|d| d.target == node)
    }

    /// A peer that is unreachable and has not been fenced — the state the
    /// break-glass operation exists for, and the only state it is offered in.
    pub fn unfenced_unreachable(&self) -> Vec<&NodeState> {
        self.nodes.iter().filter(|n| n.unclean).collect()
    }
}

/// What votequorum says. `two_node` and `wait_for_all` are read back from the
/// running cluster rather than assumed from the definition, so a conf that
/// did not take effect is visible instead of papered over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuorumState {
    pub quorate: bool,
    pub votes: u32,
    pub expected_votes: u32,
    pub two_node: bool,
    pub wait_for_all: bool,
}

/// One member as the cluster sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    pub name: String,
    /// Corosync membership: the node is in the ring.
    pub online: bool,
    /// Maintenance: resources drain away and nothing new starts here.
    pub standby: bool,
    /// Pacemaker cannot say what state this node is in — lost and not yet
    /// successfully fenced. This is the flag HA waits on: "unclean" is never
    /// acted on, "fenced" is.
    pub unclean: bool,
    /// Per-ring link health from this node's point of view, one entry per
    /// knet link.
    pub rings: Vec<RingLink>,
}

/// One knet link on one node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingLink {
    /// The link number — 0 rides the Core network, 1 rides Management.
    pub link: u8,
    pub address: String,
    pub connected: bool,
}

/// One STONITH device as Pacemaker reports it, plus what Lumen remembers
/// about testing it. IPMI is the only fence path this appliance has, so a
/// failing device is cluster-health news, not a detail.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceDeviceState {
    /// Resource id, `fence-<node>`.
    pub device: String,
    /// The node this device powers off.
    pub target: String,
    /// The resource is running (its monitor — a BMC connectivity check — is
    /// passing somewhere).
    pub active: bool,
    /// The last monitor failed: the BMC did not answer. The one warning that
    /// must never be buried, because a partition that coincides with this has
    /// no automatic resolution.
    pub failed: bool,
    /// The last guarded live fence test in this direction, if one was ever
    /// run. `None` pins the persistent untested-fencing warning.
    pub last_test: Option<FenceTest>,
}

/// The cluster VIP resource, as Pacemaker reports it.
///
/// Its own type rather than a bool because "not running" has causes an
/// operator has to act on differently. A resource Pacemaker never started is
/// a different problem from one that started and failed, and one it calls
/// "not installed" is a third — the agent, or something the agent needs, is
/// missing from the image. The reason travels with the state so the console
/// can say which.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VipState {
    /// Resource id, `<cluster>-vip`.
    pub resource: String,
    /// Running somewhere.
    pub active: bool,
    /// The member holding the address, when one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Its last operation failed.
    pub failed: bool,
    /// Pacemaker will not act on it — unmanaged, or blocked behind a failed
    /// operation it cannot recover from without help.
    pub blocked: bool,
    /// What Pacemaker calls the state, verbatim: "Started", "Stopped".
    /// Passed through rather than mapped, because the operator will search
    /// for the string Pacemaker used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Why it is not running, in Pacemaker's own words — the `rc_text` of the
    /// last operation that failed, such as "Not installed" or "Timed Out".
    ///
    /// This is the field that turns "the address does not answer" into
    /// something an operator can act on. The role alone says "Stopped" for
    /// every cause there is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The outcome of a guarded live fence test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceTest {
    /// Unix seconds.
    pub at: u64,
    pub passed: bool,
}

/// This node's hostname, as corosync node names are matched against it.
/// Duplicated across the domain crates deliberately: six lines beat a
/// dependency between domains for a string.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "lumen".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ClusterState {
        ClusterState {
            name: "alpha".into(),
            quorum: QuorumState {
                quorate: true,
                votes: 2,
                expected_votes: 2,
                two_node: true,
                wait_for_all: true,
            },
            nodes: vec![
                NodeState {
                    name: "alpha-1".into(),
                    online: true,
                    ..NodeState::default()
                },
                NodeState {
                    name: "alpha-2".into(),
                    online: false,
                    unclean: true,
                    ..NodeState::default()
                },
            ],
            fence_devices: vec![FenceDeviceState {
                device: "fence-alpha-2".into(),
                target: "alpha-2".into(),
                active: true,
                ..FenceDeviceState::default()
            }],
            vip: None,
        }
    }

    #[test]
    fn the_state_answers_by_name() {
        let state = state();
        assert!(state.node("alpha-1").is_some());
        assert!(state.node("ghost").is_none());
        assert_eq!(
            state.fence_for("alpha-2").map(|d| d.device.as_str()),
            Some("fence-alpha-2")
        );
    }

    #[test]
    fn an_unclean_node_is_the_break_glass_condition() {
        let state = state();
        let stuck: Vec<&str> = state
            .unfenced_unreachable()
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(stuck, vec!["alpha-2"]);
    }

    #[test]
    fn a_device_that_was_never_tested_reads_as_untested_not_as_passed() {
        let state = state();
        assert!(state.fence_devices[0].last_test.is_none());
    }
}
