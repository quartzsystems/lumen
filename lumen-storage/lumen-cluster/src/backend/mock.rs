//! An in-memory node and its clusters: partitions, fence outcomes, and every
//! local operation recorded instead of run — nothing touches the machine the
//! tests run on.
//!
//! Compiled unconditionally — the control plane's integration tests build
//! their `AppState` around this backend, and a `cfg(test)` item would be
//! invisible to them.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::{ClusterBackend, LocalPreflight};
use crate::environment::{ClusterRecord, EnvironmentMembership, EnvironmentNode};
use crate::error::{ClusterError, Result};
use crate::state::{
    ClusterState, FenceDeviceState, FenceTest, NodeState, QuorumState, RingLink, VipState,
};

#[derive(Debug, Default)]
struct Inner {
    clusters: HashMap<String, ClusterState>,
    preflight: LocalPreflight,
    fail_next: Option<String>,
    fail_fence: Option<String>,
    fail_standby: Option<String>,
    // What the node was asked to do, for assertions.
    written_configs: Vec<(String, String)>,
    /// Every firewall adjustment, in order: (core, management, open).
    cluster_ports: Vec<(String, Option<String>, bool)>,
    /// Every migration-listener adjustment, in order: open or close.
    migration_listener: Vec<bool>,
    stack_enabled: bool,
    config_removed: bool,
    properties: Vec<(String, String)>,
    vips: Vec<(String, String)>,
    fence_devices_created: Vec<(crate::topology::FenceDevice, String)>,
    fenced: Vec<String>,
    /// Hard power actions asked for, in order: `(node, action)`.
    powered: Vec<(String, crate::backend::HardPower)>,
    confirmed_dead: Vec<String>,
    standby_calls: Vec<(String, bool)>,
    reloads: usize,
    delay_updates: Vec<(String, u32)>,
    fence_devices_removed: Vec<String>,
    cleanups: Vec<String>,
    resources_removed: Vec<String>,
    /// The cause behind a stopped resource is still there, so a cleanup
    /// re-probes and the same failure comes straight back. Off by default:
    /// the ordinary case is an operator who fixed the cause first.
    cause_unfixed: bool,
}

pub struct MockBackend {
    inner: Mutex<Inner>,
}

impl MockBackend {
    /// A fresh appliance: no clusters, a healthy clock — today's single node.
    pub fn appliance() -> Self {
        MockBackend {
            inner: Mutex::new(Inner {
                preflight: LocalPreflight {
                    time_synchronized: true,
                    time_offset_ms: Some(2),
                    already_clustered: false,
                },
                ..Inner::default()
            }),
        }
    }

    /// The acceptance scenario's cluster states: a healthy two-node `alpha`
    /// and three-node `beta`, fence devices tested. Pair it with
    /// [`environment_membership`] seeded into the service's store.
    pub fn environment() -> Self {
        let backend = MockBackend::appliance();
        {
            let mut inner = backend.inner.lock().unwrap();
            inner.clusters.insert(
                "alpha".into(),
                healthy_cluster("alpha", &["alpha-1", "alpha-2"], true),
            );
            inner.clusters.insert(
                "beta".into(),
                healthy_cluster("beta", &["beta-1", "beta-2", "beta-3"], false),
            );
        }
        backend
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

    /// The cluster VIP never came up, with Pacemaker's own words for why.
    ///
    /// The state the recovery exists for: `rc_text` is what the agent's
    /// return code meant, and "Not installed" is the one an operator meets
    /// when the agent is missing something it shells out to.
    pub fn with_stopped_vip(self, cluster: &str, reason: &str) -> Self {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(state) = inner.clusters.get_mut(cluster) {
                if let Some(vip) = state.vip.as_mut() {
                    vip.active = false;
                    vip.node = None;
                    vip.role = Some("Stopped".into());
                    vip.reason = Some(reason.to_string());
                }
            }
        }
        self
    }

    /// Whatever stopped the address is still wrong, so clearing the recorded
    /// failure changes nothing — the re-probe fails the same way. This is the
    /// operator who asked for a recovery before fixing the cause.
    pub fn with_unfixed_cause(self) -> Self {
        self.inner.lock().unwrap().cause_unfixed = true;
        self
    }

    /// Make a cluster stop answering, as a cluster whose members are all
    /// down would: the record still names it, its state cannot be read.
    pub fn with_unreachable_cluster(self, cluster: &str) -> Self {
        self.inner.lock().unwrap().clusters.remove(cluster);
        self
    }

    /// Shape this node's own preflight answers.
    pub fn with_local_preflight(self, preflight: LocalPreflight) -> Self {
        self.inner.lock().unwrap().preflight = preflight;
        self
    }

