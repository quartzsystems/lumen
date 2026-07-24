//! Pure validation over desired + observed state.
//!
//! Returns every problem it finds, each with a machine-readable `code` the
//! console pins a field to and tests assert on, plus a human sentence the
//! console renders verbatim. No I/O, no D-Bus, no ordering assumptions.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::model::{LinkKind, NetworkDesiredState};
use crate::state::ObservedState;

/// Why a desired state was rejected. Stable strings: the console maps them to
/// form fields and the API tests assert on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    /// A link is a port of more than one bridge/bond.
    MultipleControllers,
    /// bridge -> bond -> bridge, or a link enslaved to itself.
    ControllerCycle,
    /// The management address sits on a controller with no ports.
    EmptyManagementController,
    /// The management link is itself a port of a bridge or bond, so it cannot
    /// hold an address at all.
    ManagementIsAPort,
    /// Two VLANs with the same (parent, id).
    DuplicateVlan,
    /// VLAN id outside 1..=4094.
    VlanIdOutOfRange,
    /// A bridge MTU larger than its smallest port's. Linux clamps silently;
    /// we refuse instead.
    BridgeMtuExceedsPort,
    /// A port/parent/management link that nothing defines.
    UnknownReference,
    /// Two links with the same name.
    DuplicateName,
    /// Name is not a legal interface name.
    InvalidName,
    /// The change takes the management address off a link that is currently
    /// reachable, without the caller acknowledging the risk.
    ManagementDisconnect,
    /// A bond `primary` that is not one of its ports.
    PrimaryNotAPort,
}

impl ValidationCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ValidationCode::MultipleControllers => "multiple_controllers",
            ValidationCode::ControllerCycle => "controller_cycle",
            ValidationCode::EmptyManagementController => "empty_management_controller",
            ValidationCode::ManagementIsAPort => "management_is_a_port",
            ValidationCode::DuplicateVlan => "duplicate_vlan",
            ValidationCode::VlanIdOutOfRange => "vlan_id_out_of_range",
            ValidationCode::BridgeMtuExceedsPort => "bridge_mtu_exceeds_port",
            ValidationCode::UnknownReference => "unknown_reference",
            ValidationCode::DuplicateName => "duplicate_name",
            ValidationCode::InvalidName => "invalid_name",
            ValidationCode::ManagementDisconnect => "management_disconnect",
            ValidationCode::PrimaryNotAPort => "primary_not_a_port",
        }
    }
}

/// One problem. `link` and `field` let the console attach the sentence to the
/// offending row and input instead of dumping it in a banner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationError {
    pub code: ValidationCode,
    /// The link the problem is about, when it is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// The model field the problem is about ("ports", "vlan_id", "mtu", …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// One sentence, shown to the operator as written.
    pub message: String,
}

impl ValidationError {
    fn new(code: ValidationCode, message: impl Into<String>) -> Self {
        Self {
            code,
            link: None,
            field: None,
            message: message.into(),
        }
    }

    fn at(mut self, link: impl Into<String>, field: &str) -> Self {
        self.link = Some(link.into());
        self.field = Some(field.to_string());
        self
    }
}

/// What the caller is willing to risk. An apply that would strand the
/// management address needs an explicit acknowledgement.
#[derive(Debug, Clone, Copy, Default)]
pub struct Acknowledgements {
    /// `i_understand_this_may_disconnect_me` from the request body.
    pub may_disconnect: bool,
}

/// Legal Linux interface name: 1–15 characters, no slash, no whitespace, no
/// colon (which `ip` uses as an alias separator), not "." or "..".
fn valid_ifname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c == '/' || c == ':' || c.is_whitespace() || c.is_control())
}

/// Validate `desired` against what is currently on the box.
///
/// Order of the returned errors is stable (by check, then by the order links
/// appear in the desired state) so tests and the console are deterministic.
pub fn validate(
    desired: &NetworkDesiredState,
    observed: &ObservedState,
    ack: Acknowledgements,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let kinds: Vec<(&str, LinkKind)> = desired.link_kinds();

    check_names(&kinds, &mut errors);
    check_references(desired, &kinds, &mut errors);
    check_vlans(desired, &mut errors);
    check_controllers(desired, &mut errors);
    check_mtus(desired, observed, &mut errors);
    check_management(desired, observed, ack, &mut errors);

    errors
}

