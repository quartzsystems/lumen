//! Desired state: what the operator wants the box to look like.
//!
//! Every type is `deny_unknown_fields` — a typo in an API request is a 400,
//! not a silently ignored setting. Nothing here knows about NetworkManager;
//! the mapping to connection settings lives in `backend::nm`.

use serde::{Deserialize, Serialize};

/// The complete desired configuration of one node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDesiredState {
    #[serde(default)]
    pub nics: Vec<Nic>,
    #[serde(default)]
    pub bonds: Vec<Bond>,
    #[serde(default)]
    pub vlans: Vec<Vlan>,
    #[serde(default)]
    pub bridges: Vec<Bridge>,
    /// Which link carries the management address, and how it is addressed.
    pub management: ManagementRef,
}

/// Which link the console is reached on. Addressing itself lives on the link
/// (every link carries its own [`IpConfig`]); this only says which of them the
/// appliance must not be allowed to strand.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementRef {
    /// Link name: `nic0`, `br0`, `bond0`, `vlan100`, …
    pub link: String,
    /// Where the management addressing used to live, before links carried
    /// their own. Read so an appliance upgraded in place keeps its committed
    /// configuration; folded onto the link by [`NetworkDesiredState::migrate`]
    /// and never written again.
    #[serde(default, rename = "ip", skip_serializing)]
    pub legacy_ip: Option<IpConfig>,
}

/// Layer-3 configuration of a link.
///
/// The `Dhcp`/`Static` arms are wire-compatible with the installer's
/// `NetworkConfig` (same `mode` tag, same field names) so an installed
/// appliance's management settings round-trip through both. See
/// `ip_config_matches_installer_network_config` below and docs/networking.md.
/// The default is `Disabled`, not `Dhcp`: every link carries one of these now,
/// and a link nobody has configured must not quietly start asking for a lease.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum IpConfig {
    Dhcp,
    Static {
        /// Address in CIDR form, e.g. "192.168.10.5/24".
        cidr: String,
        gateway: String,
        #[serde(default)]
        dns: Vec<String>,
    },
    #[default]
    Disabled,
}

impl IpConfig {
    /// Anything that puts an address on the link.
    pub fn is_addressed(&self) -> bool {
        !matches!(self, IpConfig::Disabled)
    }
}

/// A physical NIC. Never created or destroyed — only configured. The name is
/// whatever lumen-nicnames pinned (nic0..nicN).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Nic {
    pub name: String,
    /// Layer-3 configuration. `Disabled` for a NIC that is only ever a port.
    #[serde(default)]
    pub ip: IpConfig,
    /// Free text the operator wrote about this link; the table's Description
    /// column. Never reaches NetworkManager — it lives in `desired.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    /// `None` leaves the driver default (which is autonegotiation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoneg: Option<bool>,
    /// Forced link speed in Mb/s. Only meaningful with `autoneg: false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplex: Option<Duplex>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Duplex {
    Full,
    Half,
}

impl Duplex {
    pub fn as_str(self) -> &'static str {
        match self {
            Duplex::Full => "full",
            Duplex::Half => "half",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bond {
    pub name: String,
    pub mode: BondMode,
    #[serde(default)]
    pub ip: IpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub miimon: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lacp_rate: Option<LacpRate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xmit_hash_policy: Option<XmitHashPolicy>,
    /// Preferred port in active-backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
}

/// Bonding modes Lumen offers. The serialized spelling is the kernel's (and
/// NetworkManager's) own — `802.3ad` — with `802-3ad` accepted on input for
/// callers that dislike a dot in an enum token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BondMode {
    #[default]
    #[serde(rename = "active-backup")]
    ActiveBackup,
    #[serde(rename = "802.3ad", alias = "802-3ad")]
    Ieee8023ad,
    #[serde(rename = "balance-xor")]
    BalanceXor,
}

impl BondMode {
    /// The value NetworkManager's `bond.options["mode"]` expects.
    pub fn as_str(self) -> &'static str {
        match self {
            BondMode::ActiveBackup => "active-backup",
            BondMode::Ieee8023ad => "802.3ad",
            BondMode::BalanceXor => "balance-xor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LacpRate {
    Slow,
    Fast,
}

impl LacpRate {
    pub fn as_str(self) -> &'static str {
        match self {
            LacpRate::Slow => "slow",
            LacpRate::Fast => "fast",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum XmitHashPolicy {
    #[serde(rename = "layer2")]
    Layer2,
    #[serde(rename = "layer2+3")]
    Layer2Plus3,
    #[serde(rename = "layer3+4")]
    Layer3Plus4,
}

impl XmitHashPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            XmitHashPolicy::Layer2 => "layer2",
            XmitHashPolicy::Layer2Plus3 => "layer2+3",
            XmitHashPolicy::Layer3Plus4 => "layer3+4",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vlan {
    pub name: String,
    /// The link the VLAN rides on: a NIC, a bond, or a bridge.
    pub parent: String,
    pub vlan_id: u16,
    #[serde(default)]
    pub ip: IpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bridge {
    pub name: String,
    #[serde(default)]
    pub ip: IpConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub stp: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_delay: Option<u32>,
    /// "VLAN aware" in the console: the bridge passes tagged frames through to
    /// its ports instead of one VLAN interface per tag.
    #[serde(default)]
    pub vlan_filtering: bool,
    /// Pinned MAC. A Linux bridge otherwise inherits the lowest MAC among its
    /// ports, so adding a second port silently moves the management MAC and
    /// breaks DHCP reservations and switch-side port security.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
}

/// What a link is. Used by both desired and observed state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    Ethernet,
    Bond,
    Bridge,
    Vlan,
    /// Anything the appliance did not create and does not manage (loopback,
    /// guest taps, tunnels from a later stage).
    #[default]
    Other,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Ethernet => "ethernet",
            LinkKind::Bond => "bond",
            LinkKind::Bridge => "bridge",
            LinkKind::Vlan => "vlan",
            LinkKind::Other => "other",
        }
    }
}

impl NetworkDesiredState {
    /// Every link the desired state names, with its kind — the lookup the
    /// validator and the planner both need.
    pub fn link_kinds(&self) -> Vec<(&str, LinkKind)> {
        let nics = self
            .nics
            .iter()
            .map(|n| (n.name.as_str(), LinkKind::Ethernet));
        let bonds = self.bonds.iter().map(|b| (b.name.as_str(), LinkKind::Bond));
        let bridges = self
            .bridges
            .iter()
            .map(|b| (b.name.as_str(), LinkKind::Bridge));
        let vlans = self.vlans.iter().map(|v| (v.name.as_str(), LinkKind::Vlan));
        nics.chain(bonds).chain(bridges).chain(vlans).collect()
    }

