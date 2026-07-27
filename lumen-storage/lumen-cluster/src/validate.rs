//! Pure rules over a cluster definition and the environment it would be built
//! in. No async, no backend: a refused request must be refusable without
//! touching a node, because the wizard runs these same checks on every
//! keystroke through the console's `allowed`/`reason` plumbing.
//!
//! The codes are part of the API — the console matches on them to pin a
//! message to the field that caused it — so renaming one is a breaking change.

use serde::Serialize;

use crate::environment::EnvironmentMembership;
use crate::model::{
    valid_cluster_name, valid_node_name, ClusterDefinition, MAX_CLUSTER_NODES, MIN_CLUSTER_NODES,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    InvalidClusterName,
    DuplicateClusterName,
    InvalidNodeName,
    TooFewNodes,
    TooManyNodes,
    DuplicateNode,
    NodeNotInEnvironment,
    NodeAlreadyClustered,
    PreferredNodeNotAMember,
    PreferredNodeNeedsTwoNodes,
    DuplicateRingAddress,
    MissingBmc,
    // Networks. Produced by `networks::validate_networks`.
    InvalidSubnet,
    OverlappingSubnets,
    NetworkMemberMissing,
    NetworkMemberNotInCluster,
    SharedCoreManagementInterface,
    UnknownInterface,
    InterfaceDown,
    AddressOutsideSubnet,
    DuplicateAddress,
    InvalidMtu,
    InvalidVip,
    InvalidBridgeName,
    InvalidVlan,
    // Volumes. Produced by `validate_volume_members`; the invariant the
    // topology property tests hold for every N.
    VolumeMemberOutsideCluster,
    TooFewVolumeMembers,
}

impl ValidationCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ValidationCode::InvalidClusterName => "invalid_cluster_name",
            ValidationCode::DuplicateClusterName => "duplicate_cluster_name",
            ValidationCode::InvalidNodeName => "invalid_node_name",
            ValidationCode::TooFewNodes => "too_few_nodes",
            ValidationCode::TooManyNodes => "too_many_nodes",
            ValidationCode::DuplicateNode => "duplicate_node",
            ValidationCode::NodeNotInEnvironment => "node_not_in_environment",
            ValidationCode::NodeAlreadyClustered => "node_already_clustered",
            ValidationCode::PreferredNodeNotAMember => "preferred_node_not_a_member",
            ValidationCode::PreferredNodeNeedsTwoNodes => "preferred_node_needs_two_nodes",
            ValidationCode::DuplicateRingAddress => "duplicate_ring_address",
            ValidationCode::MissingBmc => "missing_bmc",
            ValidationCode::InvalidSubnet => "invalid_subnet",
            ValidationCode::OverlappingSubnets => "overlapping_subnets",
            ValidationCode::NetworkMemberMissing => "network_member_missing",
            ValidationCode::NetworkMemberNotInCluster => "network_member_not_in_cluster",
            ValidationCode::SharedCoreManagementInterface => "shared_core_management_interface",
            ValidationCode::UnknownInterface => "unknown_interface",
            ValidationCode::InterfaceDown => "interface_down",
            ValidationCode::AddressOutsideSubnet => "address_outside_subnet",
            ValidationCode::DuplicateAddress => "duplicate_address",
            ValidationCode::InvalidMtu => "invalid_mtu",
            ValidationCode::InvalidVip => "invalid_vip",
            ValidationCode::InvalidBridgeName => "invalid_bridge_name",
            ValidationCode::InvalidVlan => "invalid_vlan",
            ValidationCode::VolumeMemberOutsideCluster => "volume_member_outside_cluster",
            ValidationCode::TooFewVolumeMembers => "too_few_volume_members",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationError {
    pub code: ValidationCode,
    /// The cluster the problem is about, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    /// The input field the console should pin the message to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
}

impl ValidationError {
    pub fn new(code: ValidationCode, field: Option<&str>, message: impl Into<String>) -> Self {
        ValidationError {
            code,
            cluster: None,
            field: field.map(str::to_string),
            message: message.into(),
        }
    }

