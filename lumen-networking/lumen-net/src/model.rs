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

/// The management address and the single link that carries it. Exactly one
/// link has an IP configuration in this stage — guest networks come later.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementRef {
    /// Link name: `nic0`, `br0`, `bond0`, `vlan100`, …
    pub link: String,
    pub ip: IpConfig,
}

/// Layer-3 configuration of a link.
///
/// The `Dhcp`/`Static` arms are wire-compatible with the installer's
/// `NetworkConfig` (same `mode` tag, same field names) so an installed
/// appliance's management settings round-trip through both. See
/// `ip_config_matches_installer_network_config` below and docs/networking.md.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum IpConfig {
    #[default]
    Dhcp,
    Static {
        /// Address in CIDR form, e.g. "192.168.10.5/24".
        cidr: String,
        gateway: String,
        #[serde(default)]
        dns: Vec<String>,
    },
    Disabled,
}

/// A physical NIC. Never created or destroyed — only configured. The name is
/// whatever lumen-nicnames pinned (nic0..nicN).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Nic {
    pub name: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bridge {
    pub name: String,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub stp: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_delay: Option<u32>,
    /// Present now so a later stage that turns it on is a config change and
    /// not a schema migration. Nothing consumes it yet.
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
