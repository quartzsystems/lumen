//! desired + observed -> an ordered list of backend operations.
//!
//! The plan is data, not behavior: it prints, it round-trips through serde,
//! and it unit-tests without a system bus — the same shape as the installer's
//! install plan (lumen-installer/app/src/engine/plan.rs).
//!
//! Ordering rules, in the order they matter:
//!
//!   1. Links that disappear are deactivated before anything is deleted, and
//!      ports are deleted before the controllers they referenced.
//!   2. Controllers are created before their ports, and a VLAN's parent
//!      before the VLAN — one topological sort covers both.
//!   3. The management link is touched last in every phase, so a plan that
//!      fails halfway fails before it can strand the operator's session.
//!
//! Profiles are written declaratively (every desired link gets an Add or an
//! Update), but activation is *not*: only links that are new, down, or
//! observably different are activated. Rewriting a NetworkManager profile
//! does not disturb the running device — reactivating it does — so this is
//! what keeps an unrelated change from blipping the management link.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::{
    BondMode, Bridge, Duplex, IpConfig, LacpRate, LinkKind, NetworkDesiredState, XmitHashPolicy,
};
use crate::state::ObservedState;

/// What a port is a port of. NetworkManager needs this alongside the
/// controller name (`connection.port-type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortType {
    Bridge,
    Bond,
}

impl PortType {
    pub fn as_str(self) -> &'static str {
        match self {
            PortType::Bridge => "bridge",
            PortType::Bond => "bond",
        }
    }
}

/// Link-layer settings for a physical NIC.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EthernetSpec {
    pub autoneg: Option<bool>,
    pub speed: Option<u32>,
    pub duplex: Option<Duplex>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BondSpec {
    pub mode: BondMode,
    pub miimon: Option<u32>,
    pub lacp_rate: Option<LacpRate>,
    pub xmit_hash_policy: Option<XmitHashPolicy>,
    pub primary: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeSpec {
    pub stp: bool,
    pub forward_delay: Option<u32>,
    pub vlan_filtering: bool,
    pub mac_address: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlanSpec {
    pub parent: String,
    pub id: u16,
}

/// One NetworkManager connection profile, in Lumen's terms. The backend turns
/// this into the `a{sa{sv}}` NM wants; nothing above the backend knows that
/// shape exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionSpec {
    /// Profile id. Lumen uses the interface name, except for the management
    /// profile which keeps the installer's `management` / `management-port`
    /// ids so an upgraded appliance recognises its own files.
    pub id: String,
    pub interface: String,
    pub kind: LinkKind,
    /// The bridge/bond this link is enslaved to.
    pub controller: Option<String>,
    pub port_type: Option<PortType>,
    pub mtu: Option<u32>,
    pub ip: IpConfig,
    pub ethernet: Option<EthernetSpec>,
    pub bond: Option<BondSpec>,
    pub bridge: Option<BridgeSpec>,
    pub vlan: Option<VlanSpec>,
}

impl ConnectionSpec {
    fn base(interface: &str, kind: LinkKind) -> Self {
        Self {
            id: interface.to_string(),
            interface: interface.to_string(),
            kind,
            controller: None,
            port_type: None,
            mtu: None,
            ip: IpConfig::Disabled,
            ethernet: None,
            bond: None,
            bridge: None,
            vlan: None,
        }
    }
}

/// A single backend operation. Enumerated and typed so a plan can be printed
/// and asserted on without a bus connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    AddConnection {
        spec: ConnectionSpec,
    },
    UpdateConnection {
        /// The profile to rewrite. `None` when observation could not name one
        /// and the backend must resolve it by interface.
        uuid: Option<String>,
        spec: ConnectionSpec,
    },
    DeleteConnection {
        uuid: Option<String>,
        interface: String,
    },
    ActivateConnection {
        interface: String,
    },
    DeactivateConnection {
        interface: String,
    },
}