fn check_names(kinds: &[(&str, LinkKind)], errors: &mut Vec<ValidationError>) {
    let mut seen: HashSet<&str> = HashSet::new();
    for (name, _) in kinds {
        if !valid_ifname(name) {
            errors.push(
                ValidationError::new(
                    ValidationCode::InvalidName,
                    format!(
                        "\"{name}\" is not a usable interface name — up to 15 characters, \
                         no spaces, slashes, or colons."
                    ),
                )
                .at(*name, "name"),
            );
        }
        if !seen.insert(name) {
            errors.push(
                ValidationError::new(
                    ValidationCode::DuplicateName,
                    format!("More than one link is named \"{name}\"."),
                )
                .at(*name, "name"),
            );
        }
    }
}

fn check_references(
    desired: &NetworkDesiredState,
    kinds: &[(&str, LinkKind)],
    errors: &mut Vec<ValidationError>,
) {
    let known: HashSet<&str> = kinds.iter().map(|(n, _)| *n).collect();

    // Returns the error rather than pushing it, so the caller keeps exclusive
    // access to `errors` for the checks that need it too.
    let unknown = |owner: &str, field: &'static str, target: &str, what: &str| {
        (!known.contains(target)).then(|| {
            ValidationError::new(
                ValidationCode::UnknownReference,
                format!("\"{owner}\" names {what} \"{target}\", which does not exist."),
            )
            .at(owner, field)
        })
    };

    for bridge in &desired.bridges {
        for port in &bridge.ports {
            errors.extend(unknown(&bridge.name, "ports", port, "the port"));
        }
    }
    for bond in &desired.bonds {
        for port in &bond.ports {
            errors.extend(unknown(&bond.name, "ports", port, "the port"));
        }
        if let Some(primary) = &bond.primary {
            if !bond.ports.iter().any(|p| p == primary) {
                errors.push(
                    ValidationError::new(
                        ValidationCode::PrimaryNotAPort,
                        format!(
                            "\"{}\" sets \"{primary}\" as its primary port, but that link is \
                             not one of its ports.",
                            bond.name
                        ),
                    )
                    .at(&bond.name, "primary"),
                );
            }
        }
    }
    for vlan in &desired.vlans {
        errors.extend(unknown(
            &vlan.name,
            "parent",
            &vlan.parent,
            "the parent link",
        ));
    }
    if !desired.management.link.is_empty() {
        errors.extend(unknown(
            &desired.management.link,
            "link",
            &desired.management.link,
            "the management link",
        ));
    }
}

fn check_vlans(desired: &NetworkDesiredState, errors: &mut Vec<ValidationError>) {
    let mut seen: HashSet<(&str, u16)> = HashSet::new();
    for vlan in &desired.vlans {
        if !(1..=4094).contains(&vlan.vlan_id) {
            errors.push(
                ValidationError::new(
                    ValidationCode::VlanIdOutOfRange,
                    format!(
                        "VLAN id {} is out of range for \"{}\" — use 1 to 4094.",
                        vlan.vlan_id, vlan.name
                    ),
                )
                .at(&vlan.name, "vlan_id"),
            );
        }
        if !seen.insert((vlan.parent.as_str(), vlan.vlan_id)) {
            errors.push(
                ValidationError::new(
                    ValidationCode::DuplicateVlan,
                    format!(
                        "VLAN {} on \"{}\" is already defined by another interface.",
                        vlan.vlan_id, vlan.parent
                    ),
                )
                .at(&vlan.name, "vlan_id"),
            );
        }
    }
}

