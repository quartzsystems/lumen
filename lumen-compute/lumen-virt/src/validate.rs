//! Pure validation over a machine and the node it would run on.
//!
//! Returns every problem it finds, each with a machine-readable `code` the
//! console pins a field to and tests assert on, plus a human sentence the
//! console renders verbatim. No hypervisor, no storage, no I/O — exactly the
//! shape of `lumen_net::validate`.
//!
//! The rule these checks share: refuse before touching the hypervisor. A
//! definition the hypervisor accepts and then cannot start is worse than a
//! refusal, because the failure arrives later and somewhere else.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::model::{valid_tag, valid_vm_name, CpuTopology, VmConfig, FIRST_VMID, LAST_VMID};
use crate::state::{DomainState, HostInfo};

/// The smallest machine worth defining. Below this a current guest does not
/// finish starting, and the failure looks like a hypervisor fault rather than
/// a configuration mistake.
pub const MIN_MEMORY_MIB: u64 = 128;

/// Why a machine was rejected. Stable strings: the console maps them to form
/// fields and the API tests assert on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    /// Another machine already has this identifier.
    DuplicateVmid,
    /// Another machine already has this name.
    DuplicateName,
    /// Not a usable machine name.
    InvalidName,
    /// A tag with a comma, a space, or nothing in it.
    InvalidTag,
    /// Outside the range Lumen allocates from.
    VmidOutOfRange,
    /// Zero processors.
    InvalidVcpus,
    /// More processors than the node has threads.
    VcpusExceedHost,
    /// Less memory than a guest can start with.
    InvalidMemory,
    /// More memory than the node has, once the node's own reserve is taken
    /// off.
    MemoryExceedsHost,
    /// The topology multiplies out to a different number than the processor
    /// count. The hypervisor refuses this outright, so refusing it here is the
    /// difference between a field error and a failed define.
    TopologyMismatch,
    /// An adapter names a bridge that is not on this node.
    UnknownBridge,
    /// A disk names a pool that is not on this node.
    UnknownPool,
    /// A disk larger than the pool has room for.
    DiskExceedsPool,
    /// A tag outside 1..=4094.
    InvalidVlanTag,
    /// Two disks with the same target, or two adapters with the same address.
    DuplicateDevice,
    /// Something that takes a running machine down, or removes data, without
    /// the caller saying so.
    UnacknowledgedDestructiveOperation,
}

impl ValidationCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ValidationCode::DuplicateVmid => "duplicate_vmid",
            ValidationCode::DuplicateName => "duplicate_name",
            ValidationCode::InvalidName => "invalid_name",
            ValidationCode::InvalidTag => "invalid_tag",
            ValidationCode::VmidOutOfRange => "vmid_out_of_range",
            ValidationCode::InvalidVcpus => "invalid_vcpus",
            ValidationCode::VcpusExceedHost => "vcpus_exceed_host",
            ValidationCode::InvalidMemory => "invalid_memory",
            ValidationCode::MemoryExceedsHost => "memory_exceeds_host",
            ValidationCode::TopologyMismatch => "topology_mismatch",
            ValidationCode::UnknownBridge => "unknown_bridge",
            ValidationCode::UnknownPool => "unknown_pool",
            ValidationCode::DiskExceedsPool => "disk_exceeds_pool",
            ValidationCode::InvalidVlanTag => "invalid_vlan_tag",
            ValidationCode::DuplicateDevice => "duplicate_device",
            ValidationCode::UnacknowledgedDestructiveOperation => {
                "unacknowledged_destructive_operation"
            }
        }
    }
}

/// One problem. `field` lets the console attach the sentence to the offending
/// input instead of dumping it in a banner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationError {
    pub code: ValidationCode,
    /// The machine the problem is about, when it is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vm: Option<String>,
    /// The model field ("memory_mib", "bridge", "vcpus", …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// One sentence, shown to the operator as written.
    pub message: String,
}

impl ValidationError {
    pub fn new(code: ValidationCode, message: impl Into<String>) -> Self {
        Self {
            code,
            vm: None,
            field: None,
            message: message.into(),
        }
    }

    pub fn at(mut self, vm: impl Into<String>, field: &str) -> Self {
        self.vm = Some(vm.into());
        self.field = Some(field.to_string());
        self
    }
}