impl Op {
    /// The link this operation is about — for logging and for the management
    /// -last ordering.
    pub fn interface(&self) -> &str {
        match self {
            Op::AddConnection { spec } | Op::UpdateConnection { spec, .. } => &spec.interface,
            Op::DeleteConnection { interface, .. }
            | Op::ActivateConnection { interface }
            | Op::DeactivateConnection { interface } => interface,
        }
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Op::AddConnection { spec } => {
                write!(f, "add {} ({})", spec.interface, spec.kind.as_str())
            }
            Op::UpdateConnection { spec, .. } => {
                write!(f, "update {} ({})", spec.interface, spec.kind.as_str())
            }
            Op::DeleteConnection { interface, .. } => write!(f, "delete {interface}"),
            Op::ActivateConnection { interface } => write!(f, "activate {interface}"),
            Op::DeactivateConnection { interface } => write!(f, "deactivate {interface}"),
        }
    }
}

/// Print a plan the way the installer prints its own (`--print-plan`).
pub fn print_plan(ops: &[Op]) {
    for (i, op) in ops.iter().enumerate() {
        println!("{:>3}. {op}", i + 1);
    }
}

/// The connection profile ids the installer writes for the management bridge.
/// Kept stable so a runtime change rewrites the installer's files instead of
/// leaving a second profile behind that also autoconnects.
pub const MANAGEMENT_ID: &str = "management";
pub const MANAGEMENT_PORT_ID: &str = "management-port";

/// The MTU a port must carry to serve a controller asking for `controller`.
///
/// A bond or bridge cannot pass a frame larger than its smallest port will
/// take, and NetworkManager does not infer this: a port profile with no MTU
/// of its own is written at the 1500 default, so a 9000 bond over untouched
/// ports is a link that completes a TCP handshake and then silently drops
/// every full-sized packet — the resync that never advances past 0.00, the
/// promotion that times out with no explanation on the wire. The port keeps
/// its own value when that value is larger; it is never lowered here.
fn carried_mtu(controller: Option<u32>, port: Option<u32>) -> Option<u32> {
    match (controller, port) {
        (Some(controller), Some(port)) => Some(controller.max(port)),
        (Some(controller), None) => Some(controller),
        (None, port) => port,
    }
}

/// Every connection profile the desired state implies, keyed by interface.
pub fn specs(desired: &NetworkDesiredState) -> BTreeMap<String, ConnectionSpec> {
    let mut out: BTreeMap<String, ConnectionSpec> = BTreeMap::new();

    for nic in &desired.nics {
        let mut spec = ConnectionSpec::base(&nic.name, LinkKind::Ethernet);
        spec.mtu = nic.mtu;
        spec.ip = nic.ip.clone();
        spec.ethernet = Some(EthernetSpec {
            autoneg: nic.autoneg,
            speed: nic.speed,
            duplex: nic.duplex,
        });
        out.insert(nic.name.clone(), spec);
    }
    for bond in &desired.bonds {
        let mut spec = ConnectionSpec::base(&bond.name, LinkKind::Bond);
        spec.mtu = bond.mtu;
        spec.ip = bond.ip.clone();
        spec.bond = Some(BondSpec {
            mode: bond.mode,
            miimon: bond.miimon,
            lacp_rate: bond.lacp_rate,
            xmit_hash_policy: bond.xmit_hash_policy,
            primary: bond.primary.clone(),
        });
        out.insert(bond.name.clone(), spec);
    }
    for bridge in &desired.bridges {
        let mut spec = ConnectionSpec::base(&bridge.name, LinkKind::Bridge);
        spec.mtu = bridge.mtu;
        spec.ip = bridge.ip.clone();
        spec.bridge = Some(bridge_spec(bridge));
        out.insert(bridge.name.clone(), spec);
    }
    for vlan in &desired.vlans {
        let mut spec = ConnectionSpec::base(&vlan.name, LinkKind::Vlan);
        spec.mtu = vlan.mtu;
        spec.ip = vlan.ip.clone();
        spec.vlan = Some(VlanSpec {
            parent: vlan.parent.clone(),
            id: vlan.vlan_id,
        });
        out.insert(vlan.name.clone(), spec);
    }

    // Enslavement, from the controller's port list rather than the port's own
    // declaration, so the model has exactly one place that says who owns what.
    for bridge in &desired.bridges {
        for port in &bridge.ports {
            if let Some(spec) = out.get_mut(port) {
                spec.controller = Some(bridge.name.clone());
                spec.port_type = Some(PortType::Bridge);
                // A port carries no addresses of its own.
                spec.ip = IpConfig::Disabled;
                spec.mtu = carried_mtu(bridge.mtu, spec.mtu);
            }
        }
    }
    for bond in &desired.bonds {
        for port in &bond.ports {
            if let Some(spec) = out.get_mut(port) {
                spec.controller = Some(bond.name.clone());
                spec.port_type = Some(PortType::Bond);
                spec.ip = IpConfig::Disabled;
                spec.mtu = carried_mtu(bond.mtu, spec.mtu);
            }
        }
    }

    // The management link keeps the installer's profile ids so an upgrade
    // rewrites those files in place rather than leaving a second profile
    // behind that also autoconnects. Its addressing comes from the link like
    // everyone else's.
    if let Some(spec) = out.get_mut(&desired.management.link) {
        spec.id = MANAGEMENT_ID.to_string();
    }
    for port in desired.ports_of(&desired.management.link) {
        if let Some(spec) = out.get_mut(port) {
            spec.id = MANAGEMENT_PORT_ID.to_string();
        }
    }

    out
}

