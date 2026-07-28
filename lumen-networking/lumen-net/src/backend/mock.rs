//! In-memory backend. Deterministic, no bus, no clock of its own.
//!
//! Compiled unconditionally (see the note in `backend/mod.rs`) so
//! lumen-controlplane's integration tests can inject it exactly the way
//! `tests/auth_flow.rs` injects its mock realm.
//!
//! It models enough of NetworkManager to exercise the whole staged-apply
//! flow: profiles exist or do not, devices activate and deactivate, and a
//! checkpoint restores the snapshot it was taken from. It does NOT model
//! NM's automatic rollback timer — a test drives that explicitly with
//! [`MockBackend::expire_checkpoints`], because a test that waits sixty
//! seconds for a timer is a test nobody runs.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::backend::{CheckpointId, NetworkBackend};
use crate::error::{NetError, Result};
use crate::model::{IpConfig, LinkKind};
use crate::plan::{ConnectionSpec, Op};
use crate::state::{LinkState, ObservedLink, ObservedState};

#[derive(Debug)]
struct Checkpoint {
    id: CheckpointId,
    /// The state to restore. NM's checkpoint is out-of-process for exactly
    /// this reason: it survives the control plane dying mid-apply.
    snapshot: ObservedState,
    rollback_secs: u32,
}

#[derive(Debug, Default)]
struct Inner {
    state: ObservedState,
    checkpoints: Vec<Checkpoint>,
    applied: Vec<Op>,
    next_id: u32,
    /// When set, the next `apply` fails after this many ops — the partial
    /// apply the checkpoint exists to survive.
    fail_apply_after: Option<usize>,
}

pub struct MockBackend {
    inner: Mutex<Inner>,
}

impl MockBackend {
    /// A backend that starts from `state`.
    pub fn new(state: ObservedState) -> Self {
        Self {
            inner: Mutex::new(Inner {
                state,
                ..Inner::default()
            }),
        }
    }

    /// Two NICs, nic0 up with a static management address on it, plus the
    /// loopback every Linux box has — the shape of a freshly installed
    /// appliance before the bridge conversion.
    pub fn appliance() -> Self {
        Self::new(ObservedState {
            node: "lumen".into(),
            links: vec![
                ObservedLink {
                    name: "lo".into(),
                    kind: LinkKind::Other,
                    state: LinkState::Activated,
                    mtu: Some(65536),
                    addresses: vec!["127.0.0.1/8".into()],
                    ..ObservedLink::default()
                },
                ObservedLink {
                    name: "nic0".into(),
                    altname: Some("enp1s0".into()),
                    kind: LinkKind::Ethernet,
                    state: LinkState::Activated,
                    managed: true,
                    carrier: true,
                    perm_mac: Some("52:54:00:aa:bb:00".into()),
                    mac: Some("52:54:00:aa:bb:00".into()),
                    speed_mbps: Some(1000),
                    duplex: Some(crate::model::Duplex::Full),
                    mtu: Some(1500),
                    addresses: vec!["192.168.10.5/24".into()],
                    gateway: Some("192.168.10.1".into()),
                    dns: vec!["9.9.9.9".into()],
                    ip: IpConfig::Static {
                        cidr: "192.168.10.5/24".into(),
                        gateway: "192.168.10.1".into(),
                        dns: vec!["9.9.9.9".into()],
                    },
                    connection_uuid: Some("uuid-management".into()),
                    connection_id: Some("management".into()),
                    ..ObservedLink::default()
                },
                ObservedLink {
                    name: "nic1".into(),
                    altname: Some("enp2s0".into()),
                    kind: LinkKind::Ethernet,
                    state: LinkState::Disconnected,
                    managed: true,
                    carrier: true,
                    perm_mac: Some("52:54:00:aa:bb:01".into()),
                    mac: Some("52:54:00:aa:bb:01".into()),
                    speed_mbps: Some(1000),
                    duplex: Some(crate::model::Duplex::Full),
                    mtu: Some(1500),
                    ..ObservedLink::default()
                },
            ],
        })
    }

    /// The current in-memory state, for assertions.
    pub fn state(&self) -> ObservedState {
        self.inner.lock().unwrap().state.clone()
    }

    /// Every op the backend has been handed, in order.
    pub fn applied(&self) -> Vec<Op> {
        self.inner.lock().unwrap().applied.clone()
    }