    /// Stamp the cluster the problem is about.
    pub fn about(mut self, cluster: &str) -> Self {
        self.cluster = Some(cluster.to_string());
        self
    }
}

/// Whether a cluster definition is buildable in this environment. Checks the
/// definition alone plus the one thing that needs company: every member must
/// be an environment node that is not already in a cluster.
pub fn validate_definition(
    definition: &ClusterDefinition,
    environment: &EnvironmentMembership,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    if !valid_cluster_name(&definition.name) {
        errors.push(
            ValidationError::new(
                ValidationCode::InvalidClusterName,
                Some("name"),
                format!(
                    "\"{}\" is not a usable cluster name — lowercase letters, digits, and \
                     hyphens, starting with a letter, at most 32 characters.",
                    definition.name
                ),
            )
            .about(&definition.name),
        );
    }

    if environment
        .nodes
        .iter()
        .any(|n| n.cluster.as_deref() == Some(definition.name.as_str()))
    {
        errors.push(
            ValidationError::new(
                ValidationCode::DuplicateClusterName,
                Some("name"),
                format!(
                    "This environment already has a cluster called \"{}\".",
                    definition.name
                ),
            )
            .about(&definition.name),
        );
    }

    if definition.nodes.len() < MIN_CLUSTER_NODES {
        errors.push(
            ValidationError::new(
                ValidationCode::TooFewNodes,
                Some("nodes"),
                format!(
                    "A cluster needs at least {MIN_CLUSTER_NODES} nodes; {} chosen.",
                    definition.nodes.len()
                ),
            )
            .about(&definition.name),
        );
    }
    if definition.nodes.len() > MAX_CLUSTER_NODES {
        errors.push(
            ValidationError::new(
                ValidationCode::TooManyNodes,
                Some("nodes"),
                format!(
                    "A cluster holds at most {MAX_CLUSTER_NODES} nodes; {} chosen.",
                    definition.nodes.len()
                ),
            )
            .about(&definition.name),
        );
    }

    for (index, node) in definition.nodes.iter().enumerate() {
        if !valid_node_name(&node.name) {
            errors.push(
                ValidationError::new(
                    ValidationCode::InvalidNodeName,
                    Some("nodes"),
                    format!("\"{}\" is not a usable node name.", node.name),
                )
                .about(&definition.name),
            );
            continue;
        }
        if definition.nodes[..index]
            .iter()
            .any(|n| n.name == node.name)
        {
            errors.push(
                ValidationError::new(
                    ValidationCode::DuplicateNode,
                    Some("nodes"),
                    format!("\"{}\" is listed more than once.", node.name),
                )
                .about(&definition.name),
            );
            continue;
        }
        match environment.node(&node.name) {
            None => errors.push(
                ValidationError::new(
                    ValidationCode::NodeNotInEnvironment,
                    Some("nodes"),
                    format!(
                        "\"{}\" has not joined this environment, so it cannot join a cluster.",
                        node.name
                    ),
                )
                .about(&definition.name),
            ),
            Some(member) => {
                if let Some(cluster) = &member.cluster {
                    errors.push(
                        ValidationError::new(
                            ValidationCode::NodeAlreadyClustered,
                            Some("nodes"),
                            format!(
                                "\"{}\" is already a member of \"{cluster}\" — a node belongs \
                                 to at most one cluster.",
                                node.name
                            ),
                        )
                        .about(&definition.name),
                    );
                }
            }
        }
        if node.bmc.address.trim().is_empty() || node.bmc.username.trim().is_empty() {
            errors.push(
                ValidationError::new(
                    ValidationCode::MissingBmc,
                    Some("fencing"),
                    format!(
                        "\"{}\" has no BMC configured. Fencing is not optional, so every member \
                         needs one.",
                        node.name
                    ),
                )
                .about(&definition.name),
            );
        }
    }

    // Ring addresses must be unique per ring across the members: two nodes
    // answering on one address is a cluster that cannot tell them apart.
    for ring in 0..2u8 {
        let mut seen = Vec::new();
        for node in &definition.nodes {
            let addr = if ring == 0 { node.ring0 } else { node.ring1 };
            if seen.contains(&addr) {
                errors.push(
                    ValidationError::new(
                        ValidationCode::DuplicateRingAddress,
                        Some("networks"),
                        format!("{addr} is assigned to more than one node on ring {ring}."),
                    )
                    .about(&definition.name),
                );
            }
            seen.push(addr);
        }
    }

    match &definition.preferred_node {
        Some(preferred) if definition.regime() == crate::model::Regime::Quorum => {
            errors.push(
                ValidationError::new(
                    ValidationCode::PreferredNodeNeedsTwoNodes,
                    Some("preferred_node"),
                    format!(
                        "A preferred node only means something at two nodes, where it decides \
                         the fence race; at {} nodes majority quorum decides, so \"{preferred}\" \
                         would be a setting that does nothing.",
                        definition.nodes.len()
                    ),
                )
                .about(&definition.name),
            );
        }
        Some(preferred) if definition.node(preferred).is_none() => {
            errors.push(
                ValidationError::new(
                    ValidationCode::PreferredNodeNotAMember,
                    Some("preferred_node"),
                    format!("\"{preferred}\" is not one of the cluster's members."),
                )
                .about(&definition.name),
            );
        }
        _ => {}
    }

    errors
}