/// What the caller is willing to risk. An operation that takes a running
/// machine down without warning it, or removes a disk's contents, needs this.
#[derive(Debug, Clone, Copy, Default)]
pub struct Acknowledgements {
    /// `i_understand_this_may_lose_data` from the request body.
    pub may_lose_data: bool,
}

/// A disk that does not exist yet, as the validator sees it. Used by both
/// create (where none of the disks exist) and attach (where one does not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDisk {
    pub pool: String,
    /// Bytes.
    pub size: u64,
}

/// What the node has, gathered once and handed to the pure checks.
///
/// `bridges_known` and `pools_known` are the important part: "networking is
/// unreadable" is not the same answer as "there are no bridges". Refusing a
/// machine because a subsystem is down would turn one broken thing into two,
/// so an unreadable subsystem skips its check and says so in the log.
#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    pub host: HostInfo,
    pub bridges: Vec<String>,
    pub bridges_known: bool,
    /// Free bytes per pool, for every pool on the node.
    pub pools: Vec<(String, u64)>,
    pub pools_known: bool,
    /// Every machine already defined, other than the one being validated.
    pub existing: Vec<(u32, String)>,
}

impl HostFacts {
    pub fn free_space(&self, pool: &str) -> Option<u64> {
        self.pools
            .iter()
            .find(|(name, _)| name == pool)
            .map(|(_, free)| *free)
    }
}

/// Validate a machine against the node it would run on.
///
/// `planned` is the disks that do not exist yet; the ones in `config.disks`
/// already do, and the pool has already accounted for them.
pub fn validate(
    config: &VmConfig,
    planned: &[PlannedDisk],
    facts: &HostFacts,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    check_identity(config, facts, &mut errors);
    check_size(config, facts, &mut errors);
    check_devices(config, facts, &mut errors);
    check_planned_disks(config, planned, facts, &mut errors);
    errors
}

fn check_identity(config: &VmConfig, facts: &HostFacts, errors: &mut Vec<ValidationError>) {
    if !valid_vm_name(&config.name) {
        errors.push(
            ValidationError::new(
                ValidationCode::InvalidName,
                format!(
                    "\"{}\" is not a usable machine name — up to 63 letters, digits, dots, \
                     dashes, and underscores.",
                    config.name
                ),
            )
            .at(&config.name, "name"),
        );
    }
    if !(FIRST_VMID..=LAST_VMID).contains(&config.vmid) {
        errors.push(
            ValidationError::new(
                ValidationCode::VmidOutOfRange,
                format!(
                    "{} is outside the range Lumen uses — machine identifiers run from \
                     {FIRST_VMID} to {LAST_VMID}.",
                    config.vmid
                ),
            )
            .at(&config.name, "vmid"),
        );
    }
    if let Some((_, other)) = facts.existing.iter().find(|(id, _)| *id == config.vmid) {
        errors.push(
            ValidationError::new(
                ValidationCode::DuplicateVmid,
                format!(
                    "Machine identifier {} is already taken by \"{other}\".",
                    config.vmid
                ),
            )
            .at(&config.name, "vmid"),
        );
    }
    if facts
        .existing
        .iter()
        .any(|(_, name)| name.eq_ignore_ascii_case(&config.name))
    {
        errors.push(
            ValidationError::new(
                ValidationCode::DuplicateName,
                format!("A machine named \"{}\" already exists.", config.name),
            )
            .at(&config.name, "name"),
        );
    }
    for tag in &config.tags {
        if !valid_tag(tag) {
            errors.push(
                ValidationError::new(
                    ValidationCode::InvalidTag,
                    format!(
                        "\"{tag}\" is not a usable tag — up to 32 letters, digits, dots, dashes, \
                         and underscores, and no commas."
                    ),
                )
                .at(&config.name, "tags"),
            );
        }
    }
}

