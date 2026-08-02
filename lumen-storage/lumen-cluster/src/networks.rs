//! Typed cluster networks: Core, Management, and External.
//!
//! A cluster's networks are first-class objects — the definition of what is
//! shared between its nodes — realized per node through `lumen-net`. The
//! types are deliberately three and not one "network with options":
//!
//! - **Core** carries storage replication and corosync ring 0. Exactly one
//!   per cluster, a dedicated subnet, jumbo frames recommended.
//! - **Management** carries the console and corosync ring 1, usually adopting
//!   the addressing the nodes already have, plus the optional cluster VIP.
//!   Exactly one per cluster.
//! - **External** carries virtual machine traffic: an identically named
//!   bridge on every member and no host addressing at all. Zero or more.
//!
//! Consistency on External networks is enforced, not hoped for: HA restart
//! and live migration only work if a machine's network reference resolves
//! identically on every member, so the same bridge name and the same VLAN
//! semantics everywhere is a validation rule, not a convention.
//!
//! Validation here is pure — it takes what `lumen-net` observed on each node
//! and never talks to one. Realization (making the bridges, assigning the
//! addresses, the checkpoint/confirm window) is the join workflow's business
//! in a later stage.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

use crate::validate::{ValidationCode, ValidationError};

/// An IPv4 subnet in CIDR form. Serialized as the string it is written as —
/// `"10.10.0.0/24"` — because that is the one form operators, the API, and
/// corosync.conf all agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

impl Subnet {
    fn mask(self) -> u32 {
        if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - self.prefix)
        }
    }

    pub fn contains(self, address: Ipv4Addr) -> bool {
        (u32::from(address) & self.mask()) == (u32::from(self.network) & self.mask())
    }

    pub fn overlaps(self, other: Subnet) -> bool {
        self.contains(other.network) || other.contains(self.network)
    }
}

impl fmt::Display for Subnet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

impl FromStr for Subnet {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = value
            .split_once('/')
            .ok_or_else(|| format!("\"{value}\" is not in address/prefix form."))?;
        let network: Ipv4Addr = addr
            .parse()
            .map_err(|_| format!("\"{addr}\" is not an IPv4 address."))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| format!("\"{prefix}\" is not a prefix length."))?;
        if prefix > 32 {
            return Err(format!("/{prefix} is not a prefix length."));
        }
        Ok(Subnet { network, prefix })
    }
}

impl Serialize for Subnet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Subnet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// What an External network is made of.
///
/// Only [`NetworkType::Layer2`] is realizable today. The other two are named
/// here — and offered, disabled, in the console — because the choice belongs
/// to the network's definition rather than to a later migration: a record
/// that cannot say which kind a network is, is a record every future reader
/// has to guess at. A create asking for one the appliance cannot build is
/// refused with that in so many words, not accepted and quietly ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkType {
    /// A bridge on the uplink, optionally tagged into one VLAN. The default
    /// because it is what every External network recorded before this field
    /// existed was.
    #[default]
    Layer2,
    /// Routed. Not yet buildable.
    Layer3,
    /// Overlay. Not yet buildable.
    Vxlan,
}

impl NetworkType {
    pub fn as_str(self) -> &'static str {
        match self {
            NetworkType::Layer2 => "Layer 2",
            NetworkType::Layer3 => "Layer 3",
            NetworkType::Vxlan => "VXLAN",
        }
    }

    /// Whether the appliance can build this kind of network today.
    pub fn buildable(self) -> bool {
        matches!(self, NetworkType::Layer2)
    }
}

/// One node's seat on a Core or Management network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressedMember {
    pub node: String,
    /// The NIC or bond carrying this network on this node.
    pub interface: String,
    pub address: Ipv4Addr,
}

/// The Core network: storage replication and corosync ring 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreNetwork {
    pub subnet: Subnet,
    /// Jumbo frames are the default recommendation on a dedicated link —
    /// replication traffic is exactly the workload they exist for.
    pub mtu: u32,
    pub members: Vec<AddressedMember>,
}

