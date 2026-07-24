//! Observed state: what is actually on the box right now.
//!
//! Deliberately independent of desired state — a NIC nobody has configured
//! still appears here, and so does a link the appliance did not create. The
//! console's table renders straight from this plus the pending set, so every
//! field a row needs is present in one round trip.

use serde::{Deserialize, Serialize};

use crate::model::{
    Bond, BondMode, Bridge, Duplex, IpConfig, LinkKind, ManagementRef, NetworkDesiredState, Nic,
    Vlan,
};

/// Everything observed on one node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedState {
    /// Hostname of the node these links belong to.
    pub node: String,
    pub links: Vec<ObservedLink>,
}

impl ObservedState {
    pub fn link(&self, name: &str) -> Option<&ObservedLink> {
        self.links.iter().find(|l| l.name == name)
    }

    /// Links reporting an address, in observation order.
    pub fn addressed(&self) -> impl Iterator<Item = &ObservedLink> {
        self.links.iter().filter(|l| !l.addresses.is_empty())
    }
}

/// Where NetworkManager thinks a device is. A coarse projection of NM's
/// device-state enum: the console shows admin/operational state, not the
/// twelve-step activation ladder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    /// Configured and up.
    Activated,
    /// Mid-activation.
    Activating,
    /// Present, not configured.
    Disconnected,
    /// Present but NetworkManager is not touching it.
    Unmanaged,
    #[default]
    Unknown,
}

impl LinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkState::Activated => "activated",
            LinkState::Activating => "activating",
            LinkState::Disconnected => "disconnected",
            LinkState::Unmanaged => "unmanaged",
            LinkState::Unknown => "unknown",
        }
    }

    pub fn is_up(self) -> bool {
        matches!(self, LinkState::Activated | LinkState::Activating)
    }
}

/// One link as the box reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedLink {
    pub name: String,
    pub kind: LinkKind,
    pub state: LinkState,
    /// NetworkManager is managing this device.
    pub managed: bool,
    /// Physical carrier. Always false for links that have no carrier concept.
    pub carrier: bool,
    /// Burned-in address — the one to pin a bridge to. `None` for virtual
    /// links and for drivers that do not report one.
    pub perm_mac: Option<String>,
    /// Address currently on the link.
    pub mac: Option<String>,
    pub speed_mbps: Option<u32>,
    pub duplex: Option<Duplex>,
    pub mtu: Option<u32>,
    /// Addresses in CIDR form, e.g. ["192.168.10.5/24"].
    pub addresses: Vec<String>,
    pub gateway: Option<String>,
    /// Resolvers in use on this link.
    pub dns: Vec<String>,
    /// The layer-3 configuration the owning profile asks for — how the
    /// addresses above came to be. `Disabled` when nothing configures this
    /// link. This is what lets a desired state be seeded from a running box
    /// without losing "DHCP" and turning it into a static lease.
    pub ip: IpConfig,
    /// The bridge or bond this link is enslaved to.
    pub controller: Option<String>,
    /// Ports enslaved to this link, when it is a controller.
    pub ports: Vec<String>,
    pub vlan_id: Option<u16>,
    /// VLAN parent link.
    pub parent: Option<String>,
    pub bond_mode: Option<BondMode>,
    /// The NetworkManager connection that owns this link, when one does.
    pub connection_uuid: Option<String>,
    pub connection_id: Option<String>,
}

impl ObservedLink {
    /// A physical NIC as far as the appliance is concerned: it has a burned-in
    /// address and was not created by us. These are configured, never deleted.
    pub fn is_physical(&self) -> bool {
        self.kind == LinkKind::Ethernet
    }
}