    pub fn kind_of(&self, name: &str) -> Option<LinkKind> {
        self.link_kinds()
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, k)| k)
    }

    /// The controller (bridge or bond) a link is enslaved to, if any. More
    /// than one match is a validation error, not a panic — the validator
    /// reports it; this returns the first for planning purposes.
    pub fn controller_of(&self, name: &str) -> Option<&str> {
        self.controllers_of(name).into_iter().next()
    }

    pub fn controllers_of(&self, name: &str) -> Vec<&str> {
        let bridges = self
            .bridges
            .iter()
            .filter(|b| b.ports.iter().any(|p| p == name))
            .map(|b| b.name.as_str());
        let bonds = self
            .bonds
            .iter()
            .filter(|b| b.ports.iter().any(|p| p == name))
            .map(|b| b.name.as_str());
        bridges.chain(bonds).collect()
    }

    pub fn bridge(&self, name: &str) -> Option<&Bridge> {
        self.bridges.iter().find(|b| b.name == name)
    }

    pub fn bond(&self, name: &str) -> Option<&Bond> {
        self.bonds.iter().find(|b| b.name == name)
    }

    pub fn vlan(&self, name: &str) -> Option<&Vlan> {
        self.vlans.iter().find(|v| v.name == name)
    }

    pub fn nic(&self, name: &str) -> Option<&Nic> {
        self.nics.iter().find(|n| n.name == name)
    }

    /// The layer-3 configuration of a link, whatever kind it is. A name
    /// nothing defines has no addressing.
    pub fn ip_of(&self, name: &str) -> IpConfig {
        self.nic(name)
            .map(|n| n.ip.clone())
            .or_else(|| self.bond(name).map(|b| b.ip.clone()))
            .or_else(|| self.bridge(name).map(|b| b.ip.clone()))
            .or_else(|| self.vlan(name).map(|v| v.ip.clone()))
            .unwrap_or_default()
    }

    /// Set a link's layer-3 configuration. Returns false when nothing by that
    /// name is defined.
    pub fn set_ip(&mut self, name: &str, ip: IpConfig) -> bool {
        if let Some(nic) = self.nics.iter_mut().find(|n| n.name == name) {
            nic.ip = ip;
            return true;
        }
        if let Some(bond) = self.bonds.iter_mut().find(|b| b.name == name) {
            bond.ip = ip;
            return true;
        }
        if let Some(bridge) = self.bridges.iter_mut().find(|b| b.name == name) {
            bridge.ip = ip;
            return true;
        }
        if let Some(vlan) = self.vlans.iter_mut().find(|v| v.name == name) {
            vlan.ip = ip;
            return true;
        }
        false
    }

    pub fn comment_of(&self, name: &str) -> Option<&str> {
        self.nic(name)
            .and_then(|n| n.comment.as_deref())
            .or_else(|| self.bond(name).and_then(|b| b.comment.as_deref()))
            .or_else(|| self.bridge(name).and_then(|b| b.comment.as_deref()))
            .or_else(|| self.vlan(name).and_then(|v| v.comment.as_deref()))
    }

    /// Fold a document written before links carried their own addressing into
    /// the current shape. Called on every read from the store, so an appliance
    /// upgraded in place keeps its committed management address instead of
    /// silently re-seeding from the box.
    pub fn migrate(&mut self) {
        let Some(ip) = self.management.legacy_ip.take() else {
            return;
        };
        let link = self.management.link.clone();
        if link.is_empty() {
            return;
        }
        // Only if the link has nothing of its own: a document that already
        // carries per-link addressing is the newer of the two.
        if !self.ip_of(&link).is_addressed() {
            self.set_ip(&link, ip);
        }
    }