/// What a Core-network edit may change: the frame size, and which link
/// carries each member's seat.
///
/// Deliberately not a [`CoreNetwork`]: the subnet and the per-member
/// addresses are corosync's ring 0 addressing — the ring's identity — and a
/// request shape that cannot carry them is a request that cannot quietly ask
/// for a renumbering. The edit workflow additionally refuses a `members`
/// list that changes an address or the set of seats.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreNetworkUpdate {
    #[serde(default)]
    pub mtu: Option<u32>,
    /// The seats as they should now be: every member, same addresses, only
    /// the interfaces free to differ from the record.
    #[serde(default)]
    pub members: Option<Vec<AddressedMember>>,
}

/// The Management network: the console, corosync ring 1, and the optional
/// cluster VIP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagementNetwork {
    pub subnet: Subnet,
    /// A Pacemaker-managed floating address: one stable IP for the cluster's
    /// console that moves to a survivor on failure. The per-node addresses
    /// stay valid alongside it.
    pub vip: Option<Ipv4Addr>,
    pub members: Vec<AddressedMember>,
}

/// One node's uplink into an External network.
///
/// More than one interface is a bond: the member bonds the listed ports and
/// carries the network on the bond. Which ports those are is per node because
/// cabling is — the same network can arrive on `nic1`+`nic2` here and
/// `nic3`+`nic4` there — but *how many* is not policed across members, since a
/// node with one spare port is still a node that must have the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Uplink {
    pub node: String,
    /// The links this node carries the network on, in the order the operator
    /// chose them. One is used as-is; two or more are bonded first.
    pub interfaces: Vec<String>,
}

impl Uplink {
    /// Whether this seat has to build a bond before it can build a bridge.
    pub fn bonded(&self) -> bool {
        self.interfaces.len() > 1
    }
}

/// Accepts the single-`interface` form as well as the list.
///
/// Not politeness to old clients — the membership records on running
/// appliances are written in it, and a decode that refused them would be an
/// upgrade that loses every External network a cluster has.
impl<'de> Deserialize<'de> for Uplink {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            node: String,
            #[serde(default)]
            interfaces: Vec<String>,
            #[serde(default)]
            interface: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut interfaces = raw.interfaces;
        if let Some(one) = raw.interface {
            if !interfaces.contains(&one) {
                interfaces.push(one);
            }
        }
        Ok(Uplink {
            node: raw.node,
            interfaces,
        })
    }
}

/// An External network: VM traffic over an identically named bridge on every
/// member, no host addressing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalNetwork {
    pub name: String,
    /// The bridge's name on every member — identical everywhere, enforced by
    /// construction: there is one field, not one per node.
    pub bridge: String,
    #[serde(default, rename = "type")]
    pub network_type: NetworkType,
    /// Layer 2 only: the VLAN this network is an access port onto. `None`
    /// leaves the bridge VLAN-aware, passing tags through to the machines —
    /// which is what a network recorded before this field existed asked for,
    /// and why the absent case is the untagged one rather than an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlan: Option<u16>,
    /// How a member with more than one uplink bonds them. One choice for the
    /// network, not one per node: the operator picks a bonding scheme because
    /// of what the switches do, and two members of one cluster answering the
    /// same switch fabric differently is a misconfiguration, not a feature.
    /// `None` when no member is bonded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bond: Option<lumen_net::BondMode>,
    pub uplinks: Vec<Uplink>,
}

impl ExternalNetwork {
    /// This node's seat, if it has one.
    pub fn uplink(&self, node: &str) -> Option<&Uplink> {
        self.uplinks.iter().find(|up| up.node == node)
    }

    /// Whether any member carries this network on a bond.
    pub fn any_bonded(&self) -> bool {
        self.uplinks.iter().any(Uplink::bonded)
    }
}

/// What the kernel takes as a link name.
const IFNAMSIZ: usize = 15;

