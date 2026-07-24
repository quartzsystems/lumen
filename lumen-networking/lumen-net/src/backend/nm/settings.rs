//! Building and reading NetworkManager's `a{sa{sv}}` connection settings.
//!
//! One place knows that shape. Everything above the backend speaks
//! [`ConnectionSpec`]; everything here speaks D-Bus. Without this the nested
//! `HashMap<String, HashMap<String, Value>>` leaks into every call site and
//! the property names stop being greppable.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use zbus::zvariant::{OwnedValue, Value};

use crate::model::{IpConfig, LinkKind};
use crate::plan::ConnectionSpec;

use super::proxy::SettingsMap;

/// NetworkManager setting-group names, spelled once.
pub const CONNECTION: &str = "connection";
pub const WIRED: &str = "802-3-ethernet";
pub const BRIDGE: &str = "bridge";
pub const BOND: &str = "bond";
pub const VLAN: &str = "vlan";
pub const IPV4: &str = "ipv4";
pub const IPV6: &str = "ipv6";

/// `connection.type` values.
fn nm_type(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::Ethernet => WIRED,
        LinkKind::Bond => BOND,
        LinkKind::Bridge => BRIDGE,
        LinkKind::Vlan => VLAN,
        LinkKind::Other => WIRED,
    }
}

/// A settings map under construction. Insertion is infallible for the value
/// types we use, so the builder does not thread a Result through every line.
#[derive(Debug, Default)]
pub struct SettingsBuilder {
    map: SettingsMap,
}

impl SettingsBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set<'v>(&mut self, group: &str, key: &str, value: impl Into<Value<'v>>) -> &mut Self {
        let value: Value<'v> = value.into();
        // OwnedValue::try_from only fails for values holding a file
        // descriptor, which nothing here constructs.
        if let Ok(owned) = OwnedValue::try_from(value) {
            self.map
                .entry(group.to_string())
                .or_default()
                .insert(key.to_string(), owned);
        }
        self
    }

    /// Ensure a group exists even when it carries no properties (NM treats a
    /// missing `bridge` group on a bridge connection as an error).
    pub fn group(&mut self, group: &str) -> &mut Self {
        self.map.entry(group.to_string()).or_default();
        self
    }

    pub fn build(self) -> SettingsMap {
        self.map
    }
}

/// "52:54:00:aa:bb:cc" -> the `ay` NetworkManager wants. Every MAC-typed
/// property in NM serializes as a byte array, whichever setting it lives in.
fn mac_bytes(mac: &str) -> Result<Vec<u8>> {
    let bytes: Result<Vec<u8>> = mac
        .split(':')
        .map(|octet| {
            u8::from_str_radix(octet, 16).with_context(|| format!("bad MAC octet {octet:?}"))
        })
        .collect();
    let bytes = bytes?;
    if bytes.len() != 6 {
        return Err(anyhow!("{mac:?} is not a 6-octet MAC address"));
    }
    Ok(bytes)
}

/// "192.168.10.5/24" -> ("192.168.10.5", 24).
fn split_cidr(cidr: &str) -> Result<(&str, u32)> {
    let (address, prefix) = cidr
        .split_once('/')
        .ok_or_else(|| anyhow!("{cidr:?} is not an address in CIDR form"))?;
    let prefix: u32 = prefix
        .parse()
        .with_context(|| format!("bad prefix length in {cidr:?}"))?;
    if prefix > 32 {
        return Err(anyhow!(
            "prefix length {prefix} is out of range in {cidr:?}"
        ));
    }
    Ok((address, prefix))
}

/// NetworkManager's ipv4 `dns` is `au` holding each address as an `in_addr_t`
/// — network-byte-order octets reinterpreted as a host-order integer.
fn dns_word(address: &str) -> Result<u32> {
    let octets: Result<Vec<u8>> = address
        .split('.')
        .map(|part| {
            part.parse::<u8>()
                .with_context(|| format!("bad octet {part:?} in DNS address {address:?}"))
        })
        .collect();
    let octets = octets?;
    if octets.len() != 4 {
        return Err(anyhow!("{address:?} is not an IPv4 address"));
    }
    Ok(u32::from_ne_bytes([
        octets[0], octets[1], octets[2], octets[3],
    ]))
}