fn check_controllers(desired: &NetworkDesiredState, errors: &mut Vec<ValidationError>) {
    // A link may be enslaved to at most one controller.
    let mut controllers: HashMap<&str, Vec<&str>> = HashMap::new();
    for bridge in &desired.bridges {
        for port in &bridge.ports {
            controllers
                .entry(port.as_str())
                .or_default()
                .push(&bridge.name);
        }
    }
    for bond in &desired.bonds {
        for port in &bond.ports {
            controllers
                .entry(port.as_str())
                .or_default()
                .push(&bond.name);
        }
    }
    // Sorted so the message (and the test) is deterministic regardless of
    // hash order.
    let mut multiple: Vec<(&str, Vec<&str>)> = controllers
        .iter()
        .filter(|(_, owners)| owners.len() > 1)
        .map(|(port, owners)| {
            let mut owners = owners.clone();
            owners.sort_unstable();
            (*port, owners)
        })
        .collect();
    multiple.sort_unstable_by_key(|(port, _)| *port);
    for (port, owners) in multiple {
        errors.push(
            ValidationError::new(
                ValidationCode::MultipleControllers,
                format!(
                    "\"{port}\" is a port of both {} — a link can belong to only one bridge \
                     or bond.",
                    owners.join(" and ")
                ),
            )
            .at(port, "ports"),
        );
    }

    // Cycles in the controller graph, including a link enslaved to itself.
    // Walk port -> controller from every link; the graph is tiny.
    let single: HashMap<&str, &str> = controllers
        .iter()
        .filter_map(|(port, owners)| owners.first().map(|owner| (*port, *owner)))
        .collect();
    let mut reported: HashSet<&str> = HashSet::new();
    let mut starts: Vec<&str> = single.keys().copied().collect();
    starts.sort_unstable();
    for start in starts {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut cursor = start;
        seen.insert(cursor);
        while let Some(next) = single.get(cursor) {
            if !seen.insert(next) {
                // Report the cycle once, against its alphabetically first
                // member, so two entry points don't produce two errors.
                let mut members: Vec<&str> = seen.into_iter().collect();
                members.sort_unstable();
                let anchor = members[0];
                if reported.insert(anchor) {
                    errors.push(
                        ValidationError::new(
                            ValidationCode::ControllerCycle,
                            format!(
                                "{} form a loop of bridges and bonds — a link cannot end up \
                                 enslaved to itself.",
                                members.join(", ")
                            ),
                        )
                        .at(anchor, "ports"),
                    );
                }
                break;
            }
            cursor = next;
        }
    }
}

fn check_mtus(
    desired: &NetworkDesiredState,
    observed: &ObservedState,
    errors: &mut Vec<ValidationError>,
) {
    // A Linux bridge clamps its MTU to the smallest port MTU. Refusing beats
    // accepting a number the kernel will quietly ignore.
    for bridge in &desired.bridges {
        let Some(bridge_mtu) = bridge.mtu else {
            continue;
        };
        for port in &bridge.ports {
            let port_mtu = desired
                .nic(port)
                .and_then(|n| n.mtu)
                .or_else(|| desired.bond(port).and_then(|b| b.mtu))
                .or_else(|| desired.vlan(port).and_then(|v| v.mtu))
                .or_else(|| observed.link(port).and_then(|l| l.mtu));
            let Some(port_mtu) = port_mtu else { continue };
            if bridge_mtu > port_mtu {
                errors.push(
                    ValidationError::new(
                        ValidationCode::BridgeMtuExceedsPort,
                        format!(
                            "\"{}\" is set to MTU {bridge_mtu}, above its port \"{port}\" at \
                             {port_mtu} — a bridge cannot exceed its smallest port.",
                            bridge.name
                        ),
                    )
                    .at(&bridge.name, "mtu"),
                );
            }
        }
    }
}

