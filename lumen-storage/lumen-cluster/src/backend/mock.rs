//! An in-memory environment: multiple clusters, partitions, and fence
//! outcomes, none of it touching the machine the tests run on.
//!
//! Compiled unconditionally — the control plane's integration tests build
//! their `AppState` around this backend, and a `cfg(test)` item would be
//! invisible to them.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::ClusterBackend;
use crate::environment::{EnvironmentMembership, EnvironmentNode};
use crate::error::{ClusterError, Result};
use crate::state::{ClusterState, FenceDeviceState, FenceTest, NodeState, QuorumState, RingLink};

#[derive(Debug, Default)]
struct Inner {
    membership: Option<EnvironmentMembership>,
    clusters: HashMap<String, ClusterState>,
    fail_next: Option<String>,
}

pub struct MockBackend {
    inner: Mutex<Inner>,
}

impl MockBackend {
    /// A fresh appliance: no environment, no clusters — today's single node.
    pub fn appliance() -> Self {
        MockBackend {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The full acceptance scenario: one environment holding a two-node
    /// cluster (`alpha`, preferred node `alpha-1`), a three-node cluster
    /// (`beta`), and one unassigned node (`spare-1`). Every fence device
    /// healthy and tested. Local node is whatever the service says it is —
    /// the scenario includes `alpha-1` so tests usually claim to be it.
    pub fn environment() -> Self {
        let membership = EnvironmentMembership {
            id: "env-mock".into(),
            version: 7,
            nodes: vec![
                node_record("alpha-1", "192.168.10.1", Some("alpha")),
                node_record("alpha-2", "192.168.10.2", Some("alpha")),
                node_record("beta-1", "192.168.20.1", Some("beta")),
                node_record("beta-2", "192.168.20.2", Some("beta")),
                node_record("beta-3", "192.168.20.3", Some("beta")),
                node_record("spare-1", "192.168.10.9", None),
            ],
        };

        let mut clusters = HashMap::new();
        clusters.insert(
            "alpha".into(),
            healthy_cluster("alpha", &["alpha-1", "alpha-2"], true),
        );
        clusters.insert(
            "beta".into(),
            healthy_cluster("beta", &["beta-1", "beta-2", "beta-3"], false),
        );

        MockBackend {
            inner: Mutex::new(Inner {
                membership: Some(membership),
                clusters,
                fail_next: None,
            }),
        }
    }

    /// Partition a node away: offline, unclean (lost and not yet fenced),
    /// its vote gone. Quorum recomputes the way votequorum would.
    pub fn with_partition(self, cluster: &str, node: &str) -> Self {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(state) = inner.clusters.get_mut(cluster) {
                if let Some(member) = state.nodes.iter_mut().find(|n| n.name == node) {
                    member.online = false;
                    member.unclean = true;
                    for ring in &mut member.rings {
                        ring.connected = false;
                    }
                }
                let votes = state.nodes.iter().filter(|n| n.online).count() as u32;
                state.quorum.votes = votes;
                state.quorum.quorate = if state.quorum.two_node {
                    votes >= 1
                } else {
                    votes * 2 > state.quorum.expected_votes
                };
            }
        }
        self
    }

    /// The BMC stopped answering: the device's monitor fails. Combined with
    /// `with_partition` this is the no-automatic-resolution state the
    /// break-glass confirm exists for.
    pub fn with_fence_failure(self, cluster: &str, node: &str) -> Self {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(state) = inner.clusters.get_mut(cluster) {
                if let Some(device) = state.fence_devices.iter_mut().find(|d| d.target == node) {
                    device.active = false;
                    device.failed = true;
                }
            }
        }
        self
    }

    /// Forget the fence tests, as a cluster whose operator skipped them.
    pub fn with_untested_fencing(self, cluster: &str) -> Self {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(state) = inner.clusters.get_mut(cluster) {
                for device in &mut state.fence_devices {
                    device.last_test = None;
                }
            }
        }
        self
    }

    /// Put a node in standby, as maintenance would.
    pub fn with_standby(self, cluster: &str, node: &str) -> Self {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(state) = inner.clusters.get_mut(cluster) {
                if let Some(member) = state.nodes.iter_mut().find(|n| n.name == node) {
                    member.standby = true;
                }
            }
        }
        self
    }

    /// Replace the membership record, for tests that need a shape the named
    /// scenarios do not cover.
    pub fn with_membership(self, membership: EnvironmentMembership) -> Self {
        self.inner.lock().unwrap().membership = Some(membership);
        self
    }

    /// Make a cluster stop answering, as a cluster whose members are all
    /// down would: the record still names it, its state cannot be read.
    pub fn with_unreachable_cluster(self, cluster: &str) -> Self {
        self.inner.lock().unwrap().clusters.remove(cluster);
        self
    }

    /// The next call fails with this reason, once.
    pub fn fail_next(&self, reason: impl Into<String>) {
        self.inner.lock().unwrap().fail_next = Some(reason.into());
    }

    fn take_failure(&self) -> Option<ClusterError> {
        self.inner
            .lock()
            .unwrap()
            .fail_next
            .take()
            .map(|reason| ClusterError::Backend(anyhow::anyhow!(reason)))
    }
}

