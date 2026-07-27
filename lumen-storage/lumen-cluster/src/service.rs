//! The clustering domain's one entry point.
//!
//! The control plane's handlers deserialize, call one method here, and
//! serialize the answer — no validation, no corosync, nothing above this
//! line. At this stage the service is read-only: it presents the environment
//! and its clusters. The workflows that change them arrive next.

use std::sync::Arc;

use serde::Serialize;

use crate::backend::ClusterBackend;
use crate::environment::{EnvironmentMembership, EnvironmentNode};
use crate::error::{ClusterError, Result};
use crate::model::Regime;
use crate::state::{hostname, ClusterState, FenceDeviceState, QuorumState, RingLink};

/// GET /api/environment. The whole environment in one answer: grouped by
/// cluster, then by node — the repo's "grouped by node" shape extended one
/// level upward, exactly as the comment on `lumen_zfs::PoolsResponse`
/// promised it would be.
#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentResponse {
    /// `None` on a node that never joined an environment — today's
    /// standalone appliance, which is a valid state, not an empty one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentView>,
    pub clusters: Vec<ClusterView>,
    /// Environment nodes not assigned to any cluster: valid standalone
    /// hypervisors, listed rather than hidden. A node with no environment
    /// appears here too, alone — the console renders one shape either way.
    pub unassigned: Vec<UnassignedNodeView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentView {
    pub id: String,
    /// The membership record's last-writer-wins counter.
    pub version: u64,
    pub nodes: usize,
}

/// Overall cluster health, derived here so the console and the API can never
/// disagree about what a card's pill says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterHealth {
    Ok,
    /// Running, but something needs attention: a node down or in standby, a
    /// ring degraded, a fence device failing or never tested.
    Degraded,
    /// Not quorate, or a member is lost and not yet fenced.
    Critical,
    /// The cluster could not be asked — presented honestly rather than
    /// dropped or guessed at.
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClusterView {
    pub name: String,
    pub regime: Regime,
    pub health: ClusterHealth,
    pub quorum: QuorumState,
    pub nodes: Vec<ClusterNodeView>,
    pub fence: FenceSummaryView,
    /// Why the cluster's state could not be read, when it could not. The
    /// nodes are then listed from the membership record with nothing claimed
    /// about them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClusterNodeView {
    pub node: String,
    pub online: bool,
    pub standby: bool,
    /// Lost and not yet fenced — the state HA waits on and the break-glass
    /// confirm exists for.
    pub unclean: bool,
    pub rings: Vec<RingLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fence: Option<FenceDeviceState>,
    /// From the membership record: where this node's own console answers,
    /// and the control plane version it last reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controlplane_version: Option<String>,
    /// This is the node answering the request.
    pub local: bool,
}

/// The fencing panel's one-line summary. IPMI is the only fence path, so a
/// failing or untested device is cluster-level news and counted here rather
/// than left for the operator to find in a table.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FenceSummaryView {
    pub devices: usize,
    pub healthy: usize,
    pub failed: usize,
    /// Devices never live-tested. Nonzero pins the persistent warning.
    pub untested: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnassignedNodeView {
    pub node: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controlplane_version: Option<String>,
    pub local: bool,
}

pub struct ClusterService {
    backend: Arc<dyn ClusterBackend>,
    node: String,
    controlplane_version: String,
}

impl ClusterService {
    pub fn new(backend: Arc<dyn ClusterBackend>, controlplane_version: &str) -> Self {
        ClusterService {
            backend,
            node: hostname(),
            controlplane_version: controlplane_version.to_string(),
        }
    }