/// Turn a Lumen connection spec into NetworkManager settings.
///
/// `uuid` is carried through on an update so the profile keeps its identity
/// (and its keyfile) instead of being replaced by a second one that also
/// autoconnects.
pub fn to_settings(spec: &ConnectionSpec, uuid: Option<&str>) -> Result<SettingsMap> {
    let mut builder = SettingsBuilder::new();

    builder
        .set(CONNECTION, "id", spec.id.as_str())
        .set(CONNECTION, "type", nm_type(spec.kind))
        .set(CONNECTION, "interface-name", spec.interface.as_str())
        .set(CONNECTION, "autoconnect", true);
    if let Some(uuid) = uuid {
        builder.set(CONNECTION, "uuid", uuid);
    }

    // controller=/port-type=, not the deprecated master=/slave-type=. See
    // docs/networking.md; the backend refuses to start against a
    // NetworkManager too old to understand them.
    if let (Some(controller), Some(port_type)) = (&spec.controller, spec.port_type) {
        builder
            .set(CONNECTION, "controller", controller.as_str())
            .set(CONNECTION, "port-type", port_type.as_str());
    }

    // MTU lives in the wired setting for every link type — NM has no
    // bridge.mtu / bond.mtu / vlan.mtu.
    if let Some(mtu) = spec.mtu {
        builder.set(WIRED, "mtu", mtu);
    }

    match spec.kind {
        LinkKind::Ethernet | LinkKind::Other => {
            builder.group(WIRED);
            if let Some(ethernet) = &spec.ethernet {
                if let Some(autoneg) = ethernet.autoneg {
                    builder.set(WIRED, "auto-negotiate", autoneg);
                    // NM requires speed and duplex together, and only when
                    // autonegotiation is off; both zero means "driver default".
                    if autoneg {
                        builder.set(WIRED, "speed", 0u32).set(WIRED, "duplex", "");
                    }
                }
                if ethernet.autoneg == Some(false) {
                    if let (Some(speed), Some(duplex)) = (ethernet.speed, ethernet.duplex) {
                        builder
                            .set(WIRED, "speed", speed)
                            .set(WIRED, "duplex", duplex.as_str());
                    }
                }
            }
        }
        LinkKind::Bridge => {
            builder.group(BRIDGE);
            if let Some(bridge) = &spec.bridge {
                builder.set(BRIDGE, "stp", bridge.stp).set(
                    BRIDGE,
                    "vlan-filtering",
                    bridge.vlan_filtering,
                );
                if let Some(delay) = bridge.forward_delay {
                    builder.set(BRIDGE, "forward-delay", delay);
                }
                if let Some(mac) = &bridge.mac_address {
                    builder.set(BRIDGE, "mac-address", mac_bytes(mac)?);
                }
            }
        }
        LinkKind::Bond => {
            builder.group(BOND);
            if let Some(bond) = &spec.bond {
                // bond.options is a{ss}: every value is a string, including
                // the numeric ones.
                let mut options: HashMap<String, String> = HashMap::new();
                options.insert("mode".into(), bond.mode.as_str().into());
                if let Some(miimon) = bond.miimon {
                    options.insert("miimon".into(), miimon.to_string());
                }
                if let Some(rate) = bond.lacp_rate {
                    options.insert("lacp_rate".into(), rate.as_str().into());
                }
                if let Some(policy) = bond.xmit_hash_policy {
                    options.insert("xmit_hash_policy".into(), policy.as_str().into());
                }
                if let Some(primary) = &bond.primary {
                    options.insert("primary".into(), primary.clone());
                }
                builder.set(BOND, "options", options);
            }
        }
        LinkKind::Vlan => {
            let vlan = spec
                .vlan
                .as_ref()
                .ok_or_else(|| anyhow!("{} is a VLAN with no VLAN settings", spec.interface))?;
            builder
                .set(VLAN, "id", u32::from(vlan.id))
                .set(VLAN, "parent", vlan.parent.as_str());
        }
    }

    apply_ip(&mut builder, &spec.ip)?;
    Ok(builder.build())
}