    /// Make a cluster observable, the way corosync forming does on a real
    /// node. `MockPeers` calls this when every member has started.
    pub fn register_cluster(&self, state: ClusterState) {
        let mut inner = self.inner.lock().unwrap();
        inner.clusters.insert(state.name.clone(), state);
    }

    /// The next call fails with this reason, once.
    pub fn fail_next(&self, reason: impl Into<String>) {
        self.inner.lock().unwrap().fail_next = Some(reason.into());
    }

    /// The next live fence fails with this reason, once — targeted, because
    /// a fence test reads cluster state first and `fail_next` would be spent
    /// on the read instead of the fence.
    pub fn fail_fence(&self, reason: impl Into<String>) {
        self.inner.lock().unwrap().fail_fence = Some(reason.into());
    }

    /// The next standby call fails with this reason, once — targeted for the
    /// same reason as `fail_fence`: taking a node out of service reads the
    /// cluster first, and `fail_next` would be spent on that read.
    pub fn fail_standby(&self, reason: impl Into<String>) {
        self.inner.lock().unwrap().fail_standby = Some(reason.into());
    }

    // --- assertion accessors ------------------------------------------------

    pub fn written_configs(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().written_configs.clone()
    }

    /// Every firewall adjustment, in order: (core, management, open).
    pub fn cluster_ports(&self) -> Vec<(String, Option<String>, bool)> {
        self.inner.lock().unwrap().cluster_ports.clone()
    }

    /// Every migration-listener adjustment, in order: open or close.
    pub fn migration_listener(&self) -> Vec<bool> {
        self.inner.lock().unwrap().migration_listener.clone()
    }

    pub fn stack_enabled(&self) -> bool {
        self.inner.lock().unwrap().stack_enabled
    }

    pub fn config_removed(&self) -> bool {
        self.inner.lock().unwrap().config_removed
    }

    pub fn properties_set(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().properties.clone()
    }

    pub fn vips_created(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().vips.clone()
    }

    pub fn fence_devices_created(&self) -> Vec<(crate::topology::FenceDevice, String)> {
        self.inner.lock().unwrap().fence_devices_created.clone()
    }

    pub fn fenced(&self) -> Vec<String> {
        self.inner.lock().unwrap().fenced.clone()
    }

    /// Hard power actions asked for, in order — how a test sees that the
    /// console reached for the fence device rather than the node's own OS.
    pub fn powered(&self) -> Vec<(String, crate::backend::HardPower)> {
        self.inner.lock().unwrap().powered.clone()
    }

    pub fn confirmed_dead(&self) -> Vec<String> {
        self.inner.lock().unwrap().confirmed_dead.clone()
    }

    /// Every standby call in order, `(node, standby)` — so a test can assert
    /// that leaving service and returning to it both reached Pacemaker.
    pub fn standby_calls(&self) -> Vec<(String, bool)> {
        self.inner.lock().unwrap().standby_calls.clone()
    }

    pub fn reloads(&self) -> usize {
        self.inner.lock().unwrap().reloads
    }

    pub fn delay_updates(&self) -> Vec<(String, u32)> {
        self.inner.lock().unwrap().delay_updates.clone()
    }

    pub fn fence_devices_removed(&self) -> Vec<String> {
        self.inner.lock().unwrap().fence_devices_removed.clone()
    }

    /// Every resource whose failures were cleared, in order.
    pub fn cleanups(&self) -> Vec<String> {
        self.inner.lock().unwrap().cleanups.clone()
    }

    /// Every resource taken out of the CIB, in order.
    pub fn resources_removed(&self) -> Vec<String> {
        self.inner.lock().unwrap().resources_removed.clone()
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

/// The acceptance scenario's membership record: a two-node cluster, a
/// three-node cluster, and one unassigned node. Seed it into a service's
/// store next to [`MockBackend::environment`].
pub fn environment_membership() -> EnvironmentMembership {
    EnvironmentMembership {
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
        clusters: Vec::new(),
    }
}

/// A membership record for tests that need a particular shape.
pub fn membership_of(nodes: &[(&str, Option<&str>)]) -> EnvironmentMembership {
    EnvironmentMembership {
        id: "env-mock".into(),
        version: 1,
        nodes: nodes
            .iter()
            .map(|(name, cluster)| node_record(name, "192.168.10.1", *cluster))
            .collect(),
        clusters: Vec::new(),
    }
}

/// A membership record whose clusters carry stored definitions too.
pub fn with_cluster_record(
    mut membership: EnvironmentMembership,
    record: ClusterRecord,
) -> EnvironmentMembership {
    membership.clusters.push(record);
    membership
}

fn node_record(name: &str, address: &str, cluster: Option<&str>) -> EnvironmentNode {
    EnvironmentNode {
        name: name.into(),
        address: address.into(),
        controlplane_version: "0.3.0".into(),
        cluster: cluster.map(str::to_string),
        maintenance: None,
    }
}

/// A cluster the moment it forms: every member online, quorate, no fence
/// devices yet — exactly what a just-created cluster looks like before the
/// fencing stage runs.
pub fn formed_cluster(name: &str, nodes: &[&str]) -> ClusterState {
    let mut state = healthy_cluster(name, nodes, nodes.len() == 2);
    state.fence_devices.clear();
    state
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
                bmc_address: None,
                bmc_username: None,
                reason: None,
            })
            .collect(),
        // A healthy fixture's address is up on the first member. Tests that
        // care about a stopped one set it themselves.
        vip: Some(VipState {
            resource: format!("{name}-vip"),
            active: true,
            node: nodes.first().map(|node| (*node).to_string()),
            failed: false,
            blocked: false,
            role: Some("Started".into()),
            reason: None,
        }),
    }
}