    /// Pretend to be another node, for tests.
    pub fn with_node(mut self, node: impl Into<String>) -> Self {
        self.node = node.into();
        self
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    // --- reads ------------------------------------------------------------

    /// The whole environment: every cluster, every node, grouped.
    pub async fn environment(&self) -> Result<EnvironmentResponse> {
        let membership = self.backend.membership().await?;

        let Some(membership) = membership else {
            // No environment. The node still exists and the console still
            // renders it — as the one unassigned node of an environment of
            // nobody.
            return Ok(EnvironmentResponse {
                environment: None,
                clusters: Vec::new(),
                unassigned: vec![UnassignedNodeView {
                    node: self.node.clone(),
                    address: None,
                    controlplane_version: Some(self.controlplane_version.clone()),
                    local: true,
                }],
            });
        };

        let mut clusters = Vec::new();
        for name in membership.cluster_names() {
            clusters.push(self.cluster_view(&name, &membership).await);
        }

        let unassigned = membership
            .unassigned()
            .map(|node| self.unassigned_view(node))
            .collect();

        Ok(EnvironmentResponse {
            environment: Some(EnvironmentView {
                id: membership.id.clone(),
                version: membership.version,
                nodes: membership.nodes.len(),
            }),
            clusters,
            unassigned,
        })
    }

    /// One cluster, by name. The same view the environment answer carries —
    /// the detail page is the card, opened.
    pub async fn cluster(&self, name: &str) -> Result<ClusterView> {
        let membership = self.backend.membership().await?.ok_or_else(|| {
            ClusterError::NotFound(
                "This node has not joined an environment, so there are no clusters.".to_string(),
            )
        })?;
        if !membership.cluster_names().iter().any(|c| c == name) {
            return Err(ClusterError::NotFound(format!(
                "There is no cluster called \"{name}\" in this environment."
            )));
        }
        Ok(self.cluster_view(name, &membership).await)
    }

    // --- assembly ---------------------------------------------------------

    async fn cluster_view(&self, name: &str, membership: &EnvironmentMembership) -> ClusterView {
        match self.backend.cluster_state(name).await {
            Ok(state) => self.view_of(name, &state, membership),
            Err(err) => {
                // The cluster could not be asked. Say so, list its members
                // from the record, and claim nothing about them — an
                // unreachable cluster shown honestly beats one dropped from
                // the answer.
                tracing::warn!(cluster = name, "cluster state unavailable: {err}");
                let nodes = membership
                    .members_of(name)
                    .into_iter()
                    .map(|member| ClusterNodeView {
                        node: member.name.clone(),
                        online: false,
                        standby: false,
                        unclean: false,
                        rings: Vec::new(),
                        fence: None,
                        address: Some(member.address.clone()),
                        controlplane_version: Some(member.controlplane_version.clone()),
                        local: member.name == self.node,
                    })
                    .collect();
                ClusterView {
                    name: name.to_string(),
                    regime: Regime::of(membership.members_of(name).len()),
                    health: ClusterHealth::Unknown,
                    quorum: QuorumState::default(),
                    nodes,
                    fence: FenceSummaryView::default(),
                    error: Some(err.to_string()),
                }
            }
        }
    }

    fn view_of(
        &self,
        name: &str,
        state: &ClusterState,
        membership: &EnvironmentMembership,
    ) -> ClusterView {
        let nodes: Vec<ClusterNodeView> = state
            .nodes
            .iter()
            .map(|node| {
                let record = membership.node(&node.name);
                ClusterNodeView {
                    node: node.name.clone(),
                    online: node.online,
                    standby: node.standby,
                    unclean: node.unclean,
                    rings: node.rings.clone(),
                    fence: state.fence_for(&node.name).cloned(),
                    address: record.map(|r| r.address.clone()),
                    controlplane_version: record.map(|r| r.controlplane_version.clone()),
                    local: node.name == self.node,
                }
            })
            .collect();

        let fence = FenceSummaryView {
            devices: state.fence_devices.len(),
            healthy: state
                .fence_devices
                .iter()
                .filter(|d| d.active && !d.failed)
                .count(),
            failed: state.fence_devices.iter().filter(|d| d.failed).count(),
            untested: state
                .fence_devices
                .iter()
                .filter(|d| d.last_test.is_none())
                .count(),
        };

        ClusterView {
            name: name.to_string(),
            regime: if state.quorum.two_node {
                Regime::TwoNode
            } else {
                Regime::of(state.nodes.len())
            },
            health: health_of(state),
            quorum: state.quorum,
            nodes,
            fence,
            error: None,
        }
    }

    fn unassigned_view(&self, node: &EnvironmentNode) -> UnassignedNodeView {
        UnassignedNodeView {
            node: node.name.clone(),
            address: Some(node.address.clone()),
            controlplane_version: Some(node.controlplane_version.clone()),
            local: node.name == self.node,
        }
    }
}

/// The one derivation of a cluster's health pill. Critical is reserved for
/// "data is at stake now": lost quorum, or a member lost and not yet fenced.
/// Everything an operator should fix but can schedule is Degraded — including
/// a failing or untested fence device, because IPMI is the only fence path
/// this appliance has.
fn health_of(state: &ClusterState) -> ClusterHealth {
    if !state.quorum.quorate || state.nodes.iter().any(|n| n.unclean) {
        return ClusterHealth::Critical;
    }
    let ring_degraded = state
        .nodes
        .iter()
        .any(|n| n.rings.iter().any(|r| !r.connected));
    let fence_worry = state
        .fence_devices
        .iter()
        .any(|d| d.failed || !d.active || d.last_test.is_none());
    if state.nodes.iter().any(|n| !n.online || n.standby) || ring_degraded || fence_worry {
        return ClusterHealth::Degraded;
    }
    ClusterHealth::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    fn service(backend: MockBackend) -> ClusterService {
        ClusterService::new(Arc::new(backend), "0.3.0").with_node("alpha-1")
    }

    #[tokio::test]
    async fn a_node_with_no_environment_is_one_unassigned_node_not_an_error() {
        let service = service(MockBackend::appliance());
        let response = service.environment().await.unwrap();
        assert!(response.environment.is_none());
        assert!(response.clusters.is_empty());
        assert_eq!(response.unassigned.len(), 1);
        assert_eq!(response.unassigned[0].node, "alpha-1");
        assert!(response.unassigned[0].local);
    }

    #[tokio::test]
    async fn the_environment_answer_groups_by_cluster_then_by_node() {
        let service = service(MockBackend::environment());
        let response = service.environment().await.unwrap();

        let environment = response.environment.unwrap();
        assert_eq!(environment.nodes, 6);

        assert_eq!(response.clusters.len(), 2);
        assert_eq!(response.clusters[0].name, "alpha");
        assert_eq!(response.clusters[0].regime, Regime::TwoNode);
        assert_eq!(response.clusters[0].health, ClusterHealth::Ok);
        assert_eq!(response.clusters[0].nodes.len(), 2);
        assert_eq!(response.clusters[1].name, "beta");
        assert_eq!(response.clusters[1].regime, Regime::Quorum);
        assert_eq!(response.clusters[1].nodes.len(), 3);

        assert_eq!(response.unassigned.len(), 1);
        assert_eq!(response.unassigned[0].node, "spare-1");
        assert!(!response.unassigned[0].local);

        // The answering node is marked, once, in the right cluster.
        let locals: Vec<&str> = response
            .clusters
            .iter()
            .flat_map(|c| &c.nodes)
            .filter(|n| n.local)
            .map(|n| n.node.as_str())
            .collect();
        assert_eq!(locals, vec!["alpha-1"]);
    }

    #[tokio::test]
    async fn a_lost_unfenced_member_makes_the_cluster_critical() {
        let service = service(MockBackend::environment().with_partition("alpha", "alpha-2"));
        let cluster = service.cluster("alpha").await.unwrap();
        assert_eq!(cluster.health, ClusterHealth::Critical);
        let lost = cluster.nodes.iter().find(|n| n.node == "alpha-2").unwrap();
        assert!(lost.unclean && !lost.online);
    }

    #[tokio::test]
    async fn a_failing_fence_device_degrades_the_cluster_and_is_counted() {
        let service = service(MockBackend::environment().with_fence_failure("alpha", "alpha-2"));
        let cluster = service.cluster("alpha").await.unwrap();
        assert_eq!(cluster.health, ClusterHealth::Degraded);
        assert_eq!(cluster.fence.devices, 2);
        assert_eq!(cluster.fence.failed, 1);
        assert_eq!(cluster.fence.healthy, 1);
    }

    #[tokio::test]
    async fn untested_fencing_is_a_warning_that_does_not_go_away() {
        let service = service(MockBackend::environment().with_untested_fencing("alpha"));
        let cluster = service.cluster("alpha").await.unwrap();
        assert_eq!(cluster.health, ClusterHealth::Degraded);
        assert_eq!(cluster.fence.untested, 2);
    }

    #[tokio::test]
    async fn an_unreachable_cluster_is_presented_not_dropped() {
        let backend = MockBackend::environment().with_unreachable_cluster("beta");
        let service = service(backend);
        let response = service.environment().await.unwrap();
        assert_eq!(response.clusters.len(), 2, "beta must still be listed");
        let beta = &response.clusters[1];
        assert_eq!(beta.health, ClusterHealth::Unknown);
        assert!(beta.error.is_some());
        // Its members are listed from the record, with nothing claimed.
        assert_eq!(beta.nodes.len(), 3);
        assert!(beta.nodes.iter().all(|n| !n.online && !n.unclean));
    }

    #[tokio::test]
    async fn asking_for_a_cluster_that_is_not_there_names_the_problem() {
        let joined = service(MockBackend::environment());
        let err = joined.cluster("gamma").await.unwrap_err();
        assert!(matches!(err, ClusterError::NotFound(_)), "{err}");

        let standalone = service(MockBackend::appliance());
        let err = standalone.cluster("alpha").await.unwrap_err();
        assert!(err.to_string().contains("environment"), "{err}");
    }

    #[tokio::test]
    async fn a_standby_node_reads_as_degraded_not_broken() {
        let service = service(MockBackend::environment().with_standby("beta", "beta-2"));
        let cluster = service.cluster("beta").await.unwrap();
        assert_eq!(cluster.health, ClusterHealth::Degraded);
        assert!(cluster.quorum.quorate);
    }
}