fn check_size(config: &VmConfig, facts: &HostFacts, errors: &mut Vec<ValidationError>) {
    if config.vcpus == 0 {
        errors.push(
            ValidationError::new(
                ValidationCode::InvalidVcpus,
                "A machine needs at least one processor.",
            )
            .at(&config.name, "vcpus"),
        );
    } else if facts.host.cpus > 0 && config.vcpus > facts.host.cpus {
        errors.push(
            ValidationError::new(
                ValidationCode::VcpusExceedHost,
                format!(
                    "This node has {} processors, so a machine cannot be given {}.",
                    facts.host.cpus, config.vcpus
                ),
            )
            .at(&config.name, "vcpus"),
        );
    }

    if config.memory_mib < MIN_MEMORY_MIB {
        errors.push(
            ValidationError::new(
                ValidationCode::InvalidMemory,
                format!("A machine needs at least {MIN_MEMORY_MIB} MiB of memory."),
            )
            .at(&config.name, "memory_mib"),
        );
    } else {
        let assignable = facts.host.assignable_memory_mib();
        if facts.host.memory_mib > 0 && config.memory_mib > assignable {
            errors.push(
                ValidationError::new(
                    ValidationCode::MemoryExceedsHost,
                    format!(
                        "This node has {} MiB of memory and keeps {} MiB for itself, so a machine \
                         cannot be given {} MiB.",
                        facts.host.memory_mib,
                        crate::state::HOST_MEMORY_RESERVE_MIB,
                        config.memory_mib
                    ),
                )
                .at(&config.name, "memory_mib"),
            );
        }
    }

    if let Some(topology @ CpuTopology { .. }) = config.topology {
        if topology.sockets == 0 || topology.cores == 0 || topology.threads == 0 {
            errors.push(
                ValidationError::new(
                    ValidationCode::TopologyMismatch,
                    "A processor layout needs at least one socket, one core, and one thread.",
                )
                .at(&config.name, "topology"),
            );
        } else if topology.total() != config.vcpus {
            errors.push(
                ValidationError::new(
                    ValidationCode::TopologyMismatch,
                    format!(
                        "The processor layout comes to {} ({} × {} × {}), but the machine is set \
                         to {} — the hypervisor requires them to match.",
                        topology.total(),
                        topology.sockets,
                        topology.cores,
                        topology.threads,
                        config.vcpus
                    ),
                )
                .at(&config.name, "topology"),
            );
        }
    }
}

fn check_devices(config: &VmConfig, facts: &HostFacts, errors: &mut Vec<ValidationError>) {
    let mut targets: HashSet<&str> = HashSet::new();
    for disk in &config.disks {
        if !targets.insert(disk.id.as_str()) {
            errors.push(
                ValidationError::new(
                    ValidationCode::DuplicateDevice,
                    format!("More than one disk is attached as \"{}\".", disk.id),
                )
                .at(&config.name, "disks"),
            );
        }
    }

    let mut macs: HashSet<String> = HashSet::new();
    for nic in &config.nics {
        if !macs.insert(nic.id.to_ascii_lowercase()) {
            errors.push(
                ValidationError::new(
                    ValidationCode::DuplicateDevice,
                    format!(
                        "More than one adapter has the hardware address \"{}\".",
                        nic.id
                    ),
                )
                .at(&config.name, "nics"),
            );
        }
        if let Some(tag) = nic.vlan_tag {
            if !(1..=4094).contains(&tag) {
                errors.push(
                    ValidationError::new(
                        ValidationCode::InvalidVlanTag,
                        format!("Virtual network tag {tag} is out of range — use 1 to 4094."),
                    )
                    .at(&config.name, "vlan_tag"),
                );
            }
        }
        // A bridge that is not there produces a machine that defines cleanly
        // and then has no network at all, with nothing anywhere saying why.
        if facts.bridges_known && !facts.bridges.iter().any(|b| b == &nic.bridge) {
            let available = if facts.bridges.is_empty() {
                "this node has none".to_string()
            } else {
                format!("this node has {}", facts.bridges.join(", "))
            };
            errors.push(
                ValidationError::new(
                    ValidationCode::UnknownBridge,
                    format!(
                        "There is no bridge named \"{}\" on this node — {available}.",
                        nic.bridge
                    ),
                )
                .at(&config.name, "bridge"),
            );
        }
    }
}