#[async_trait]
impl ClusterBackend for MockBackend {
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

    async fn local_preflight(&self) -> Result<LocalPreflight> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        Ok(self.inner.lock().unwrap().preflight.clone())
    }

    async fn write_cluster_config(&self, conf: &str, authkey: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        inner
            .written_configs
            .push((conf.to_string(), authkey.to_string()));
        inner.preflight.already_clustered = true;
        Ok(())
    }

    async fn set_cluster_ports(
        &self,
        core: &str,
        management: Option<&str>,
        open: bool,
    ) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner.lock().unwrap().cluster_ports.push((
            core.to_string(),
            management.map(str::to_string),
            open,
        ));
        Ok(())
    }

    async fn set_migration_listener(&self, open: bool) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner.lock().unwrap().migration_listener.push(open);
        Ok(())
    }

    async fn enable_stack(&self) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner.lock().unwrap().stack_enabled = true;
        Ok(())
    }

    async fn disable_stack(&self) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner.lock().unwrap().stack_enabled = false;
        Ok(())
    }

    async fn remove_cluster_config(&self) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        inner.config_removed = true;
        inner.preflight.already_clustered = false;
        Ok(())
    }

    async fn set_pacemaker_properties(&self, properties: &[(String, String)]) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner
            .lock()
            .unwrap()
            .properties
            .extend(properties.iter().cloned());
        Ok(())
    }

    async fn create_vip(
        &self,
        cluster: &str,
        address: std::net::Ipv4Addr,
        prefix: u8,
    ) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        // Observable straight away, the way a CIB write shows up in the next
        // crm_mon — so a caller that reads the address back after creating it
        // sees a resource rather than nothing.
        if let Some(state) = inner.clusters.get_mut(cluster) {
            state.vip = Some(VipState {
                resource: format!("{cluster}-vip"),
                active: true,
                node: state.nodes.first().map(|node| node.name.clone()),
                failed: false,
                blocked: false,
                role: Some("Started".into()),
                reason: None,
            });
        }
        inner
            .vips
            .push((cluster.to_string(), format!("{address}/{prefix}")));
        Ok(())
    }

    async fn remove_resource(&self, resource: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        // Pacemaker stops knowing about it, so a read after this sees what a
        // real one would: a cluster with no address resource at all.
        for state in inner.clusters.values_mut() {
            if state.vip.as_ref().is_some_and(|v| v.resource == resource) {
                state.vip = None;
            }
        }
        inner.resources_removed.push(resource.to_string());
        Ok(())
    }

    async fn cleanup_resource(&self, resource: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        let cause_unfixed = inner.cause_unfixed;
        // The recorded failure is forgotten and the resource probed again, so
        // a read after this sees what a real one would. Whether it then comes
        // up is not the cleanup's doing — it is whether the cause was fixed.
        for state in inner.clusters.values_mut() {
            let Some(vip) = state.vip.as_mut() else {
                continue;
            };
            if vip.resource != resource {
                continue;
            }
            if cause_unfixed {
                continue;
            }
            vip.failed = false;
            vip.blocked = false;
            vip.reason = None;
            vip.active = true;
            vip.role = Some("Started".into());
            vip.node = state.nodes.first().map(|node| node.name.clone());
        }
        inner.cleanups.push(resource.to_string());
        Ok(())
    }

    async fn create_fence_device(
        &self,
        device: &crate::topology::FenceDevice,
        password: &str,
    ) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        // The device becomes observable on whichever simulated cluster the
        // target belongs to, the way a CIB write shows up in the next
        // crm_mon — active, unfailed, and never live-tested.
        for state in inner.clusters.values_mut() {
            if state.nodes.iter().any(|n| n.name == device.target) {
                state.fence_devices.push(FenceDeviceState {
                    device: device.id.clone(),
                    target: device.target.clone(),
                    active: true,
                    failed: false,
                    last_test: None,
                    bmc_address: None,
                    bmc_username: None,
                    reason: None,
                });
            }
        }
        inner
            .fence_devices_created
            .push((device.clone(), password.to_string()));
        Ok(())
    }

    async fn fence_node(&self, target: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(reason) = inner.fail_fence.take() {
            return Err(ClusterError::Backend(anyhow::anyhow!(reason)));
        }
        inner.fenced.push(target.to_string());
        Ok(())
    }

    async fn power_node(&self, target: &str, action: crate::backend::HardPower) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(reason) = inner.fail_fence.take() {
            return Err(ClusterError::Backend(anyhow::anyhow!(reason)));
        }
        inner.powered.push((target.to_string(), action));
        Ok(())
    }

    async fn set_standby(&self, target: &str, standby: bool) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(reason) = inner.fail_standby.take() {
            return Err(ClusterError::Backend(anyhow::anyhow!(reason)));
        }
        // Pacemaker's own view changes, so a read after this call sees what a
        // real one would.
        for state in inner.clusters.values_mut() {
            if let Some(node) = state.nodes.iter_mut().find(|n| n.name == target) {
                node.standby = standby;
            }
        }
        inner.standby_calls.push((target.to_string(), standby));
        Ok(())
    }

    async fn confirm_node_dead(&self, target: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        // Pacemaker stops calling the node unclean: the operator has vouched
        // for what fencing could not prove. It stays offline.
        for state in inner.clusters.values_mut() {
            if let Some(node) = state.nodes.iter_mut().find(|n| n.name == target) {
                node.unclean = false;
            }
        }
        inner.confirmed_dead.push(target.to_string());
        Ok(())
    }

    async fn authkey(&self) -> Result<String> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        // The key the simulated members were created with — the newcomer
        // has to be handed the running cluster's, not a fresh one.
        Ok(self
            .inner
            .lock()
            .unwrap()
            .written_configs
            .last()
            .map(|(_, key)| key.clone())
            .unwrap_or_else(|| "mock-cluster-key".to_string()))
    }

    async fn reload_corosync(&self) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner.lock().unwrap().reloads += 1;
        Ok(())
    }

    async fn update_fence_delay(&self, device: &str, delay_secs: u32) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner
            .lock()
            .unwrap()
            .delay_updates
            .push((device.to_string(), delay_secs));
        Ok(())
    }

    async fn remove_fence_device(&self, device: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        for state in inner.clusters.values_mut() {
            state.fence_devices.retain(|d| d.device != device);
        }
        inner.fence_devices_removed.push(device.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_appliance_scenario_is_a_node_that_never_clustered() {
        let backend = MockBackend::appliance();
        assert!(backend.cluster_state("alpha").await.is_err());
        let preflight = backend.local_preflight().await.unwrap();
        assert!(preflight.time_synchronized && !preflight.already_clustered);
    }

    #[tokio::test]
    async fn the_environment_scenario_matches_the_acceptance_shape() {
        let backend = MockBackend::environment();
        let membership = environment_membership();
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
    async fn preparing_a_node_is_visible_to_its_own_preflight() {
        let backend = MockBackend::appliance();
        backend
            .write_cluster_config("totem {}", "key")
            .await
            .unwrap();
        assert!(backend.local_preflight().await.unwrap().already_clustered);
        backend.remove_cluster_config().await.unwrap();
        assert!(!backend.local_preflight().await.unwrap().already_clustered);
    }

    #[tokio::test]
    async fn a_created_device_is_observable_and_a_confirmation_clears_unclean() {
        let backend = MockBackend::environment().with_partition("alpha", "alpha-2");
        backend
            .create_fence_device(
                &crate::topology::FenceDevice {
                    id: "fence-extra".into(),
                    target: "alpha-1".into(),
                    bmc_address: "10.20.0.9".into(),
                    bmc_username: "ADMIN".into(),
                    delay_base_secs: 0,
                    bmc_cipher: None,
                },
                "pw",
            )
            .await
            .unwrap();
        let alpha = backend.cluster_state("alpha").await.unwrap();
        assert!(alpha
            .fence_devices
            .iter()
            .any(|d| d.device == "fence-extra"));

        assert_eq!(alpha.unfenced_unreachable().len(), 1);
        backend.confirm_node_dead("alpha-2").await.unwrap();
        let alpha = backend.cluster_state("alpha").await.unwrap();
        // Vouched for: no longer unclean, still offline.
        assert!(alpha.unfenced_unreachable().is_empty());
        assert!(!alpha.node("alpha-2").unwrap().online);
        assert_eq!(backend.confirmed_dead(), vec!["alpha-2"]);
    }

    #[tokio::test]
    async fn a_failure_is_injected_once() {
        let backend = MockBackend::environment();
        backend.fail_next("the ring is on fire");
        assert!(backend.cluster_state("alpha").await.is_err());
        assert!(backend.cluster_state("alpha").await.is_ok());
    }
}