fn apply_ip(builder: &mut SettingsBuilder, ip: &IpConfig) -> Result<()> {
    // IPv6 is off across the appliance in this stage, exactly as the
    // installer's keyfile has it.
    builder.set(IPV6, "method", "disabled");

    match ip {
        IpConfig::Disabled => {
            builder.set(IPV4, "method", "disabled");
        }
        IpConfig::Dhcp => {
            builder.set(IPV4, "method", "auto");
        }
        IpConfig::Static { cidr, gateway, dns } => {
            let (address, prefix) = split_cidr(cidr)?;
            let mut entry: HashMap<String, Value<'_>> = HashMap::new();
            entry.insert("address".into(), Value::from(address.to_string()));
            entry.insert("prefix".into(), Value::from(prefix));
            builder
                .set(IPV4, "method", "manual")
                .set(IPV4, "address-data", vec![entry])
                .set(IPV4, "gateway", gateway.as_str());
            if !dns.is_empty() {
                let words: Result<Vec<u32>> = dns.iter().map(|d| dns_word(d)).collect();
                builder.set(IPV4, "dns", words?);
            }
        }
    }
    Ok(())
}

/// Read a string property out of a settings map.
pub fn get_str<'a>(settings: &'a SettingsMap, group: &str, key: &str) -> Option<&'a str> {
    settings
        .get(group)?
        .get(key)
        .and_then(|value| <&str>::try_from(value).ok())
}

/// Read a u32 property out of a settings map.
pub fn get_u32(settings: &SettingsMap, group: &str, key: &str) -> Option<u32> {
    settings
        .get(group)?
        .get(key)
        .and_then(|value| u32::try_from(value).ok())
}

/// Read one `bond.options` entry.
pub fn get_bond_option(settings: &SettingsMap, key: &str) -> Option<String> {
    let value = settings.get(BOND)?.get("options")?;
    let options: HashMap<String, String> = HashMap::try_from(value.try_clone().ok()?).ok()?;
    options.get(key).cloned()
}

/// The layer-3 configuration a stored profile asks for — the inverse of
/// [`apply_ip`].
///
/// Reading this back is what lets a desired state be seeded from a running box
/// without a DHCP appliance turning into a static one (or the other way
/// round) on the very first apply.
pub fn get_ip(settings: &SettingsMap) -> IpConfig {
    match get_str(settings, IPV4, "method") {
        Some("auto") => IpConfig::Dhcp,
        Some("manual") => {
            let Some(cidr) = first_address(settings) else {
                // "manual" with no address is a profile that cannot come up;
                // reporting it as disabled keeps a seeded document honest.
                return IpConfig::Disabled;
            };
            IpConfig::Static {
                cidr,
                gateway: get_str(settings, IPV4, "gateway")
                    .unwrap_or_default()
                    .to_string(),
                dns: dns_addresses(settings),
            }
        }
        // "disabled", "link-local", "shared", or a profile with no ipv4
        // setting at all: nothing Lumen models as an address.
        _ => IpConfig::Disabled,
    }
}

/// The first `ipv4.address-data` entry as "address/prefix".
fn first_address(settings: &SettingsMap) -> Option<String> {
    let value = settings.get(IPV4)?.get("address-data")?;
    let entries: Vec<HashMap<String, OwnedValue>> = Vec::try_from(value.try_clone().ok()?).ok()?;
    let entry = entries.first()?;
    let address = entry
        .get("address")
        .and_then(|v| <&str>::try_from(v).ok())?;
    let prefix = entry.get("prefix").and_then(|v| u32::try_from(v).ok())?;
    Some(format!("{address}/{prefix}"))
}

/// `ipv4.dns` back from `au` — the mirror of [`dns_word`].
fn dns_addresses(settings: &SettingsMap) -> Vec<String> {
    let Some(value) = settings.get(IPV4).and_then(|group| group.get("dns")) else {
        return Vec::new();
    };
    let Ok(words) = value.try_clone().and_then(Vec::<u32>::try_from) else {
        return Vec::new();
    };
    words
        .into_iter()
        .map(|word| {
            let [a, b, c, d] = word.to_ne_bytes();
            format!("{a}.{b}.{c}.{d}")
        })
        .collect()
}