/// The bond a member builds when it carries this network on more than one
/// port.
///
/// Derived from the network's name rather than stored, and so identical on
/// every member: an operator reading two nodes' Interfaces pages sees one
/// name for one thing. Nothing else in the appliance may claim it — a bond
/// found under this name with different ports is a conflict the member
/// reports rather than a link it adopts.
///
/// The prefix is `bn-` rather than the more obvious `bond-` because of what
/// has to fit on top of it. An access network puts `<bond>.<vlan>` above the
/// bond, the kernel takes 15 characters for that name too, and a four-digit
/// tag costs five of them — so `bond-` would leave five characters for the
/// network's name and refuse "guests" on VLAN 4094. Three characters of
/// prefix buys back the two that make ordinary names work; what a link is, is
/// a column on the Interfaces page and a comment on the connection, neither of
/// which is paying for the tag.
pub fn bond_name_for(network: &str) -> String {
    let slug: String = network
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // Runs of punctuation collapse: "vm net--1" and "vm-net-1" are the same
    // cabling, and two bonds a character apart help nobody.
    let mut squeezed = String::new();
    for c in slug.chars() {
        if c == '-' && squeezed.ends_with('-') {
            continue;
        }
        squeezed.push(c);
    }
    format!("bn-{}", squeezed.trim_matches('-'))
}

/// Whether a derived bond name — and the VLAN interface that would ride on
/// it — fit what the kernel takes.
///
/// Checked rather than truncated. Truncating is how two networks end up
/// sharing one bond name on a box where the second silently fails to build,
/// and the operator has a shorter name available for the asking.
pub fn bond_name_fits(bond: &str, vlan: Option<u16>) -> bool {
    if bond.len() <= "bn-".len() {
        return false;
    }
    let longest = match vlan {
        Some(id) => bond.len() + 1 + id.to_string().len(),
        None => bond.len(),
    };
    longest <= IFNAMSIZ
}

/// Everything a cluster's members share, as one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterNetworks {
    pub core: CoreNetwork,
    pub management: ManagementNetwork,
    #[serde(default)]
    pub external: Vec<ExternalNetwork>,
}

/// A usable bridge name: what the kernel accepts as an interface name.
pub fn valid_bridge_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && !name.contains(['/', ' ', ':'])
        && name != "."
        && name != ".."
}