fn check_management(
    desired: &NetworkDesiredState,
    observed: &ObservedState,
    ack: Acknowledgements,
    errors: &mut Vec<ValidationError>,
) {
    let link = desired.management.link.as_str();
    if link.is_empty() {
        errors.push(ValidationError::new(
            ValidationCode::UnknownReference,
            "No link is set to carry the management address.",
        ));
        return;
    }

    // A port hands its addressing to its controller — an address configured
    // on it is simply dropped. This is the shape an operator lands on when
    // hand-building a management bridge and forgetting to move the address:
    // the apply would take the box off the network with nothing to fall back
    // to. It is also the one case NetworkManager will accept without
    // complaint, which is exactly why it is checked here.
    if let Some(controller) = desired.controller_of(link) {
        errors.push(
            ValidationError::new(
                ValidationCode::ManagementIsAPort,
                format!(
                    "\"{link}\" carries the management address but is a port of \
                     \"{controller}\" — move the address to \"{controller}\" instead."
                ),
            )
            .at(link, "management"),
        );
    }

    // A controller with no ports has no path to the wire, so the management
    // address on it is unreachable the moment it is applied.
    match desired.kind_of(link) {
        Some(LinkKind::Bridge) | Some(LinkKind::Bond) => {
            if desired.ports_of(link).is_empty() {
                errors.push(
                    ValidationError::new(
                        ValidationCode::EmptyManagementController,
                        format!(
                            "\"{link}\" carries the management address but has no ports — it \
                             would have no path to the network."
                        ),
                    )
                    .at(link, "ports"),
                );
            }
        }
        _ => {}
    }

    // Moving the address off a link that is currently reachable is exactly the
    // truck-roll case: allowed, but only when the caller says so.
    if ack.may_disconnect {
        return;
    }
    let currently: Vec<&str> = observed
        .addressed()
        .filter(|l| l.state.is_up())
        .map(|l| l.name.as_str())
        .collect();
    if currently.is_empty() || currently.contains(&link) {
        return;
    }
    errors.push(
        ValidationError::new(
            ValidationCode::ManagementDisconnect,
            format!(
                "This moves the management address from \"{}\" to \"{link}\", which can drop \
                 your connection. Confirm that you understand the risk.",
                currently.join("\", \"")
            ),
        )
        .at(link, "management"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Bond, Bridge, IpConfig, ManagementRef, Nic, Vlan};
    use crate::state::{LinkState, ObservedLink};

    fn codes(errors: &[ValidationError]) -> Vec<ValidationCode> {
        errors.iter().map(|e| e.code).collect()
    }

    /// nic0 and nic1 present, nic0 up with the management address.
    fn observed() -> ObservedState {
        ObservedState {
            node: "lumen".into(),
            links: vec![
                ObservedLink {
                    name: "nic0".into(),
                    kind: LinkKind::Ethernet,
                    state: LinkState::Activated,
                    carrier: true,
                    mtu: Some(1500),
                    addresses: vec!["192.168.10.5/24".into()],
                    gateway: Some("192.168.10.1".into()),
                    ..ObservedLink::default()
                },
                ObservedLink {
                    name: "nic1".into(),
                    kind: LinkKind::Ethernet,
                    state: LinkState::Disconnected,
                    carrier: true,
                    mtu: Some(1500),
                    ..ObservedLink::default()
                },
            ],
        }
    }

    fn nic(name: &str) -> Nic {
        Nic {
            name: name.into(),
            ..Nic::default()
        }
    }

    /// The healthy baseline every case below perturbs: two NICs, management
    /// on nic0 exactly where the box already has it.
    fn baseline() -> NetworkDesiredState {
        NetworkDesiredState {
            nics: vec![nic("nic0"), nic("nic1")],
            management: ManagementRef {
                link: "nic0".into(),
                ip: IpConfig::Dhcp,
            },
            ..NetworkDesiredState::default()
        }
    }

    #[test]
    fn baseline_is_clean() {
        assert_eq!(
            validate(&baseline(), &observed(), Acknowledgements::default()),
            vec![]
        );
    }

    #[test]
    fn table_driven_rejections() {
        // (name, mutation, expected code)
        /// A named perturbation of the healthy baseline and the code it must
        /// produce.
        type Case = (&'static str, fn(&mut NetworkDesiredState), ValidationCode);

        let cases: Vec<Case> = vec![
            (
                "port of two controllers",
                |d| {
                    d.bridges.push(Bridge {
                        name: "br0".into(),
                        ports: vec!["nic1".into()],
                        ..Bridge::default()
                    });
                    d.bonds.push(Bond {
                        name: "bond0".into(),
                        ports: vec!["nic1".into()],
                        ..Bond::default()
                    });
                },
                ValidationCode::MultipleControllers,
            ),
            (
                "bridge enslaved to a bond enslaved back to the bridge",
                |d| {
                    d.bridges.push(Bridge {
                        name: "br0".into(),
                        ports: vec!["bond0".into()],
                        ..Bridge::default()
                    });
                    d.bonds.push(Bond {
                        name: "bond0".into(),
                        ports: vec!["br0".into()],
                        ..Bond::default()
                    });
                },
                ValidationCode::ControllerCycle,
            ),
            (
                "management bridge with no ports",
                |d| {
                    d.bridges.push(Bridge {
                        name: "br0".into(),
                        ..Bridge::default()
                    });
                    d.management.link = "br0".into();
                },
                ValidationCode::EmptyManagementController,
            ),
            (
                "management bond with no ports",
                |d| {
                    d.bonds.push(Bond {
                        name: "bond0".into(),
                        ..Bond::default()
                    });
                    d.management.link = "bond0".into();
                },
                ValidationCode::EmptyManagementController,
            ),
            (
                "management link enslaved to a bridge",
                |d| {
                    d.bridges.push(Bridge {
                        name: "br0".into(),
                        ports: vec!["nic0".into()],
                        ..Bridge::default()
                    });
                },
                ValidationCode::ManagementIsAPort,
            ),
            (
                "two VLANs with the same parent and id",
                |d| {
                    d.vlans.push(Vlan {
                        name: "vlan100".into(),
                        parent: "nic0".into(),
                        vlan_id: 100,
                        mtu: None,
                    });
                    d.vlans.push(Vlan {
                        name: "vlan100b".into(),
                        parent: "nic0".into(),
                        vlan_id: 100,
                        mtu: None,
                    });
                },
                ValidationCode::DuplicateVlan,
            ),
            (
                "VLAN id above 4094",
                |d| {
                    d.vlans.push(Vlan {
                        name: "vlan4095".into(),
                        parent: "nic0".into(),
                        vlan_id: 4095,
                        mtu: None,
                    });
                },
                ValidationCode::VlanIdOutOfRange,
            ),
            (
                "VLAN id zero",
                |d| {
                    d.vlans.push(Vlan {
                        name: "vlan0".into(),
                        parent: "nic0".into(),
                        vlan_id: 0,
                        mtu: None,
                    });
                },
                ValidationCode::VlanIdOutOfRange,
            ),
            (
                "bridge MTU above its port's",
                |d| {
                    d.nics[1].mtu = Some(1500);
                    d.bridges.push(Bridge {
                        name: "br0".into(),
                        ports: vec!["nic1".into()],
                        mtu: Some(9000),
                        ..Bridge::default()
                    });
                },
                ValidationCode::BridgeMtuExceedsPort,
            ),
            (
                "port that does not exist",
                |d| {
                    d.bridges.push(Bridge {
                        name: "br0".into(),
                        ports: vec!["nic9".into()],
                        ..Bridge::default()
                    });
                },
                ValidationCode::UnknownReference,
            ),
            (
                "VLAN parent that does not exist",
                |d| {
                    d.vlans.push(Vlan {
                        name: "vlan100".into(),
                        parent: "bond9".into(),
                        vlan_id: 100,
                        mtu: None,
                    });
                },
                ValidationCode::UnknownReference,
            ),
            (
                "primary that is not a port",
                |d| {
                    d.bonds.push(Bond {
                        name: "bond0".into(),
                        ports: vec!["nic1".into()],
                        primary: Some("nic0".into()),
                        ..Bond::default()
                    });
                },
                ValidationCode::PrimaryNotAPort,
            ),
            (
                "duplicate link name",
                |d| d.nics.push(nic("nic0")),
                ValidationCode::DuplicateName,
            ),
            (
                "interface name too long",
                |d| {
                    d.bridges.push(Bridge {
                        name: "bridge-with-a-very-long-name".into(),
                        ports: vec!["nic1".into()],
                        ..Bridge::default()
                    });
                },
                ValidationCode::InvalidName,
            ),
        ];

        for (name, mutate, expected) in cases {
            let mut desired = baseline();
            mutate(&mut desired);
            let errors = validate(&desired, &observed(), Acknowledgements::default());
            assert!(
                codes(&errors).contains(&expected),
                "{name}: expected {expected:?}, got {:?}",
                codes(&errors)
            );
        }
    }

    #[test]
    fn bridge_mtu_equal_to_port_is_fine() {
        let mut desired = baseline();
        desired.nics[1].mtu = Some(9000);
        desired.bridges.push(Bridge {
            name: "br0".into(),
            ports: vec!["nic1".into()],
            mtu: Some(9000),
            ..Bridge::default()
        });
        assert_eq!(
            validate(&desired, &observed(), Acknowledgements::default()),
            vec![]
        );
    }

    #[test]
    fn bridge_mtu_is_checked_against_the_observed_port_when_unset() {
        // The port's MTU isn't in the request, so the box's own value decides.
        let mut desired = baseline();
        desired.bridges.push(Bridge {
            name: "br0".into(),
            ports: vec!["nic1".into()],
            mtu: Some(9000),
            ..Bridge::default()
        });
        let errors = validate(&desired, &observed(), Acknowledgements::default());
        assert_eq!(codes(&errors), vec![ValidationCode::BridgeMtuExceedsPort]);
    }

    #[test]
    fn moving_the_management_address_needs_an_acknowledgement() {
        let mut desired = baseline();
        desired.bridges.push(Bridge {
            name: "br0".into(),
            ports: vec!["nic0".into()],
            ..Bridge::default()
        });
        desired.management.link = "br0".into();

        let errors = validate(&desired, &observed(), Acknowledgements::default());
        assert_eq!(codes(&errors), vec![ValidationCode::ManagementDisconnect]);

        let acked = validate(
            &desired,
            &observed(),
            Acknowledgements {
                may_disconnect: true,
            },
        );
        assert_eq!(acked, vec![]);
    }

    #[test]
    fn leaving_the_management_address_where_it_is_needs_no_acknowledgement() {
        let mut desired = baseline();
        desired.bridges.push(Bridge {
            name: "br0".into(),
            ports: vec!["nic1".into()],
            ..Bridge::default()
        });
        assert_eq!(
            validate(&desired, &observed(), Acknowledgements::default()),
            vec![]
        );
    }

    #[test]
    fn a_cycle_is_reported_once_not_once_per_member() {
        let mut desired = baseline();
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
        let errors = validate(&desired, &observed(), Acknowledgements::default());
        assert_eq!(
            errors
                .iter()
                .filter(|e| e.code == ValidationCode::ControllerCycle)
                .count(),
            1
        );
    }

    #[test]
    fn errors_carry_the_link_and_field_the_console_pins_them_to() {
        let mut desired = baseline();
        desired.vlans.push(Vlan {
            name: "vlan9999".into(),
            parent: "nic0".into(),
            vlan_id: 9999,
            mtu: None,
        });
        let errors = validate(&desired, &observed(), Acknowledgements::default());
        let err = &errors[0];
        assert_eq!(err.code, ValidationCode::VlanIdOutOfRange);
        assert_eq!(err.link.as_deref(), Some("vlan9999"));
        assert_eq!(err.field.as_deref(), Some("vlan_id"));
        assert!(err.message.ends_with('.'), "message must be a sentence");
    }

    #[test]
    fn every_problem_is_reported_not_just_the_first() {
        let mut desired = baseline();
        desired.vlans.push(Vlan {
            name: "vlan0".into(),
            parent: "nic9".into(),
            vlan_id: 0,
            mtu: None,
        });
        let errors = validate(&desired, &observed(), Acknowledgements::default());
        assert!(codes(&errors).contains(&ValidationCode::UnknownReference));
        assert!(codes(&errors).contains(&ValidationCode::VlanIdOutOfRange));
    }

    #[test]
    fn empty_management_link_is_rejected() {
        let mut desired = baseline();
        desired.management.link = String::new();
        let errors = validate(&desired, &observed(), Acknowledgements::default());
        assert_eq!(codes(&errors), vec![ValidationCode::UnknownReference]);
    }

    #[test]
    fn ifname_rules() {
        assert!(valid_ifname("nic0"));
        assert!(valid_ifname("br0"));
        assert!(valid_ifname("bond0.100"));
        assert!(!valid_ifname(""));
        assert!(!valid_ifname("."));
        assert!(!valid_ifname(".."));
        assert!(!valid_ifname("has space"));
        assert!(!valid_ifname("has/slash"));
        assert!(!valid_ifname("has:colon"));
        assert!(!valid_ifname("0123456789abcdef")); // 16 characters
    }
}