fn check_planned_disks(
    config: &VmConfig,
    planned: &[PlannedDisk],
    facts: &HostFacts,
    errors: &mut Vec<ValidationError>,
) {
    if !facts.pools_known {
        return;
    }
    // Several disks on one pool have to fit together, not just individually.
    let mut pools: Vec<(&str, u64)> = Vec::new();
    for disk in planned {
        match pools.iter_mut().find(|(name, _)| *name == disk.pool) {
            Some((_, total)) => *total += disk.size,
            None => pools.push((disk.pool.as_str(), disk.size)),
        }
    }

    for (pool, wanted) in pools {
        let Some(free) = facts.free_space(pool) else {
            errors.push(
                ValidationError::new(
                    ValidationCode::UnknownPool,
                    format!("There is no pool named \"{pool}\" on this node."),
                )
                .at(&config.name, "pool"),
            );
            continue;
        };
        if wanted > free {
            errors.push(
                ValidationError::new(
                    ValidationCode::DiskExceedsPool,
                    format!(
                        "\"{pool}\" has {} GiB free, which is not enough for {} GiB of disks.",
                        free / (1024 * 1024 * 1024),
                        wanted / (1024 * 1024 * 1024)
                    ),
                )
                .at(&config.name, "size_gib"),
            );
        }
    }
}