/// The controller a connection is enslaved to, accepting either spelling on
/// read: a profile written by an older appliance (or by hand) still parses.
pub fn get_controller(settings: &SettingsMap) -> Option<&str> {
    get_str(settings, CONNECTION, "controller")
        .or_else(|| get_str(settings, CONNECTION, "master"))
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BondMode, Duplex, LacpRate};
    use crate::plan::{BondSpec, BridgeSpec, EthernetSpec, PortType, VlanSpec};

    fn spec(interface: &str, kind: LinkKind) -> ConnectionSpec {
        ConnectionSpec {
            id: interface.into(),
            interface: interface.into(),
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

    fn string_of(settings: &SettingsMap, group: &str, key: &str) -> String {
        get_str(settings, group, key)
            .unwrap_or_else(|| panic!("{group}.{key} missing: {settings:?}"))
            .to_string()
    }

    #[test]
    fn a_bridge_carries_its_own_group_and_a_pinned_mac() {
        let mut bridge = spec("br0", LinkKind::Bridge);
        bridge.bridge = Some(BridgeSpec {
            stp: false,
            forward_delay: Some(0),
            vlan_filtering: false,
            mac_address: Some("52:54:00:aa:bb:00".into()),
        });
        bridge.mtu = Some(9000);
        let settings = to_settings(&bridge, None).unwrap();

        assert_eq!(string_of(&settings, CONNECTION, "type"), BRIDGE);
        assert_eq!(string_of(&settings, CONNECTION, "interface-name"), "br0");
        assert_eq!(get_u32(&settings, BRIDGE, "forward-delay"), Some(0));
        assert_eq!(get_u32(&settings, WIRED, "mtu"), Some(9000));
        let mac = settings[BRIDGE]["mac-address"].try_clone().unwrap();
        assert_eq!(
            Vec::<u8>::try_from(mac).unwrap(),
            vec![0x52, 0x54, 0x00, 0xaa, 0xbb, 0x00]
        );
    }

    #[test]
    fn a_port_uses_the_current_property_spelling() {
        let mut port = spec("nic0", LinkKind::Ethernet);
        port.controller = Some("br0".into());
        port.port_type = Some(PortType::Bridge);
        let settings = to_settings(&port, None).unwrap();

        assert_eq!(string_of(&settings, CONNECTION, "controller"), "br0");
        assert_eq!(string_of(&settings, CONNECTION, "port-type"), "bridge");
        assert!(
            !settings[CONNECTION].contains_key("master"),
            "the deprecated spelling must not be written"
        );
        assert!(!settings[CONNECTION].contains_key("slave-type"));
        // …but it is still accepted on read.
        assert_eq!(get_controller(&settings), Some("br0"));
    }

    #[test]
    fn a_static_address_becomes_address_data_and_a_gateway() {
        let mut link = spec("br0", LinkKind::Bridge);
        link.bridge = Some(BridgeSpec::default());
        link.ip = IpConfig::Static {
            cidr: "192.168.10.5/24".into(),
            gateway: "192.168.10.1".into(),
            dns: vec!["9.9.9.9".into()],
        };
        let settings = to_settings(&link, None).unwrap();

        assert_eq!(string_of(&settings, IPV4, "method"), "manual");
        assert_eq!(string_of(&settings, IPV4, "gateway"), "192.168.10.1");
        assert_eq!(string_of(&settings, IPV6, "method"), "disabled");
        let dns = settings[IPV4]["dns"].try_clone().unwrap();
        assert_eq!(
            Vec::<u32>::try_from(dns).unwrap(),
            vec![u32::from_ne_bytes([9, 9, 9, 9])]
        );
    }

    #[test]
    fn dhcp_and_disabled_set_only_a_method() {
        let mut dhcp = spec("nic0", LinkKind::Ethernet);
        dhcp.ip = IpConfig::Dhcp;
        let settings = to_settings(&dhcp, None).unwrap();
        assert_eq!(string_of(&settings, IPV4, "method"), "auto");
        assert!(!settings[IPV4].contains_key("address-data"));

        let off = to_settings(&spec("nic1", LinkKind::Ethernet), None).unwrap();
        assert_eq!(string_of(&off, IPV4, "method"), "disabled");
    }

    #[test]
    fn bond_options_are_strings_including_the_numbers() {
        let mut bond = spec("bond0", LinkKind::Bond);
        bond.bond = Some(BondSpec {
            mode: BondMode::Ieee8023ad,
            miimon: Some(100),
            lacp_rate: Some(LacpRate::Fast),
            xmit_hash_policy: None,
            primary: None,
        });
        let settings = to_settings(&bond, None).unwrap();
        assert_eq!(
            get_bond_option(&settings, "mode").as_deref(),
            Some("802.3ad")
        );
        assert_eq!(get_bond_option(&settings, "miimon").as_deref(), Some("100"));
        assert_eq!(
            get_bond_option(&settings, "lacp_rate").as_deref(),
            Some("fast")
        );
        assert_eq!(get_bond_option(&settings, "xmit_hash_policy"), None);
    }

    #[test]
    fn a_vlan_carries_its_id_and_parent() {
        let mut vlan = spec("vlan100", LinkKind::Vlan);
        vlan.vlan = Some(VlanSpec {
            parent: "bond0".into(),
            id: 100,
        });
        let settings = to_settings(&vlan, None).unwrap();
        assert_eq!(get_u32(&settings, VLAN, "id"), Some(100));
        assert_eq!(string_of(&settings, VLAN, "parent"), "bond0");
    }

    #[test]
    fn forcing_a_link_speed_needs_autoneg_off() {
        let mut nic = spec("nic0", LinkKind::Ethernet);
        nic.ethernet = Some(EthernetSpec {
            autoneg: Some(false),
            speed: Some(1000),
            duplex: Some(Duplex::Full),
        });
        let settings = to_settings(&nic, None).unwrap();
        assert_eq!(get_u32(&settings, WIRED, "speed"), Some(1000));
        assert_eq!(string_of(&settings, WIRED, "duplex"), "full");

        // With autonegotiation on, NM requires both cleared.
        let mut auto = spec("nic0", LinkKind::Ethernet);
        auto.ethernet = Some(EthernetSpec {
            autoneg: Some(true),
            speed: Some(1000),
            duplex: Some(Duplex::Full),
        });
        let settings = to_settings(&auto, None).unwrap();
        assert_eq!(get_u32(&settings, WIRED, "speed"), Some(0));
        assert_eq!(string_of(&settings, WIRED, "duplex"), "");
    }

    #[test]
    fn a_carried_uuid_keeps_the_profile_identity() {
        let settings = to_settings(&spec("nic0", LinkKind::Ethernet), Some("abc-123")).unwrap();
        assert_eq!(string_of(&settings, CONNECTION, "uuid"), "abc-123");
    }

    #[test]
    fn malformed_input_is_an_error_not_a_wrong_setting() {
        let mut bad_mac = spec("br0", LinkKind::Bridge);
        bad_mac.bridge = Some(BridgeSpec {
            mac_address: Some("not-a-mac".into()),
            ..BridgeSpec::default()
        });
        assert!(to_settings(&bad_mac, None).is_err());

        let mut bad_cidr = spec("br0", LinkKind::Bridge);
        bad_cidr.bridge = Some(BridgeSpec::default());
        bad_cidr.ip = IpConfig::Static {
            cidr: "192.168.10.5".into(),
            gateway: "192.168.10.1".into(),
            dns: vec![],
        };
        assert!(to_settings(&bad_cidr, None).is_err());
    }

    /// What is written has to read back as the same thing, or seeding a
    /// desired state from a running box silently rewrites its addressing.
    #[test]
    fn ip_configuration_round_trips_through_a_profile() {
        for ip in [
            IpConfig::Dhcp,
            IpConfig::Disabled,
            IpConfig::Static {
                cidr: "192.168.10.5/24".into(),
                gateway: "192.168.10.1".into(),
                dns: vec!["9.9.9.9".into(), "1.1.1.1".into()],
            },
        ] {
            let mut link = spec("br0", LinkKind::Bridge);
            link.bridge = Some(BridgeSpec::default());
            link.ip = ip.clone();
            let settings = to_settings(&link, None).unwrap();
            assert_eq!(get_ip(&settings), ip);
        }
    }

    #[test]
    fn a_manual_profile_with_no_address_reads_as_disabled() {
        let mut builder = SettingsBuilder::new();
        builder.set(IPV4, "method", "manual");
        assert_eq!(get_ip(&builder.build()), IpConfig::Disabled);
        // …as does a profile with no ipv4 group at all.
        assert_eq!(get_ip(&SettingsMap::new()), IpConfig::Disabled);
    }

    #[test]
    fn cidr_and_mac_parsing() {
        assert_eq!(split_cidr("10.0.0.1/8").unwrap(), ("10.0.0.1", 8));
        assert!(split_cidr("10.0.0.1/33").is_err());
        assert!(mac_bytes("52:54:00:aa:bb").is_err());
        assert_eq!(mac_bytes("00:11:22:33:44:55").unwrap().len(), 6);
    }
}