fn bridge_spec(bridge: &Bridge) -> BridgeSpec {
    BridgeSpec {
        stp: bridge.stp,
        forward_delay: bridge.forward_delay,
        vlan_filtering: bridge.vlan_filtering,
        mac_address: bridge.mac_address.clone(),
    }
}

/// Links in dependency order: a controller before its ports, a VLAN's parent
/// before the VLAN. Ties break alphabetically so a plan is reproducible.
/// A cycle (which validation rejects) degrades to alphabetical rather than
/// looping — planning is never the place that hangs.
fn topological(desired: &NetworkDesiredState) -> Vec<String> {
    let all: BTreeSet<String> = desired
        .link_kinds()
        .into_iter()
        .map(|(name, _)| name.to_string())
        .collect();

    // needs[x] = links that must be configured before x.
    let mut needs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for name in &all {
        needs.insert(name.clone(), BTreeSet::new());
    }
    for bridge in &desired.bridges {
        for port in &bridge.ports {
            if let Some(set) = needs.get_mut(port) {
                set.insert(bridge.name.clone());
            }
        }
    }
    for bond in &desired.bonds {
        for port in &bond.ports {
            if let Some(set) = needs.get_mut(port) {
                set.insert(bond.name.clone());
            }
        }
    }
    for vlan in &desired.vlans {
        if let Some(set) = needs.get_mut(&vlan.name) {
            if all.contains(&vlan.parent) {
                set.insert(vlan.parent.clone());
            }
        }
    }

    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut order: Vec<String> = Vec::new();
    while order.len() < all.len() {
        let ready: Vec<String> = all
            .iter()
            .filter(|name| !done.contains(*name))
            .filter(|name| needs[*name].iter().all(|dep| done.contains(dep)))
            .cloned()
            .collect();
        if ready.is_empty() {
            // Cycle: emit whatever is left alphabetically and let validation
            // be the thing that complains.
            for name in &all {
                if !done.contains(name) {
                    order.push(name.clone());
                }
            }
            break;
        }
        for name in ready {
            done.insert(name.clone());
            order.push(name);
        }
    }
    order
}

/// Move every op that touches the management link to the end, keeping the
/// relative order of both groups.
fn management_last(ops: Vec<Op>, management: &str) -> Vec<Op> {
    let (mgmt, rest): (Vec<Op>, Vec<Op>) =
        ops.into_iter().partition(|op| op.interface() == management);
    rest.into_iter().chain(mgmt).collect()
}