/// The guard on operations that take a running machine down without letting
/// the guest finish, or that remove a disk's contents.
///
/// Separate from [`validate`] because it is not about the shape of a machine:
/// the same configuration is fine to define and not fine to force off while
/// somebody is using it.
pub fn check_destructive(
    what: &str,
    vm: &str,
    state: DomainState,
    removes_data: bool,
    ack: Acknowledgements,
) -> Option<ValidationError> {
    if ack.may_lose_data {
        return None;
    }
    let running = state.is_running();
    if !running && !removes_data {
        return None;
    }
    let reason = match (running, removes_data) {
        (true, true) => format!(
            "\"{vm}\" is running, and {what} also removes its disks. Confirm that you understand \
             the machine will be stopped without warning and its data destroyed."
        ),
        (true, false) => format!(
            "\"{vm}\" is running. {what} does not give the guest a chance to shut down, which can \
             lose whatever it had not written yet — confirm that you understand the risk."
        ),
        (false, true) => format!(
            "{what} destroys the disks belonging to \"{vm}\" and cannot be undone. Confirm that \
             you understand the risk."
        ),
        (false, false) => unreachable!("handled above"),
    };
    Some(
        ValidationError::new(ValidationCode::UnacknowledgedDestructiveOperation, reason)
            .at(vm, "i_understand_this_may_lose_data"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{generate_mac, CacheMode, DiskBus, NicModel, VmDisk, VmNic};

    fn codes(errors: &[ValidationError]) -> Vec<ValidationCode> {
        errors.iter().map(|e| e.code).collect()
    }

    fn facts() -> HostFacts {
        HostFacts {
            host: HostInfo {
                node: "lumen".into(),
                cpus: 16,
                memory_mib: 32_768,
                hypervisor_version: Some("11.10.0".into()),
            },
            bridges: vec!["br0".into()],
            bridges_known: true,
            pools: vec![("boot".into(), 512 * 1024 * 1024 * 1024)],
            pools_known: true,
            existing: vec![(100, "db01".into())],
        }
    }

    /// The healthy baseline every case below perturbs.
    fn baseline() -> VmConfig {
        VmConfig {
            vmid: 101,
            name: "web01".into(),
            vcpus: 4,
            memory_mib: 8192,
            disks: vec![VmDisk {
                id: "vda".into(),
                bus: DiskBus::VirtioBlk,
                source: "/dev/zvol/boot/lumen/vm-101-disk-0".into(),
                size: 34_359_738_368,
                cache: CacheMode::None,
                discard: true,
                boot_index: Some(1),
            }],
            nics: vec![VmNic {
                id: generate_mac(101, 0),
                model: NicModel::Virtio,
                bridge: "br0".into(),
                vlan_tag: None,
                boot_index: None,
            }],
            ..VmConfig::default()
        }
    }

    #[test]
    fn baseline_is_clean() {
        assert_eq!(validate(&baseline(), &[], &facts()), vec![]);
    }

    #[test]
    fn table_driven_rejections() {
        /// A named perturbation of the healthy baseline and the code it must
        /// produce.
        type Case = (&'static str, fn(&mut VmConfig), ValidationCode);

        let cases: Vec<Case> = vec![
            (
                "an identifier another machine already has",
                |c| c.vmid = 100,
                ValidationCode::DuplicateVmid,
            ),
            (
                "a name another machine already has",
                |c| c.name = "db01".into(),
                ValidationCode::DuplicateName,
            ),
            (
                "a name that differs only in case",
                |c| c.name = "DB01".into(),
                ValidationCode::DuplicateName,
            ),
            (
                "a name with a separator in it",
                |c| c.name = "web/01".into(),
                ValidationCode::InvalidName,
            ),
            (
                "an identifier below the range",
                |c| c.vmid = 99,
                ValidationCode::VmidOutOfRange,
            ),
            (
                "no processors at all",
                |c| c.vcpus = 0,
                ValidationCode::InvalidVcpus,
            ),
            (
                "more processors than the node has threads",
                |c| c.vcpus = 64,
                ValidationCode::VcpusExceedHost,
            ),
            (
                "less memory than a guest can start with",
                |c| c.memory_mib = 32,
                ValidationCode::InvalidMemory,
            ),
            (
                "more memory than the node has left",
                |c| c.memory_mib = 32_768,
                ValidationCode::MemoryExceedsHost,
            ),
            (
                "a layout that does not come to the processor count",
                |c| {
                    c.topology = Some(CpuTopology {
                        sockets: 1,
                        cores: 3,
                        threads: 1,
                    })
                },
                ValidationCode::TopologyMismatch,
            ),
            (
                "a layout with a zero in it",
                |c| {
                    c.topology = Some(CpuTopology {
                        sockets: 1,
                        cores: 4,
                        threads: 0,
                    })
                },
                ValidationCode::TopologyMismatch,
            ),
            (
                "a bridge this node does not have",
                |c| c.nics[0].bridge = "br9".into(),
                ValidationCode::UnknownBridge,
            ),
            (
                "a virtual network tag out of range",
                |c| c.nics[0].vlan_tag = Some(4095),
                ValidationCode::InvalidVlanTag,
            ),
            (
                "two disks on the same target",
                |c| {
                    let mut second = c.disks[0].clone();
                    second.source = "/dev/zvol/boot/lumen/vm-101-disk-1".into();
                    c.disks.push(second);
                },
                ValidationCode::DuplicateDevice,
            ),
            (
                "two adapters with one hardware address",
                |c| {
                    let second = c.nics[0].clone();
                    c.nics.push(second);
                },
                ValidationCode::DuplicateDevice,
            ),
            (
                "a tag with a comma in it",
                |c| c.tags = vec!["one,two".into()],
                ValidationCode::InvalidTag,
            ),
        ];

        for (name, mutate, expected) in cases {
            let mut config = baseline();
            mutate(&mut config);
            let errors = validate(&config, &[], &facts());
            assert!(
                codes(&errors).contains(&expected),
                "{name}: expected {expected:?}, got {:?}",
                codes(&errors)
            );
        }
    }

    #[test]
    fn a_disk_larger_than_the_pool_is_refused_before_anything_is_created() {
        let planned = vec![PlannedDisk {
            pool: "boot".into(),
            size: 1024 * 1024 * 1024 * 1024, // 1 TiB into 512 GiB
        }];
        let errors = validate(&baseline(), &planned, &facts());
        assert_eq!(codes(&errors), vec![ValidationCode::DiskExceedsPool]);
    }

    /// Two disks that each fit but do not fit together.
    #[test]
    fn disks_on_one_pool_are_measured_together() {
        let planned = vec![
            PlannedDisk {
                pool: "boot".into(),
                size: 400 * 1024 * 1024 * 1024,
            },
            PlannedDisk {
                pool: "boot".into(),
                size: 400 * 1024 * 1024 * 1024,
            },
        ];
        let errors = validate(&baseline(), &planned, &facts());
        assert_eq!(codes(&errors), vec![ValidationCode::DiskExceedsPool]);
    }

    #[test]
    fn a_pool_this_node_does_not_have_is_refused() {
        let planned = vec![PlannedDisk {
            pool: "tank".into(),
            size: 1024,
        }];
        let errors = validate(&baseline(), &planned, &facts());
        assert_eq!(codes(&errors), vec![ValidationCode::UnknownPool]);
    }

    /// A subsystem that cannot be read is not the same as a subsystem that
    /// says no. Refusing to define a machine because networking is down would
    /// turn one broken thing into two.
    #[test]
    fn an_unreadable_subsystem_skips_its_check_rather_than_failing_everything() {
        let mut facts = facts();
        facts.bridges_known = false;
        facts.bridges = Vec::new();
        facts.pools_known = false;
        facts.pools = Vec::new();

        let mut config = baseline();
        config.nics[0].bridge = "br9".into();
        let planned = vec![PlannedDisk {
            pool: "nonexistent".into(),
            size: u64::MAX,
        }];
        assert_eq!(validate(&config, &planned, &facts), vec![]);
    }

    #[test]
    fn every_problem_is_reported_not_just_the_first() {
        let mut config = baseline();
        config.vcpus = 0;
        config.memory_mib = 1;
        config.name = "bad name".into();
        let errors = validate(&config, &[], &facts());
        assert!(codes(&errors).contains(&ValidationCode::InvalidVcpus));
        assert!(codes(&errors).contains(&ValidationCode::InvalidMemory));
        assert!(codes(&errors).contains(&ValidationCode::InvalidName));
    }

    #[test]
    fn errors_carry_the_field_the_console_pins_them_to() {
        let mut config = baseline();
        config.nics[0].bridge = "br9".into();
        let errors = validate(&config, &[], &facts());
        let err = &errors[0];
        assert_eq!(err.code, ValidationCode::UnknownBridge);
        assert_eq!(err.field.as_deref(), Some("bridge"));
        assert_eq!(err.vm.as_deref(), Some("web01"));
        assert!(err.message.ends_with('.'), "message must be a sentence");
        // …and it says what the node does have, not just what it does not.
        assert!(err.message.contains("br0"), "{}", err.message);
    }

    #[test]
    fn stopping_a_running_machine_without_warning_it_needs_an_acknowledgement() {
        let refused = check_destructive(
            "Stopping it",
            "web01",
            DomainState::Running,
            false,
            Acknowledgements::default(),
        );
        assert_eq!(
            refused.as_ref().map(|e| e.code),
            Some(ValidationCode::UnacknowledgedDestructiveOperation)
        );
        assert_eq!(
            refused.as_ref().and_then(|e| e.field.as_deref()),
            Some("i_understand_this_may_lose_data")
        );

        // Acknowledged, or not running and not removing anything: allowed.
        assert!(check_destructive(
            "Stopping it",
            "web01",
            DomainState::Running,
            false,
            Acknowledgements {
                may_lose_data: true
            },
        )
        .is_none());
        assert!(check_destructive(
            "Removing it",
            "web01",
            DomainState::ShutOff,
            false,
            Acknowledgements::default(),
        )
        .is_none());
    }

    /// Removing the disks is destructive whether or not the machine is up, and
    /// the sentence says which case the operator is in.
    #[test]
    fn destroying_disks_always_needs_an_acknowledgement() {
        let stopped = check_destructive(
            "Removing it",
            "web01",
            DomainState::ShutOff,
            true,
            Acknowledgements::default(),
        )
        .expect("refused");
        assert!(
            stopped.message.contains("destroys the disks"),
            "{}",
            stopped.message
        );

        let running = check_destructive(
            "removing it",
            "web01",
            DomainState::Running,
            true,
            Acknowledgements::default(),
        )
        .expect("refused");
        assert!(running.message.contains("running"), "{}", running.message);
        assert!(
            running.message.contains("data destroyed"),
            "{}",
            running.message
        );
    }

    /// A node that reports nothing about itself must not refuse every machine
    /// on the grounds that it has no processors.
    #[test]
    fn a_node_that_reports_nothing_about_itself_does_not_block_everything() {
        let facts = HostFacts {
            host: HostInfo::default(),
            bridges_known: false,
            pools_known: false,
            existing: Vec::new(),
            ..HostFacts::default()
        };
        assert_eq!(validate(&baseline(), &[], &facts), vec![]);
    }
}