/// The placement rule the topology property tests hold for every N: a
/// replicated volume's members are cluster members, and there are at least
/// two of them. `lumen-drbd` calls this before anything touches a zvol.
pub fn validate_volume_members(
    members: &[String],
    cluster_nodes: &[String],
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    if members.len() < 2 {
        errors.push(ValidationError::new(
            ValidationCode::TooFewVolumeMembers,
            Some("members"),
            "A replicated volume needs at least two members.",
        ));
    }
    for member in members {
        if !cluster_nodes.contains(member) {
            errors.push(ValidationError::new(
                ValidationCode::VolumeMemberOutsideCluster,
                Some("members"),
                format!(
                    "\"{member}\" is not a member of this cluster — replicas live only where \
                     the cluster's machines can run."
                ),
            ));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::{EnvironmentMembership, EnvironmentNode};
    use crate::model::{BmcConfig, MemberNode};
    use std::net::Ipv4Addr;

    fn member(name: &str, last_octet: u8) -> MemberNode {
        MemberNode {
            name: name.into(),
            ring0: Ipv4Addr::new(10, 10, 0, last_octet),
            ring1: Ipv4Addr::new(192, 168, 10, last_octet),
            bmc: BmcConfig {
                address: format!("10.20.0.{last_octet}"),
                username: "ADMIN".into(),
            },
        }
    }

    fn environment_of(names: &[&str]) -> EnvironmentMembership {
        EnvironmentMembership {
            id: "env-test".into(),
            version: 1,
            nodes: names
                .iter()
                .map(|name| EnvironmentNode {
                    name: (*name).into(),
                    address: "192.168.10.1".into(),
                    controlplane_version: "0.3.0".into(),
                    cluster: None,
                })
                .collect(),
        }
    }

    fn definition(nodes: Vec<MemberNode>, preferred: Option<&str>) -> ClusterDefinition {
        ClusterDefinition {
            name: "alpha".into(),
            nodes,
            preferred_node: preferred.map(str::to_string),
        }
    }

    fn codes(errors: &[ValidationError]) -> Vec<ValidationCode> {
        errors.iter().map(|e| e.code).collect()
    }

    #[test]
    fn a_well_formed_two_node_definition_passes() {
        let env = environment_of(&["alpha-1", "alpha-2"]);
        let def = definition(
            vec![member("alpha-1", 1), member("alpha-2", 2)],
            Some("alpha-1"),
        );
        assert_eq!(
            validate_definition(&def, &env),
            vec![],
            "expected no errors"
        );
    }

    #[test]
    fn every_problem_is_reported_not_just_the_first() {
        let env = environment_of(&["alpha-1"]);
        let mut lonely = member("alpha-1", 1);
        lonely.bmc.address.clear();
        let def = ClusterDefinition {
            name: "Bad Name".into(),
            nodes: vec![lonely],
            preferred_node: Some("ghost".into()),
        };
        let errors = validate_definition(&def, &env);
        let found = codes(&errors);
        assert!(
            found.contains(&ValidationCode::InvalidClusterName),
            "{errors:#?}"
        );
        assert!(found.contains(&ValidationCode::TooFewNodes), "{errors:#?}");
        assert!(found.contains(&ValidationCode::MissingBmc), "{errors:#?}");
        assert!(
            found.contains(&ValidationCode::PreferredNodeNotAMember),
            "{errors:#?}"
        );
    }

    #[test]
    fn a_node_outside_the_environment_is_refused() {
        let env = environment_of(&["alpha-1"]);
        let def = definition(vec![member("alpha-1", 1), member("stranger", 2)], None);
        assert!(
            codes(&validate_definition(&def, &env)).contains(&ValidationCode::NodeNotInEnvironment)
        );
    }

    #[test]
    fn a_node_already_in_a_cluster_cannot_join_another() {
        let mut env = environment_of(&["alpha-1", "alpha-2"]);
        env.nodes[1].cluster = Some("beta".into());
        let def = definition(vec![member("alpha-1", 1), member("alpha-2", 2)], None);
        assert!(
            codes(&validate_definition(&def, &env)).contains(&ValidationCode::NodeAlreadyClustered)
        );
    }

    #[test]
    fn six_nodes_are_too_many_and_one_is_too_few() {
        let names = ["a-1", "a-2", "a-3", "a-4", "a-5", "a-6"];
        let env = environment_of(&names);
        let six: Vec<MemberNode> = names
            .iter()
            .enumerate()
            .map(|(i, n)| member(n, i as u8 + 1))
            .collect();
        let def = definition(six, None);
        assert!(codes(&validate_definition(&def, &env)).contains(&ValidationCode::TooManyNodes));

        let def = definition(vec![member("a-1", 1)], None);
        assert!(codes(&validate_definition(&def, &env)).contains(&ValidationCode::TooFewNodes));
    }

    #[test]
    fn a_preferred_node_at_three_nodes_is_a_setting_that_does_nothing() {
        let env = environment_of(&["a-1", "a-2", "a-3"]);
        let def = definition(
            vec![member("a-1", 1), member("a-2", 2), member("a-3", 3)],
            Some("a-1"),
        );
        assert!(codes(&validate_definition(&def, &env))
            .contains(&ValidationCode::PreferredNodeNeedsTwoNodes));
    }

    #[test]
    fn two_nodes_on_one_ring_address_cannot_be_told_apart() {
        let env = environment_of(&["a-1", "a-2"]);
        let mut twin = member("a-2", 2);
        twin.ring0 = Ipv4Addr::new(10, 10, 0, 1);
        let def = definition(vec![member("a-1", 1), twin], None);
        assert!(
            codes(&validate_definition(&def, &env)).contains(&ValidationCode::DuplicateRingAddress)
        );
    }

    #[test]
    fn volume_members_are_cluster_members_and_at_least_two() {
        let nodes: Vec<String> = vec!["a-1".into(), "a-2".into(), "a-3".into()];
        assert_eq!(
            validate_volume_members(&["a-1".into(), "a-2".into()], &nodes),
            vec![]
        );
        assert!(codes(&validate_volume_members(&["a-1".into()], &nodes))
            .contains(&ValidationCode::TooFewVolumeMembers));
        assert!(codes(&validate_volume_members(
            &["a-1".into(), "b-9".into()],
            &nodes
        ))
        .contains(&ValidationCode::VolumeMemberOutsideCluster));
    }
}