/// Does the box already look the way this spec asks for, closely enough that
/// reactivating the device would be pointless churn?
///
/// Only NetworkManager-observable properties are compared. Settings NM does
/// not report per device (STP, miimon, hash policy, link autonegotiation) are
/// always written by the Add/Update op, so they still reach the box — they
/// just do not, on their own, force a reactivation. That is deliberate: those
/// settings take effect on the next activation of the link they belong to,
/// and none of them is worth dropping the operator's session for.
fn activation_needed(spec: &ConnectionSpec, observed: &ObservedState) -> bool {
    let Some(link) = observed.link(&spec.interface) else {
        return true; // not on the box yet — a bridge, bond, or VLAN to create
    };

    let differs = link.controller.as_deref() != spec.controller.as_deref()
        || (spec.mtu.is_some() && link.mtu != spec.mtu)
        || match &spec.ip {
            IpConfig::Disabled => !link.addresses.is_empty(),
            IpConfig::Dhcp => link.addresses.is_empty(),
            IpConfig::Static { cidr, gateway, .. } => {
                !link.addresses.iter().any(|a| a == cidr)
                    || link.gateway.as_deref().unwrap_or_default() != gateway
            }
        };
    if differs {
        return true;
    }

    // A physical NIC with nothing asked of it — no controller, no address —
    // is left exactly as the box has it. Writing its profile is enough;
    // forcing an unused link up is a change nobody requested, and on first
    // seed that would be every NIC in the machine at once.
    let idle = spec.kind == LinkKind::Ethernet
        && spec.controller.is_none()
        && spec.ip == IpConfig::Disabled;
    !link.state.is_up() && !idle
}