    /// Make the next `apply` fail after `n` ops, leaving the state half-done.
    pub fn fail_apply_after(&self, n: usize) {
        self.inner.lock().unwrap().fail_apply_after = Some(n);
    }

    /// Stand in for NetworkManager's rollback timer firing: restore and drop
    /// every outstanding checkpoint. Returns how many fired.
    pub fn expire_checkpoints(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let count = inner.checkpoints.len();
        if let Some(first) = inner.checkpoints.first() {
            inner.state = first.snapshot.clone();
        }
        inner.checkpoints.clear();
        count
    }

    /// The rollback window a checkpoint was created with.
    pub fn checkpoint_window(&self, id: &CheckpointId) -> Option<u32> {
        self.inner
            .lock()
            .unwrap()
            .checkpoints
            .iter()
            .find(|c| &c.id == id)
            .map(|c| c.rollback_secs)
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::appliance()
    }
}

/// Recompute every controller's port list from the ports' own `controller`
/// field, so the two never drift the way they could if both were written.
fn resync_ports(state: &mut ObservedState) {
    let pairs: Vec<(String, String)> = state
        .links
        .iter()
        .filter_map(|l| l.controller.clone().map(|c| (c, l.name.clone())))
        .collect();
    for link in &mut state.links {
        link.ports = pairs
            .iter()
            .filter(|(controller, _)| controller == &link.name)
            .map(|(_, port)| port.clone())
            .collect();
        link.ports.sort();
    }
}

fn apply_spec(link: &mut ObservedLink, spec: &ConnectionSpec) {
    link.kind = spec.kind;
    link.managed = true;
    link.controller = spec.controller.clone();
    if let Some(mtu) = spec.mtu {
        link.mtu = Some(mtu);
    }
    if let Some(vlan) = &spec.vlan {
        link.vlan_id = Some(vlan.id);
        link.parent = Some(vlan.parent.clone());
    }
    if let Some(bond) = &spec.bond {
        link.bond_mode = Some(bond.mode);
    }
    if let Some(bridge) = &spec.bridge {
        if let Some(mac) = &bridge.mac_address {
            link.mac = Some(mac.clone());
        }
    }
    link.connection_id = Some(spec.id.clone());
}

/// The addresses a spec's IP configuration produces once the link is up. DHCP
/// deterministically "leases" .50 in the same /24 as whatever the link had, or
/// a fixed address when it had none — enough for tests to tell DHCP apart from
/// static without inventing a DHCP server.
fn addresses_for(spec: &ConnectionSpec, previous: &[String]) -> (Vec<String>, Option<String>) {
    match &spec.ip {
        IpConfig::Disabled => (Vec::new(), None),
        // An empty gateway is no gateway, which is how the NetworkManager
        // backend reports a link that has none.
        IpConfig::Static { cidr, gateway, .. } => (
            vec![cidr.clone()],
            Some(gateway.clone()).filter(|g| !g.is_empty()),
        ),
        IpConfig::Dhcp => {
            let leased = previous
                .first()
                .cloned()
                .unwrap_or_else(|| "192.168.10.50/24".to_string());
            (vec![leased], Some("192.168.10.1".to_string()))
        }
    }
}

#[async_trait]
impl NetworkBackend for MockBackend {
    async fn observe(&self) -> Result<ObservedState> {
        Ok(self.inner.lock().unwrap().state.clone())
    }

    async fn apply(&self, ops: &[Op]) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let fail_after = inner.fail_apply_after.take();