    /// Ports of a link, whatever kind of controller it is.
    pub fn ports_of(&self, name: &str) -> &[String] {
        if let Some(bridge) = self.bridge(name) {
            return &bridge.ports;
        }
        if let Some(bond) = self.bond(name) {
            return &bond.ports;
        }
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bond_mode_accepts_both_spellings() {
        let dotted: BondMode = serde_json::from_str("\"802.3ad\"").unwrap();
        let dashed: BondMode = serde_json::from_str("\"802-3ad\"").unwrap();
        assert_eq!(dotted, BondMode::Ieee8023ad);
        assert_eq!(dashed, BondMode::Ieee8023ad);
        // One spelling on the wire, and it is the one NetworkManager uses.
        assert_eq!(serde_json::to_string(&dotted).unwrap(), "\"802.3ad\"");
        assert_eq!(BondMode::Ieee8023ad.as_str(), "802.3ad");
    }

    /// The installer writes its management settings as
    /// `lumen_installer::config::NetworkConfig`; the control plane reads and
    /// rewrites the same configuration as `IpConfig`. The two crates do not
    /// depend on each other (see docs/networking.md), so this test pins the
    /// wire format both sides must keep producing. The installer has the
    /// mirror-image test (`network_config_wire_format_matches_lumen_net`).
    #[test]
    fn ip_config_matches_installer_network_config() {
        let dhcp = serde_json::json!({ "mode": "dhcp" });
        let static_json = serde_json::json!({
            "mode": "static",
            "cidr": "192.168.10.5/24",
            "gateway": "192.168.10.1",
            "dns": ["9.9.9.9"],
        });

        assert_eq!(serde_json::to_value(IpConfig::Dhcp).unwrap(), dhcp);
        assert_eq!(
            serde_json::to_value(IpConfig::Static {
                cidr: "192.168.10.5/24".into(),
                gateway: "192.168.10.1".into(),
                dns: vec!["9.9.9.9".into()],
            })
            .unwrap(),
            static_json
        );
        // …and back, so an installer-written value parses here unchanged.
        let back: IpConfig = serde_json::from_value(static_json).unwrap();
        match back {
            IpConfig::Static { cidr, gateway, dns } => {
                assert_eq!(cidr, "192.168.10.5/24");
                assert_eq!(gateway, "192.168.10.1");
                assert_eq!(dns, vec!["9.9.9.9".to_string()]);
            }
            other => panic!("expected static, got {other:?}"),
        }
    }

    /// An appliance upgraded in place has `{"management":{"link":"br0","ip":…}}`
    /// on disk. Losing that would re-seed from the box and drop every comment
    /// and every setting NetworkManager does not report back.
    #[test]
    fn a_pre_per_link_document_migrates_onto_the_link() {
        let mut state: NetworkDesiredState = serde_json::from_str(
            r#"{
                "bridges": [{"name":"br0","ports":["nic0"]}],
                "nics": [{"name":"nic0"}],
                "management": {
                    "link": "br0",
                    "ip": {"mode":"static","cidr":"192.168.10.5/24","gateway":"192.168.10.1"}
                }
            }"#,
        )
        .unwrap();
        state.migrate();
        assert!(state.management.legacy_ip.is_none());
        match state.ip_of("br0") {
            IpConfig::Static { cidr, gateway, .. } => {
                assert_eq!(cidr, "192.168.10.5/24");
                assert_eq!(gateway, "192.168.10.1");
            }
            other => panic!("expected the address to land on br0, got {other:?}"),
        }
        // The port keeps none of it.
        assert_eq!(state.ip_of("nic0"), IpConfig::Disabled);
        // …and it is never written back out.
        let json = serde_json::to_value(&state).unwrap();
        assert!(json["management"].get("ip").is_none(), "{json}");
    }

    #[test]
    fn a_link_nobody_configured_has_no_addressing() {
        assert_eq!(IpConfig::default(), IpConfig::Disabled);
        assert_eq!(Nic::default().ip, IpConfig::Disabled);
        assert!(!IpConfig::Disabled.is_addressed());
        assert!(IpConfig::Dhcp.is_addressed());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = serde_json::from_str::<Bridge>(r#"{"name":"br0","stpp":true}"#);
        assert!(err.is_err(), "typo must not be silently ignored");
    }

    #[test]
    fn controllers_of_finds_bridge_and_bond() {
        let state = NetworkDesiredState {
            bonds: vec![Bond {
                name: "bond0".into(),
                ports: vec!["nic0".into()],
                ..Bond::default()
            }],
            bridges: vec![Bridge {
                name: "br0".into(),
                ports: vec!["nic0".into()],
                ..Bridge::default()
            }],
            ..NetworkDesiredState::default()
        };
        let mut controllers = state.controllers_of("nic0");
        controllers.sort_unstable();
        assert_eq!(controllers, vec!["bond0", "br0"]);
        assert!(state.controllers_of("nic1").is_empty());
    }
}