/// Build the ordered operation list that takes `observed` to `desired`.
pub fn plan(desired: &NetworkDesiredState, observed: &ObservedState) -> Vec<Op> {
    let specs = specs(desired);
    let order = topological(desired);
    let management = desired.management.link.as_str();

    // --- links that disappear -------------------------------------------
    // A link is removed if the box has a managed connection for it and the
    // desired state no longer mentions it. Physical NICs are never removed;
    // an unconfigured NIC just has no profile.
    let removed: Vec<&crate::state::ObservedLink> = observed
        .links
        .iter()
        .filter(|link| !specs.contains_key(&link.name))
        .filter(|link| link.connection_uuid.is_some())
        .filter(|link| {
            matches!(
                link.kind,
                LinkKind::Bridge | LinkKind::Bond | LinkKind::Vlan | LinkKind::Ethernet
            )
        })
        .collect();

    // Ports first, controllers after: deleting a controller while a port
    // still references it leaves the port's profile pointing at nothing.
    let mut removal_order: Vec<&crate::state::ObservedLink> = removed.clone();
    removal_order.sort_by_key(|link| {
        let is_controller = matches!(link.kind, LinkKind::Bridge | LinkKind::Bond);
        (is_controller, link.name.clone())
    });

    let mut deactivations: Vec<Op> = Vec::new();
    let mut deletions: Vec<Op> = Vec::new();
    for link in &removal_order {
        if link.state.is_up() {
            deactivations.push(Op::DeactivateConnection {
                interface: link.name.clone(),
            });
        }
        deletions.push(Op::DeleteConnection {
            uuid: link.connection_uuid.clone(),
            interface: link.name.clone(),
        });
    }

    // A port that stays but leaves its controller has to be released before
    // the controller is reconfigured, or NM re-attaches it on activation.
    for link in &observed.links {
        let Some(spec) = specs.get(&link.name) else {
            continue;
        };
        let leaving = link.controller.is_some() && link.controller != spec.controller;
        if leaving && link.state.is_up() {
            deactivations.push(Op::DeactivateConnection {
                interface: link.name.clone(),
            });
        }
    }

    // --- profiles --------------------------------------------------------
    let mut writes: Vec<Op> = Vec::new();
    let mut activations: Vec<Op> = Vec::new();
    for name in &order {
        let Some(spec) = specs.get(name) else {
            continue;
        };
        let existing = observed.link(name).and_then(|l| l.connection_uuid.clone());
        writes.push(match existing {
            Some(uuid) => Op::UpdateConnection {
                uuid: Some(uuid),
                spec: spec.clone(),
            },
            None => Op::AddConnection { spec: spec.clone() },
        });
        if activation_needed(spec, observed) {
            activations.push(Op::ActivateConnection {
                interface: name.clone(),
            });
        }
    }

    let mut ops = Vec::new();
    ops.extend(management_last(deactivations, management));
    ops.extend(management_last(deletions, management));
    ops.extend(management_last(writes, management));
    ops.extend(management_last(activations, management));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Bond, Bridge, ManagementRef, Nic, Vlan};
    use crate::state::{LinkState, ObservedLink};

    fn nic(name: &str) -> Nic {
        Nic {
            name: name.into(),
            ..Nic::default()
        }
    }

    fn management_ip() -> IpConfig {
        IpConfig::Static {
            cidr: "192.168.10.5/24".into(),
            gateway: "192.168.10.1".into(),
            dns: vec!["9.9.9.9".into()],
        }
    }

    fn observed_nic(name: &str, uuid: Option<&str>) -> ObservedLink {
        ObservedLink {
            name: name.into(),
            kind: LinkKind::Ethernet,
            state: if uuid.is_some() {
                LinkState::Activated
            } else {
                LinkState::Disconnected
            },
            carrier: true,
            mtu: Some(1500),
            connection_uuid: uuid.map(String::from),
            ..ObservedLink::default()
        }
    }

    /// nic0 configured and carrying the management address, nic1 idle.
    fn observed() -> ObservedState {
        let mut nic0 = observed_nic("nic0", Some("uuid-nic0"));
        nic0.addresses = vec!["192.168.10.5/24".into()];
        nic0.gateway = Some("192.168.10.1".into());
        ObservedState {
            node: "lumen".into(),
            links: vec![nic0, observed_nic("nic1", None)],
        }
    }

    fn flat() -> NetworkDesiredState {
        NetworkDesiredState {
            nics: vec![
                Nic {
                    ip: management_ip(),
                    ..nic("nic0")
                },
                nic("nic1"),
            ],
            management: ManagementRef {
                link: "nic0".into(),
                ..ManagementRef::default()
            },
            ..NetworkDesiredState::default()
        }
    }

    fn names(ops: &[Op]) -> Vec<String> {
        ops.iter().map(|op| op.to_string()).collect()
    }

    fn position(ops: &[Op], needle: &str) -> usize {
        names(ops)
            .iter()
            .position(|s| s == needle)
            .unwrap_or_else(|| panic!("plan has no {needle:?}: {:?}", names(ops)))
    }

    #[test]
    fn steady_state_writes_profiles_but_activates_nothing() {
        let ops = plan(&flat(), &observed());
        assert!(
            !ops.iter()
                .any(|op| matches!(op, Op::ActivateConnection { .. })),
            "nothing changed observably: {:?}",
            names(&ops)
        );
        // nic1 has no profile yet, so it is added; nic0 already has one.
        assert!(names(&ops).contains(&"add nic1 (ethernet)".to_string()));
        assert!(names(&ops).contains(&"update nic0 (ethernet)".to_string()));
    }

    #[test]
    fn an_unused_nic_is_given_a_profile_but_not_forced_up() {
        // The first apply on a fresh appliance must not bring every idle NIC
        // in the machine up as a side effect of seeding.
        let ops = plan(&flat(), &observed());
        assert!(!names(&ops).contains(&"activate nic1".to_string()));

        // …but as soon as something is asked of it, it comes up.
        let mut desired = flat();
        desired.nics[1].mtu = Some(9000);
        let ops = plan(&desired, &observed());
        assert!(names(&ops).contains(&"activate nic1".to_string()));
    }

    #[test]
    fn controllers_are_created_before_their_ports() {
        let mut desired = flat();
        desired.bridges.push(Bridge {
            name: "br0".into(),
            ports: vec!["nic1".into()],
            ..Bridge::default()
        });
        let ops = plan(&desired, &observed());
        assert!(position(&ops, "add br0 (bridge)") < position(&ops, "add nic1 (ethernet)"));
        assert!(
            position(&ops, "activate br0") < position(&ops, "activate nic1"),
            "{:?}",
            names(&ops)
        );
    }

    /// A controller's MTU reaches the ports that actually carry the frames.
    ///
    /// The bug this pins cost a bring-up: a 9000 bond over two ports left at
    /// the 1500 default connected, authenticated, exchanged a compressed
    /// bitmap — everything small — and then moved zero bytes of resync,
    /// while promotion failed as a 30-second state-change timeout with
    /// nothing on the wire to explain it.
    #[test]
    fn a_ports_mtu_rises_to_the_controller_it_serves() {
        let mut desired = flat();
        desired.bonds.push(Bond {
            name: "bond0".into(),
            ports: vec!["nic1".into()],
            mtu: Some(9000),
            ..Bond::default()
        });
        let bonded = specs(&desired);
        assert_eq!(bonded["bond0"].mtu, Some(9000));
        assert_eq!(
            bonded["nic1"].mtu,
            Some(9000),
            "a port left at the default silently drops every full-sized frame"
        );

        // A bridge port is the same physics.
        let mut desired = flat();
        desired.bridges.push(Bridge {
            name: "br0".into(),
            ports: vec!["nic1".into()],
            mtu: Some(9000),
            ..Bridge::default()
        });
        assert_eq!(specs(&desired)["nic1"].mtu, Some(9000));
    }

    /// A port that already asks for more keeps it — the controller's value is
    /// a floor, not an assignment.
    #[test]
    fn a_ports_own_larger_mtu_is_never_lowered_to_its_controllers() {
        let mut desired = flat();
        desired.nics[1].mtu = Some(9000);
        desired.bonds.push(Bond {
            name: "bond0".into(),
            ports: vec![desired.nics[1].name.clone()],
            mtu: Some(1500),
            ..Bond::default()
        });
        let name = desired.nics[1].name.clone();
        assert_eq!(specs(&desired)[&name].mtu, Some(9000));
    }

    #[test]
    fn a_vlan_comes_after_its_parent() {
        let mut desired = flat();
        desired.bonds.push(Bond {
            name: "bond0".into(),
            ports: vec!["nic1".into()],
            ..Bond::default()
        });
        desired.vlans.push(Vlan {
            name: "vlan100".into(),
            parent: "bond0".into(),
            vlan_id: 100,
            ..Vlan::default()
        });
        let ops = plan(&desired, &observed());
        assert!(position(&ops, "add bond0 (bond)") < position(&ops, "add vlan100 (vlan)"));
        assert!(position(&ops, "add bond0 (bond)") < position(&ops, "add nic1 (ethernet)"));
    }

    #[test]
    fn the_management_link_is_touched_last() {
        let mut desired = flat();
        desired.bridges.push(Bridge {
            name: "br0".into(),
            ports: vec!["nic1".into()],
            ..Bridge::default()
        });
        desired.nics[0].mtu = Some(9000); // forces nic0 to reactivate
        let ops = plan(&desired, &observed());
        let last_write = ops
            .iter()
            .rposition(|op| matches!(op, Op::AddConnection { .. } | Op::UpdateConnection { .. }))
            .unwrap();
        assert_eq!(ops[last_write].interface(), "nic0");
        assert_eq!(ops.last().unwrap().interface(), "nic0");
    }

    #[test]
    fn removing_a_bridge_deactivates_its_port_before_deleting_the_bridge() {
        // The box already has br0 with nic1 in it; desired drops both.
        let mut obs = observed();
        obs.links.push(ObservedLink {
            name: "br0".into(),
            kind: LinkKind::Bridge,
            state: LinkState::Activated,
            ports: vec!["nic1".into()],
            connection_uuid: Some("uuid-br0".into()),
            ..ObservedLink::default()
        });
        obs.links[1].controller = Some("br0".into());
        obs.links[1].connection_uuid = Some("uuid-nic1".into());
        obs.links[1].state = LinkState::Activated;

        let mut desired = flat();
        desired.nics.retain(|n| n.name != "nic1");

        let ops = plan(&desired, &obs);
        assert!(position(&ops, "deactivate nic1") < position(&ops, "delete br0"));
        assert!(position(&ops, "delete nic1") < position(&ops, "delete br0"));
        assert!(position(&ops, "delete br0") < position(&ops, "update nic0 (ethernet)"));
    }

    #[test]
    fn a_port_leaving_its_controller_is_released_first() {
        let mut obs = observed();
        obs.links.push(ObservedLink {
            name: "br0".into(),
            kind: LinkKind::Bridge,
            state: LinkState::Activated,
            ports: vec!["nic1".into()],
            connection_uuid: Some("uuid-br0".into()),
            ..ObservedLink::default()
        });
        obs.links[1].controller = Some("br0".into());
        obs.links[1].connection_uuid = Some("uuid-nic1".into());
        obs.links[1].state = LinkState::Activated;

        // br0 stays, nic1 leaves it.
        let mut desired = flat();
        desired.bridges.push(Bridge {
            name: "br0".into(),
            ports: vec![],
            ..Bridge::default()
        });
        let ops = plan(&desired, &obs);
        assert!(position(&ops, "deactivate nic1") < position(&ops, "update nic1 (ethernet)"));
    }

    #[test]
    fn the_management_link_gets_the_installers_profile_ids() {
        let mut desired = flat();
        desired.bridges.push(Bridge {
            name: "br0".into(),
            ports: vec!["nic0".into()],
            ip: management_ip(),
            mac_address: Some("52:54:00:11:22:33".into()),
            ..Bridge::default()
        });
        desired.set_ip("nic0", IpConfig::Disabled);
        desired.management.link = "br0".into();
        let specs = specs(&desired);
        assert_eq!(specs["br0"].id, MANAGEMENT_ID);
        assert_eq!(specs["nic0"].id, MANAGEMENT_PORT_ID);
        assert_eq!(specs["nic0"].controller.as_deref(), Some("br0"));
        assert_eq!(specs["nic0"].port_type, Some(PortType::Bridge));
        // Only the bridge holds the address.
        assert_eq!(specs["nic0"].ip, IpConfig::Disabled);
        assert!(matches!(specs["br0"].ip, IpConfig::Static { .. }));
        assert_eq!(
            specs["br0"].bridge.as_ref().unwrap().mac_address.as_deref(),
            Some("52:54:00:11:22:33")
        );
    }

    #[test]
    fn changing_only_an_unrelated_link_does_not_reactivate_management() {
        let mut desired = flat();
        desired.bridges.push(Bridge {
            name: "br0".into(),
            ports: vec!["nic1".into()],
            ..Bridge::default()
        });
        let ops = plan(&desired, &observed());
        assert!(
            !names(&ops).contains(&"activate nic0".to_string()),
            "management must not blip for an unrelated change: {:?}",
            names(&ops)
        );
    }

    #[test]
    fn changing_the_management_address_does_reactivate_it() {
        let mut desired = flat();
        desired.set_ip(
            "nic0",
            IpConfig::Static {
                cidr: "192.168.10.9/24".into(),
                gateway: "192.168.10.1".into(),
                dns: vec![],
            },
        );
        let ops = plan(&desired, &observed());
        assert!(names(&ops).contains(&"activate nic0".to_string()));
    }

    /// Addressing follows the link, not the management reference: a second
    /// bridge on a storage VLAN gets its address without anything moving.
    #[test]
    fn any_link_can_carry_an_address_of_its_own() {
        let mut desired = flat();
        desired.bridges.push(Bridge {
            name: "br1".into(),
            ports: vec!["nic1".into()],
            ip: IpConfig::Static {
                cidr: "10.0.0.5/24".into(),
                gateway: "10.0.0.1".into(),
                dns: vec![],
            },
            ..Bridge::default()
        });
        let specs = specs(&desired);
        assert!(matches!(specs["br1"].ip, IpConfig::Static { .. }));
        // The management link keeps its own, and the port keeps none.
        assert!(matches!(specs["nic0"].ip, IpConfig::Static { .. }));
        assert_eq!(specs["nic1"].ip, IpConfig::Disabled);
        assert_eq!(specs["nic0"].id, MANAGEMENT_ID);
    }

    #[test]
    fn a_cycle_plans_without_hanging() {
        let mut desired = flat();
        desired.bridges.push(Bridge {
            name: "br0".into(),
            ports: vec!["bond0".into()],
            ..Bridge::default()
        });
        desired.bonds.push(Bond {
            name: "bond0".into(),
            ports: vec!["br0".into()],
            ..Bond::default()
        });
        let ops = plan(&desired, &observed());
        assert!(names(&ops).iter().any(|s| s.starts_with("add br0")));
        assert!(names(&ops).iter().any(|s| s.starts_with("add bond0")));
    }

    #[test]
    fn ops_round_trip_through_serde() {
        let ops = plan(&flat(), &observed());
        let json = serde_json::to_string(&ops).unwrap();
        let back: Vec<Op> = serde_json::from_str(&json).unwrap();
        assert_eq!(ops, back);
    }
}