/// Whether these networks are realizable on these nodes. `observed` is one
/// `lumen_net::ObservedState` per member, as each node's control plane
/// reported it; a node with no entry fails its checks with "not observed"
/// rather than passing them silently.
pub fn validate_networks(
    networks: &ClusterNetworks,
    nodes: &[String],
    observed: &[lumen_net::ObservedState],
) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // Subnets must not overlap: a Core packet that could route over
    // Management is a ring that is not the ring it claims to be.
    if networks.core.subnet.overlaps(networks.management.subnet) {
        errors.push(ValidationError::new(
            ValidationCode::OverlappingSubnets,
            Some("core.subnet"),
            format!(
                "The Core subnet {} overlaps the Management subnet {} — the two rings must be \
                 separate networks.",
                networks.core.subnet, networks.management.subnet
            ),
        ));
    }

    if !(576..=9216).contains(&networks.core.mtu) {
        errors.push(ValidationError::new(
            ValidationCode::InvalidMtu,
            Some("core.mtu"),
            format!("{} is not a usable MTU.", networks.core.mtu),
        ));
    }

    // Every member node has a seat on Core and Management — exactly one.
    for (label, members) in [
        ("core", &networks.core.members),
        ("management", &networks.management.members),
    ] {
        for node in nodes {
            match members.iter().filter(|m| &m.node == node).count() {
                0 => errors.push(ValidationError::new(
                    ValidationCode::NetworkMemberMissing,
                    Some(&format!("{label}.members")),
                    format!("\"{node}\" has no seat on the {label} network."),
                )),
                1 => {}
                n => errors.push(ValidationError::new(
                    ValidationCode::NetworkMemberMissing,
                    Some(&format!("{label}.members")),
                    format!("\"{node}\" is listed {n} times on the {label} network."),
                )),
            }
        }
        for member in members.iter() {
            if !nodes.contains(&member.node) {
                errors.push(ValidationError::new(
                    ValidationCode::NetworkMemberNotInCluster,
                    Some(&format!("{label}.members")),
                    format!("\"{}\" is not one of the cluster's members.", member.node),
                ));
            }
        }
    }

    // Addresses sit inside their subnet and are unique within each network.
    for (label, subnet, members) in [
        ("core", networks.core.subnet, &networks.core.members),
        (
            "management",
            networks.management.subnet,
            &networks.management.members,
        ),
    ] {
        let mut seen: Vec<Ipv4Addr> = Vec::new();
        for member in members.iter() {
            if !subnet.contains(member.address) {
                errors.push(ValidationError::new(
                    ValidationCode::AddressOutsideSubnet,
                    Some(&format!("{label}.members")),
                    format!(
                        "{} on \"{}\" is outside the {label} subnet {subnet}.",
                        member.address, member.node
                    ),
                ));
            }
            if seen.contains(&member.address) {
                errors.push(ValidationError::new(
                    ValidationCode::DuplicateAddress,
                    Some(&format!("{label}.members")),
                    format!("{} is assigned more than once.", member.address),
                ));
            }
            seen.push(member.address);
        }
    }

    // Core and Management must ride different interfaces on every node: one
    // cable is one failure, and two rings on it are one ring with extra steps.
    for node in nodes {
        let core = networks.core.members.iter().find(|m| &m.node == node);
        let management = networks.management.members.iter().find(|m| &m.node == node);
        if let (Some(core), Some(management)) = (core, management) {
            if core.interface == management.interface {
                errors.push(ValidationError::new(
                    ValidationCode::SharedCoreManagementInterface,
                    Some("core.members"),
                    format!(
                        "\"{node}\" carries Core and Management on \"{}\" — the rings must not \
                         share a link.",
                        core.interface
                    ),
                ));
            }
        }
    }

    // The VIP lives in the Management subnet and is nobody's node address.
    if let Some(vip) = networks.management.vip {
        if !networks.management.subnet.contains(vip) {
            errors.push(ValidationError::new(
                ValidationCode::InvalidVip,
                Some("management.vip"),
                format!(
                    "The cluster VIP {vip} is outside the Management subnet {}.",
                    networks.management.subnet
                ),
            ));
        }
        if networks.management.members.iter().any(|m| m.address == vip) {
            errors.push(ValidationError::new(
                ValidationCode::InvalidVip,
                Some("management.vip"),
                format!("The cluster VIP {vip} is already a node's own address."),
            ));
        }
    }

    // External networks: usable names, sane VLANs, one uplink per member.
    for external in &networks.external {
        if !valid_bridge_name(&external.bridge) {
            errors.push(ValidationError::new(
                ValidationCode::InvalidBridgeName,
                Some("external"),
                format!(
                    "\"{}\" is not a usable bridge name for \"{}\".",
                    external.bridge, external.name
                ),
            ));
        }
        if let Some(vlan) = external.vlan {
            if !(1..=4094).contains(&vlan) {
                errors.push(ValidationError::new(
                    ValidationCode::InvalidVlan,
                    Some("external"),
                    format!("VLAN {vlan} on \"{}\" is not in 1–4094.", external.name),
                ));
            }
        }
        // A bond has to be named before it is built, and the name has to
        // survive the VLAN interface that rides on it.
        if external.any_bonded() {
            if external.bond.is_none() {
                errors.push(ValidationError::new(
                    ValidationCode::InvalidVlan,
                    Some("external"),
                    format!(
                        "\"{}\" bonds more than one port on a member but says nothing about how \
                         — a bond needs a mode before it can be built.",
                        external.name
                    ),
                ));
            }
            let bond = bond_name_for(&external.name);
            if !bond_name_fits(&bond, external.vlan) {
                errors.push(ValidationError::new(
                    ValidationCode::InvalidBridgeName,
                    Some("external"),
                    format!(
                        "\"{bond}\" is the bond \"{}\" would build, and it does not fit the {} \
                         characters the kernel allows a link name.",
                        external.name, IFNAMSIZ
                    ),
                ));
            }
        }
        for uplink in &external.uplinks {
            if uplink.interfaces.is_empty() {
                errors.push(ValidationError::new(
                    ValidationCode::NetworkMemberMissing,
                    Some("external"),
                    format!(
                        "\"{}\" has a seat on \"{}\" with no interfaces on it.",
                        uplink.node, external.name
                    ),
                ));
            }
            // The same port twice is one port, and a bond built from it is a
            // bond of one that the operator believes is redundant.
            for (index, port) in uplink.interfaces.iter().enumerate() {
                if uplink.interfaces[..index].contains(port) {
                    errors.push(ValidationError::new(
                        ValidationCode::UnknownInterface,
                        Some("external"),
                        format!(
                            "\"{}\" names \"{port}\" twice on \"{}\".",
                            uplink.node, external.name
                        ),
                    ));
                }
            }
        }
        for node in nodes {
            if !external.uplinks.iter().any(|u| &u.node == node) {
                errors.push(ValidationError::new(
                    ValidationCode::NetworkMemberMissing,
                    Some("external"),
                    format!(
                        "\"{node}\" has no uplink into \"{}\" — an External network exists on \
                         every member or on none, because a machine restarted there must find it.",
                        external.name
                    ),
                ));
            }
        }
        for uplink in &external.uplinks {
            if !nodes.contains(&uplink.node) {
                errors.push(ValidationError::new(
                    ValidationCode::NetworkMemberNotInCluster,
                    Some("external"),
                    format!("\"{}\" is not one of the cluster's members.", uplink.node),
                ));
            }
        }
    }

    // Finally, the interfaces named must exist and carry link, per what each
    // node's own control plane observed.
    let mut wanted: Vec<(&str, &str, &str)> = Vec::new();
    for member in &networks.core.members {
        wanted.push((member.node.as_str(), member.interface.as_str(), "core"));
    }
    for member in &networks.management.members {
        wanted.push((
            member.node.as_str(),
            member.interface.as_str(),
            "management",
        ));
    }
    for external in &networks.external {
        for uplink in &external.uplinks {
            for interface in &uplink.interfaces {
                wanted.push((uplink.node.as_str(), interface.as_str(), "external"));
            }
        }
    }
    for (node, interface, label) in wanted {
        if !nodes.iter().any(|n| n == node) {
            continue; // Already reported as not a member.
        }
        let Some(state) = observed.iter().find(|s| s.node == node) else {
            errors.push(ValidationError::new(
                ValidationCode::UnknownInterface,
                Some(&format!("{label}.members")),
                format!(
                    "\"{node}\" has not reported its links, so \"{interface}\" cannot be \
                     checked."
                ),
            ));
            continue;
        };
        match state.link(interface) {
            None => errors.push(ValidationError::new(
                ValidationCode::UnknownInterface,
                Some(&format!("{label}.members")),
                format!("\"{node}\" has no link called \"{interface}\"."),
            )),
            Some(link) if !link.carrier => errors.push(ValidationError::new(
                ValidationCode::InterfaceDown,
                Some(&format!("{label}.members")),
                format!(
                    "\"{interface}\" on \"{node}\" has no carrier — plug it in before building \
                     a ring on it."
                ),
            )),
            Some(_) => {}
        }
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_net::{LinkKind, LinkState, ObservedLink, ObservedState};

    fn observed(node: &str, links: &[&str]) -> ObservedState {
        ObservedState {
            node: node.into(),
            links: links
                .iter()
                .map(|name| ObservedLink {
                    name: (*name).into(),
                    kind: LinkKind::Ethernet,
                    state: LinkState::Activated,
                    carrier: true,
                    ..ObservedLink::default()
                })
                .collect(),
        }
    }

    fn networks() -> ClusterNetworks {
        ClusterNetworks {
            core: CoreNetwork {
                subnet: "10.10.0.0/24".parse().unwrap(),
                mtu: 9000,
                members: vec![
                    AddressedMember {
                        node: "a-1".into(),
                        interface: "nic1".into(),
                        address: Ipv4Addr::new(10, 10, 0, 1),
                    },
                    AddressedMember {
                        node: "a-2".into(),
                        interface: "nic1".into(),
                        address: Ipv4Addr::new(10, 10, 0, 2),
                    },
                ],
            },
            management: ManagementNetwork {
                subnet: "192.168.10.0/24".parse().unwrap(),
                vip: Some(Ipv4Addr::new(192, 168, 10, 100)),
                members: vec![
                    AddressedMember {
                        node: "a-1".into(),
                        interface: "nic0".into(),
                        address: Ipv4Addr::new(192, 168, 10, 1),
                    },
                    AddressedMember {
                        node: "a-2".into(),
                        interface: "nic0".into(),
                        address: Ipv4Addr::new(192, 168, 10, 2),
                    },
                ],
            },
            external: vec![ExternalNetwork {
                name: "guests".into(),
                bridge: "vmbr0".into(),
                network_type: NetworkType::Layer2,
                vlan: None,
                bond: None,
                uplinks: vec![
                    Uplink {
                        node: "a-1".into(),
                        interfaces: vec!["nic2".into()],
                    },
                    Uplink {
                        node: "a-2".into(),
                        interfaces: vec!["nic2".into()],
                    },
                ],
            }],
        }
    }

    fn nodes() -> Vec<String> {
        vec!["a-1".into(), "a-2".into()]
    }

    fn full_observation() -> Vec<ObservedState> {
        vec![
            observed("a-1", &["nic0", "nic1", "nic2"]),
            observed("a-2", &["nic0", "nic1", "nic2"]),
        ]
    }

    fn codes(errors: &[ValidationError]) -> Vec<ValidationCode> {
        errors.iter().map(|e| e.code).collect()
    }

    #[test]
    fn a_subnet_parses_contains_and_round_trips() {
        let subnet: Subnet = "10.10.0.0/24".parse().unwrap();
        assert!(subnet.contains(Ipv4Addr::new(10, 10, 0, 200)));
        assert!(!subnet.contains(Ipv4Addr::new(10, 10, 1, 1)));
        assert_eq!(subnet.to_string(), "10.10.0.0/24");
        assert_eq!(serde_json::to_string(&subnet).unwrap(), "\"10.10.0.0/24\"");
        let back: Subnet = serde_json::from_str("\"10.10.0.0/24\"").unwrap();
        assert_eq!(back, subnet);
        assert!("10.10.0.0".parse::<Subnet>().is_err());
        assert!("10.10.0.0/33".parse::<Subnet>().is_err());
    }

    #[test]
    fn overlap_is_symmetric_and_prefix_aware() {
        let wide: Subnet = "10.0.0.0/8".parse().unwrap();
        let narrow: Subnet = "10.10.0.0/24".parse().unwrap();
        let other: Subnet = "192.168.10.0/24".parse().unwrap();
        assert!(wide.overlaps(narrow));
        assert!(narrow.overlaps(wide));
        assert!(!narrow.overlaps(other));
    }

    #[test]
    fn a_well_formed_definition_passes() {
        let errors = validate_networks(&networks(), &nodes(), &full_observation());
        assert_eq!(errors, vec![], "expected no errors");
    }

    #[test]
    fn the_rings_must_not_share_a_link() {
        let mut n = networks();
        n.core.members[0].interface = "nic0".into();
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::SharedCoreManagementInterface));
    }

    #[test]
    fn overlapping_subnets_are_refused() {
        let mut n = networks();
        n.management.subnet = "10.10.0.0/16".parse().unwrap();
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::OverlappingSubnets));
    }

    #[test]
    fn every_member_needs_a_seat_on_core_and_management() {
        let mut n = networks();
        n.core.members.pop();
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::NetworkMemberMissing));
    }

    #[test]
    fn an_address_outside_its_subnet_is_refused() {
        let mut n = networks();
        n.core.members[0].address = Ipv4Addr::new(172, 16, 0, 1);
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::AddressOutsideSubnet));
    }

    #[test]
    fn the_vip_must_live_in_the_management_subnet_and_be_nobodys_address() {
        let mut n = networks();
        n.management.vip = Some(Ipv4Addr::new(10, 99, 0, 1));
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::InvalidVip));

        let mut n = networks();
        n.management.vip = Some(Ipv4Addr::new(192, 168, 10, 1));
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::InvalidVip));
    }

    /// The consistency rule HA depends on: an External network is on every
    /// member or on none.
    #[test]
    fn an_external_network_missing_a_member_is_refused() {
        let mut n = networks();
        n.external[0].uplinks.pop();
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::NetworkMemberMissing));
    }

    #[test]
    fn an_interface_the_node_does_not_have_is_refused() {
        let mut obs = full_observation();
        obs[1].links.retain(|l| l.name != "nic1");
        assert!(codes(&validate_networks(&networks(), &nodes(), &obs))
            .contains(&ValidationCode::UnknownInterface));
    }

    #[test]
    fn a_dead_link_is_named_before_a_ring_is_built_on_it() {
        let mut obs = full_observation();
        obs[0].links[1].carrier = false;
        assert!(codes(&validate_networks(&networks(), &nodes(), &obs))
            .contains(&ValidationCode::InterfaceDown));
    }

    #[test]
    fn a_node_that_never_reported_fails_its_checks_rather_than_passing_them() {
        let obs = vec![observed("a-1", &["nic0", "nic1", "nic2"])];
        assert!(codes(&validate_networks(&networks(), &nodes(), &obs))
            .contains(&ValidationCode::UnknownInterface));
    }

    #[test]
    fn vlan_ids_stay_inside_dot1q() {
        let mut n = networks();
        n.external[0].vlan = Some(5000);
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::InvalidVlan));
    }

    #[test]
    fn every_port_of_a_bonded_seat_is_checked_not_just_the_first() {
        let mut n = networks();
        n.external[0].bond = Some(lumen_net::BondMode::Ieee8023ad);
        n.external[0].uplinks[0].interfaces = vec!["nic2".into(), "nic9".into()];
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::UnknownInterface));
    }

    #[test]
    fn a_bonded_seat_without_a_mode_is_a_bond_that_cannot_be_built() {
        let mut n = networks();
        n.external[0].uplinks[0].interfaces = vec!["nic1".into(), "nic2".into()];
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::InvalidVlan));
    }

    #[test]
    fn one_port_named_twice_is_not_a_bond() {
        let mut n = networks();
        n.external[0].bond = Some(lumen_net::BondMode::ActiveBackup);
        n.external[0].uplinks[0].interfaces = vec!["nic2".into(), "nic2".into()];
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::UnknownInterface));
    }

    #[test]
    fn a_bond_name_that_outgrows_the_kernel_is_refused_before_it_is_truncated() {
        let mut n = networks();
        n.external[0].name = "guest traffic north".into();
        n.external[0].bond = Some(lumen_net::BondMode::ActiveBackup);
        n.external[0].vlan = Some(100);
        n.external[0].uplinks[0].interfaces = vec!["nic1".into(), "nic2".into()];
        assert!(codes(&validate_networks(&n, &nodes(), &full_observation()))
            .contains(&ValidationCode::InvalidBridgeName));
    }

    #[test]
    fn a_derived_bond_name_is_one_name_for_one_thing() {
        assert_eq!(bond_name_for("guests"), "bn-guests");
        // Punctuation and case are cabling noise, not identity.
        assert_eq!(bond_name_for("VM Net--1"), "bn-vm-net-1");
        // The tag is what the budget is really for: an ordinary name has to
        // survive the widest VLAN there is.
        assert!(bond_name_fits(&bond_name_for("guests"), Some(4094)));
        assert!(!bond_name_fits(&bond_name_for("guest-traffic"), Some(100)));
        // A network named entirely out of punctuation derives nothing usable.
        assert!(!bond_name_fits(&bond_name_for("---"), None));
    }

    /// The membership records on running appliances are written in the older
    /// single-`interface`, `mode`-tagged form. Decoding them is what makes an
    /// upgrade an upgrade rather than a loss of every External network.
    #[test]
    fn the_older_recorded_form_still_decodes() {
        let access: ExternalNetwork = serde_json::from_str(
            r#"{"name":"guests","bridge":"vmbr0","mode":"access","vlan":100,
                "uplinks":[{"node":"a-1","interface":"nic2"}]}"#,
        )
        .expect("an access network as it was recorded");
        assert_eq!(access.network_type, NetworkType::Layer2);
        assert_eq!(access.vlan, Some(100));
        assert_eq!(access.uplinks[0].interfaces, vec!["nic2".to_string()]);
        assert!(!access.any_bonded());

        // A trunk had no VLAN of its own — it passed every tag through — and
        // that is exactly what an absent VLAN now means.
        let trunk: ExternalNetwork = serde_json::from_str(
            r#"{"name":"guests","bridge":"vmbr0","mode":"trunk","allowed":[10,20],
                "uplinks":[{"node":"a-1","interface":"nic2"}]}"#,
        )
        .expect("a trunk network as it was recorded");
        assert_eq!(trunk.vlan, None);
        assert_eq!(trunk.uplinks[0].interfaces, vec!["nic2".to_string()]);
    }
}