/// The desired state a running box implies, for the first boot after an
/// install (or any time `desired.json` is missing): every link that exists,
/// configured the way it already is. Seeding from the box rather than from an
/// empty document is what keeps the first API call from proposing to tear the
/// appliance's own management connection down.
pub fn derive_desired(observed: &ObservedState) -> NetworkDesiredState {
    let mut desired = NetworkDesiredState::default();

    for link in &observed.links {
        match link.kind {
            LinkKind::Ethernet => desired.nics.push(Nic {
                name: link.name.clone(),
                mtu: link.mtu,
                autoneg: None,
                speed: None,
                duplex: None,
            }),
            LinkKind::Bond => desired.bonds.push(Bond {
                name: link.name.clone(),
                mode: link.bond_mode.unwrap_or_default(),
                ports: link.ports.clone(),
                mtu: link.mtu,
                ..Bond::default()
            }),
            LinkKind::Bridge => desired.bridges.push(Bridge {
                name: link.name.clone(),
                ports: link.ports.clone(),
                // Pin whatever the bridge is using now, so seeding does not
                // itself become the change that moves the management MAC.
                mac_address: link.mac.clone(),
                mtu: link.mtu,
                ..Bridge::default()
            }),
            LinkKind::Vlan => desired.vlans.push(Vlan {
                name: link.name.clone(),
                parent: link.parent.clone().unwrap_or_default(),
                vlan_id: link.vlan_id.unwrap_or(1),
                mtu: link.mtu,
            }),
            LinkKind::Other => {}
        }
    }

    // The management link is the one carrying an address. Prefer a link that
    // is up; a box with none leaves the reference empty and validation says so.
    let management = observed
        .addressed()
        .find(|l| l.state.is_up())
        .or_else(|| observed.addressed().next());
    if let Some(link) = management {
        desired.management = ManagementRef {
            link: link.name.clone(),
            ip: link.ip.clone(),
        };
    }
    desired
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_kind_defaults_to_other() {
        assert_eq!(ObservedLink::default().kind, LinkKind::Other);
    }

    #[test]
    fn addressed_finds_the_management_link() {
        let state = ObservedState {
            node: "lumen".into(),
            links: vec![
                ObservedLink {
                    name: "nic0".into(),
                    kind: LinkKind::Ethernet,
                    ..ObservedLink::default()
                },
                ObservedLink {
                    name: "br0".into(),
                    kind: LinkKind::Bridge,
                    addresses: vec!["192.168.10.5/24".into()],
                    ..ObservedLink::default()
                },
            ],
        };
        let names: Vec<&str> = state.addressed().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["br0"]);
    }

    #[test]
    fn seeding_from_a_running_box_keeps_dhcp_dhcp() {
        let observed = ObservedState {
            node: "lumen".into(),
            links: vec![
                ObservedLink {
                    name: "nic0".into(),
                    kind: LinkKind::Ethernet,
                    state: LinkState::Activated,
                    mtu: Some(1500),
                    addresses: vec!["192.168.10.50/24".into()],
                    ip: IpConfig::Dhcp,
                    ..ObservedLink::default()
                },
                ObservedLink {
                    name: "nic1".into(),
                    kind: LinkKind::Ethernet,
                    ..ObservedLink::default()
                },
            ],
        };
        let desired = derive_desired(&observed);
        assert_eq!(desired.nics.len(), 2);
        assert_eq!(desired.management.link, "nic0");
        assert_eq!(desired.management.ip, IpConfig::Dhcp);
        assert_eq!(desired.nics[0].mtu, Some(1500));
    }

    #[test]
    fn seeding_pins_an_existing_bridge_to_the_mac_it_already_uses() {
        let observed = ObservedState {
            node: "lumen".into(),
            links: vec![ObservedLink {
                name: "br0".into(),
                kind: LinkKind::Bridge,
                state: LinkState::Activated,
                mac: Some("52:54:00:aa:bb:00".into()),
                ports: vec!["nic0".into()],
                addresses: vec!["192.168.10.5/24".into()],
                ip: IpConfig::Static {
                    cidr: "192.168.10.5/24".into(),
                    gateway: "192.168.10.1".into(),
                    dns: vec!["9.9.9.9".into()],
                },
                ..ObservedLink::default()
            }],
        };
        let desired = derive_desired(&observed);
        assert_eq!(
            desired.bridges[0].mac_address.as_deref(),
            Some("52:54:00:aa:bb:00")
        );
        assert_eq!(desired.bridges[0].ports, vec!["nic0".to_string()]);
        assert_eq!(desired.management.link, "br0");
        // DNS survives the round trip — an appliance seeded from its own box
        // must not silently lose its resolvers on the next apply.
        match desired.management.ip {
            IpConfig::Static { ref dns, .. } => assert_eq!(dns, &vec!["9.9.9.9".to_string()]),
            ref other => panic!("expected static, got {other:?}"),
        }
    }

    #[test]
    fn seeding_an_empty_box_leaves_the_management_reference_empty() {
        let desired = derive_desired(&ObservedState::default());
        assert!(desired.management.link.is_empty());
    }
}