fn node_record(name: &str, address: &str, cluster: Option<&str>) -> EnvironmentNode {
    EnvironmentNode {
        name: name.into(),
        address: address.into(),
        controlplane_version: "0.3.0".into(),
        cluster: cluster.map(str::to_string),
    }
}

fn healthy_cluster(name: &str, nodes: &[&str], two_node: bool) -> ClusterState {
    ClusterState {
        name: name.into(),
        quorum: QuorumState {
            quorate: true,
            votes: nodes.len() as u32,
            expected_votes: nodes.len() as u32,
            two_node,
            wait_for_all: two_node,
        },
        nodes: nodes
            .iter()
            .enumerate()
            .map(|(i, node)| NodeState {
                name: (*node).into(),
                online: true,
                standby: false,
                unclean: false,
                rings: vec![
                    RingLink {
                        link: 0,
                        address: format!("10.10.0.{}", i + 1),
                        connected: true,
                    },
                    RingLink {
                        link: 1,
                        address: format!("192.168.10.{}", i + 1),
                        connected: true,
                    },
                ],
            })
            .collect(),
        fence_devices: nodes
            .iter()
            .map(|node| FenceDeviceState {
                device: format!("fence-{node}"),
                target: (*node).into(),
                active: true,
                failed: false,
                last_test: Some(FenceTest {
                    at: 1_753_000_000,
                    passed: true,
                }),
            })
            .collect(),
    }
}

#[async_trait]
impl ClusterBackend for MockBackend {
    async fn membership(&self) -> Result<Option<EnvironmentMembership>> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        Ok(self.inner.lock().unwrap().membership.clone())
    }

    async fn cluster_state(&self, name: &str) -> Result<ClusterState> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner
            .lock()
            .unwrap()
            .clusters
            .get(name)
            .cloned()
            .ok_or_else(|| {
                ClusterError::NotFound(format!("There is no cluster called \"{name}\"."))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_appliance_scenario_is_a_node_that_never_joined() {
        let backend = MockBackend::appliance();
        assert_eq!(backend.membership().await.unwrap(), None);
        assert!(backend.cluster_state("alpha").await.is_err());
    }

    #[tokio::test]
    async fn the_environment_scenario_matches_the_acceptance_shape() {
        let backend = MockBackend::environment();
        let membership = backend.membership().await.unwrap().unwrap();
        assert_eq!(membership.nodes.len(), 6);
        assert_eq!(membership.cluster_names(), vec!["alpha", "beta"]);
        assert_eq!(membership.unassigned().count(), 1);

        let alpha = backend.cluster_state("alpha").await.unwrap();
        assert!(alpha.quorum.two_node && alpha.quorum.wait_for_all);
        let beta = backend.cluster_state("beta").await.unwrap();
        assert!(!beta.quorum.two_node && !beta.quorum.wait_for_all);
    }

    #[tokio::test]
    async fn a_partition_recomputes_quorum_the_way_votequorum_would() {
        let backend = MockBackend::environment().with_partition("alpha", "alpha-2");
        let alpha = backend.cluster_state("alpha").await.unwrap();
        // two_node: the survivor keeps quorum.
        assert!(alpha.quorum.quorate);
        assert_eq!(alpha.unfenced_unreachable().len(), 1);

        let backend = MockBackend::environment()
            .with_partition("beta", "beta-2")
            .with_partition("beta", "beta-3");
        let beta = backend.cluster_state("beta").await.unwrap();
        // One of three is a minority: not quorate.
        assert!(!beta.quorum.quorate);
    }

    #[tokio::test]
    async fn a_fence_failure_is_visible_on_the_device() {
        let backend = MockBackend::environment().with_fence_failure("alpha", "alpha-2");
        let alpha = backend.cluster_state("alpha").await.unwrap();
        let device = alpha.fence_for("alpha-2").unwrap();
        assert!(device.failed && !device.active);
    }

    #[tokio::test]
    async fn a_failure_is_injected_once() {
        let backend = MockBackend::environment();
        backend.fail_next("the ring is on fire");
        assert!(backend.membership().await.is_err());
        assert!(backend.membership().await.is_ok());
    }
}