        for (index, op) in ops.iter().enumerate() {
            if fail_after == Some(index) {
                inner.applied.push(op.clone());
                return Err(NetError::Backend(anyhow::anyhow!(
                    "mock backend: injected failure at op {index}"
                )));
            }
            inner.applied.push(op.clone());
            match op {
                Op::AddConnection { spec } | Op::UpdateConnection { spec, .. } => {
                    let position = inner
                        .state
                        .links
                        .iter()
                        .position(|l| l.name == spec.interface);
                    let index = match position {
                        Some(index) => index,
                        None => {
                            inner.state.links.push(ObservedLink {
                                name: spec.interface.clone(),
                                state: LinkState::Disconnected,
                                ..ObservedLink::default()
                            });
                            inner.state.links.len() - 1
                        }
                    };
                    let link = &mut inner.state.links[index];
                    apply_spec(link, spec);
                    link.connection_uuid = link
                        .connection_uuid
                        .clone()
                        .or_else(|| Some(format!("uuid-{}", spec.interface)));
                }
                Op::DeleteConnection { interface, .. } => {
                    let physical = inner
                        .state
                        .link(interface)
                        .is_some_and(|l| l.kind == LinkKind::Ethernet);
                    if physical {
                        // A NIC survives losing its profile; it just becomes
                        // an unconfigured link again.
                        if let Some(link) =
                            inner.state.links.iter_mut().find(|l| &l.name == interface)
                        {
                            link.connection_uuid = None;
                            link.connection_id = None;
                            link.controller = None;
                            link.addresses.clear();
                            link.gateway = None;
                            link.dns.clear();
                            link.ip = IpConfig::Disabled;
                            link.state = LinkState::Disconnected;
                        }
                    } else {
                        inner.state.links.retain(|l| &l.name != interface);
                    }
                }
                Op::ActivateConnection { interface } => {
                    let Some(index) = inner.state.links.iter().position(|l| &l.name == interface)
                    else {
                        return Err(NetError::NotFound(format!(
                            "mock backend: no link named {interface}"
                        )));
                    };
                    // Addresses come from the profile the last write left.
                    let previous = inner.state.links[index].addresses.clone();
                    let spec = inner
                        .applied
                        .iter()
                        .rev()
                        .find_map(|op| match op {
                            Op::AddConnection { spec } | Op::UpdateConnection { spec, .. }
                                if &spec.interface == interface =>
                            {
                                Some(spec.clone())
                            }
                            _ => None,
                        })
                        .ok_or_else(|| {
                            NetError::Backend(anyhow::anyhow!(
                                "mock backend: activate {interface} with no profile written"
                            ))
                        })?;
                    let (addresses, gateway) = addresses_for(&spec, &previous);
                    let link = &mut inner.state.links[index];
                    link.state = LinkState::Activated;
                    link.carrier = true;
                    link.addresses = addresses;
                    link.gateway = gateway;
                    link.dns = match &spec.ip {
                        IpConfig::Static { dns, .. } => dns.clone(),
                        _ => Vec::new(),
                    };
                    link.ip = spec.ip.clone();
                }
                Op::DeactivateConnection { interface } => {
                    if let Some(link) = inner.state.links.iter_mut().find(|l| &l.name == interface)
                    {
                        link.state = LinkState::Disconnected;
                        link.addresses.clear();
                        link.gateway = None;
                        link.dns.clear();
                        link.controller = None;
                    }
                }
            }
            resync_ports(&mut inner.state);
        }
        Ok(())
    }

    async fn checkpoint_create(&self, rollback_secs: u32) -> Result<CheckpointId> {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = CheckpointId(format!(
            "/org/freedesktop/NetworkManager/Checkpoint/{}",
            inner.next_id
        ));
        let snapshot = inner.state.clone();
        inner.checkpoints.push(Checkpoint {
            id: id.clone(),
            snapshot,
            rollback_secs,
        });
        Ok(id)
    }

    async fn checkpoint_destroy(&self, id: &CheckpointId) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.checkpoints.len();
        inner.checkpoints.retain(|c| &c.id != id);
        if inner.checkpoints.len() == before {
            return Err(NetError::NotFound(format!("no such checkpoint: {id}")));
        }
        Ok(())
    }

    async fn checkpoint_rollback(&self, id: &CheckpointId) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let Some(position) = inner.checkpoints.iter().position(|c| &c.id == id) else {
            return Err(NetError::NotFound(format!("no such checkpoint: {id}")));
        };
        let checkpoint = inner.checkpoints.remove(position);
        inner.state = checkpoint.snapshot;
        Ok(())
    }

    async fn checkpoint_extend(&self, id: &CheckpointId, add_secs: u32) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let Some(checkpoint) = inner.checkpoints.iter_mut().find(|c| &c.id == id) else {
            return Err(NetError::NotFound(format!("no such checkpoint: {id}")));
        };
        checkpoint.rollback_secs += add_secs;
        Ok(())
    }

    async fn checkpoints(&self) -> Result<Vec<CheckpointId>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .checkpoints
            .iter()
            .map(|c| c.id.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Bridge, ManagementRef, Nic};
    use crate::plan::plan;
    use crate::NetworkDesiredState;

    fn bridged() -> NetworkDesiredState {
        NetworkDesiredState {
            nics: vec![
                Nic {
                    name: "nic0".into(),
                    ip: IpConfig::Static {
                        cidr: "192.168.10.5/24".into(),
                        gateway: "192.168.10.1".into(),
                        dns: vec![],
                    },
                    ..Nic::default()
                },
                Nic {
                    name: "nic1".into(),
                    ..Nic::default()
                },
            ],
            bridges: vec![Bridge {
                name: "br0".into(),
                ports: vec!["nic1".into()],
                ..Bridge::default()
            }],
            management: ManagementRef {
                link: "nic0".into(),
                ..ManagementRef::default()
            },
            ..NetworkDesiredState::default()
        }
    }

    #[tokio::test]
    async fn applying_a_plan_creates_the_bridge_and_enslaves_the_port() {
        let backend = MockBackend::appliance();
        let observed = backend.observe().await.unwrap();
        let ops = plan(&bridged(), &observed);
        backend.apply(&ops).await.unwrap();

        let state = backend.state();
        let br0 = state.link("br0").expect("bridge must exist");
        assert_eq!(br0.kind, LinkKind::Bridge);
        assert_eq!(br0.state, LinkState::Activated);
        assert_eq!(br0.ports, vec!["nic1".to_string()]);
        assert_eq!(
            state.link("nic1").unwrap().controller.as_deref(),
            Some("br0")
        );
        // The management link was not disturbed.
        assert_eq!(
            state.link("nic0").unwrap().addresses,
            vec!["192.168.10.5/24".to_string()]
        );
    }

    #[tokio::test]
    async fn rollback_restores_the_snapshot_the_checkpoint_was_taken_from() {
        let backend = MockBackend::appliance();
        let before = backend.state();
        let id = backend.checkpoint_create(60).await.unwrap();

        let observed = backend.observe().await.unwrap();
        let ops = plan(&bridged(), &observed);
        backend.apply(&ops).await.unwrap();
        assert!(backend.state().link("br0").is_some());

        backend.checkpoint_rollback(&id).await.unwrap();
        assert_eq!(backend.state(), before);
        assert!(backend.checkpoints().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn expiry_rolls_back_exactly_like_an_explicit_rollback() {
        let backend = MockBackend::appliance();
        let before = backend.state();
        backend.checkpoint_create(60).await.unwrap();
        let observed = backend.observe().await.unwrap();
        backend.apply(&plan(&bridged(), &observed)).await.unwrap();

        assert_eq!(backend.expire_checkpoints(), 1);
        assert_eq!(backend.state(), before);
    }

    #[tokio::test]
    async fn destroy_makes_the_change_permanent() {
        let backend = MockBackend::appliance();
        let id = backend.checkpoint_create(60).await.unwrap();
        let observed = backend.observe().await.unwrap();
        backend.apply(&plan(&bridged(), &observed)).await.unwrap();
        backend.checkpoint_destroy(&id).await.unwrap();

        assert_eq!(backend.expire_checkpoints(), 0);
        assert!(backend.state().link("br0").is_some());
    }

    #[tokio::test]
    async fn a_partial_apply_still_rolls_all_the_way_back() {
        let backend = MockBackend::appliance();
        let before = backend.state();
        let id = backend.checkpoint_create(60).await.unwrap();
        let observed = backend.observe().await.unwrap();
        let ops = plan(&bridged(), &observed);
        backend.fail_apply_after(1);
        assert!(backend.apply(&ops).await.is_err());

        backend.checkpoint_rollback(&id).await.unwrap();
        assert_eq!(backend.state(), before);
    }

    #[tokio::test]
    async fn extending_widens_the_window() {
        let backend = MockBackend::appliance();
        let id = backend.checkpoint_create(60).await.unwrap();
        backend.checkpoint_extend(&id, 120).await.unwrap();
        assert_eq!(backend.checkpoint_window(&id), Some(180));
    }

    #[tokio::test]
    async fn deleting_a_nic_profile_leaves_the_nic_visible() {
        let backend = MockBackend::appliance();
        backend
            .apply(&[Op::DeleteConnection {
                uuid: Some("uuid-management".into()),
                interface: "nic0".into(),
            }])
            .await
            .unwrap();
        let nic0 = backend.state().link("nic0").cloned().expect("nic survives");
        assert!(nic0.connection_uuid.is_none());
        assert!(nic0.addresses.is_empty());
    }
}
