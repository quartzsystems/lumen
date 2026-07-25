//! Pure rules over a pool and the disks it would be built on.
//!
//! `zpool create` is the most destructive thing this appliance can be asked to
//! do: it reformats every disk it is given, and there is no undo. So every
//! check happens here, before anything is run, and it returns **every** problem
//! rather than the first — the same contract `lumen_net` and `lumen_virt`
//! established, with the same reason, plus one of its own: a dialog that
//! reports one problem at a time is a dialog somebody clicks through.

use serde::Serialize;

use crate::model::{
    valid_device_path, valid_new_pool_name, BlockDevice, Compression, Pool, VdevKind,
};

/// Why a request was rejected. Stable strings — the console matches on them and
/// tests assert on them, so they are part of the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    InvalidPoolName,
    DuplicatePoolName,
    NoDisksChosen,
    /// Fewer disks than the chosen arrangement means anything with.
    NotEnoughDisks,
    /// The same disk chosen twice, which would build a pool on one disk that
    /// believes it is two.
    DuplicateDisk,
    UnknownDisk,
    /// A disk that already holds something.
    DiskInUse,
    UnacknowledgedDestructiveOperation,
    InvalidAshift,
}

impl ValidationCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ValidationCode::InvalidPoolName => "invalid_pool_name",
            ValidationCode::DuplicatePoolName => "duplicate_pool_name",
            ValidationCode::NoDisksChosen => "no_disks_chosen",
            ValidationCode::NotEnoughDisks => "not_enough_disks",
            ValidationCode::DuplicateDisk => "duplicate_disk",
            ValidationCode::UnknownDisk => "unknown_disk",
            ValidationCode::DiskInUse => "disk_in_use",
            ValidationCode::UnacknowledgedDestructiveOperation => {
                "unacknowledged_destructive_operation"
            }
            ValidationCode::InvalidAshift => "invalid_ashift",
        }
    }
}

/// One rejection, tied to the field it belongs to so the dialog can render it
/// against the offending input rather than in a banner at the top.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationError {
    pub code: ValidationCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
}

impl ValidationError {
    pub fn new(code: ValidationCode, field: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            code,
            pool: None,
            field: field.map(str::to_string),
            message: message.into(),
        }
    }

    pub fn about(mut self, pool: &str) -> Self {
        self.pool = Some(pool.to_string());
        self
    }
}

/// The acknowledgement a destructive operation demands, named after what it
/// means rather than after what it guards.
#[derive(Debug, Clone, Copy, Default)]
pub struct Acknowledgements {
    pub may_lose_data: bool,
}

/// A pool the console is asking for. Deserialized straight off the request, so
/// `deny_unknown_fields` turns a typo into a 400 rather than a silently ignored
/// setting.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolCreate {
    pub name: String,
    #[serde(default)]
    pub vdev: VdevKind,
    /// The disks, as the picker reported them — a stable path, a kernel path,
    /// or a bare kernel name. The service resolves whichever was sent back to
    /// the stable one before anything is built.
    pub disks: Vec<String>,
    /// `None` means [`crate::model::DEFAULT_ASHIFT`], which is what anybody
    /// who has not thought about it should get.
    #[serde(default)]
    pub ashift: Option<u8>,
    #[serde(default)]
    pub compression: Compression,
    #[serde(default = "default_true")]
    pub autotrim: bool,
}

fn default_true() -> bool {
    true
}

/// Check a pool against the node it would be built on.
pub fn validate_pool(
    request: &PoolCreate,
    existing: &[Pool],
    devices: &[BlockDevice],
    ack: Acknowledgements,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let name = request.name.trim();

    if !valid_new_pool_name(name) {
        errors.push(ValidationError::new(
            ValidationCode::InvalidPoolName,
            Some("name"),
            format!(
                "\"{name}\" is not a usable pool name. Start with a letter, then use letters, \
                 digits, and any of _ - . : — and not a word the pool command already uses, like \
                 \"mirror\" or \"raidz2\"."
            ),
        ));
    } else if existing.iter().any(|pool| pool.name == name) {
        errors.push(ValidationError::new(
            ValidationCode::DuplicatePoolName,
            Some("name"),
            format!("This node already has a pool called \"{name}\"."),
        ));
    }

    if let Some(ashift) = request.ashift {
        // 9 is 512-byte sectors and 16 is 64 KiB; outside that it is not a
        // sector size, and the value cannot be changed after creation.
        if !(9..=16).contains(&ashift) {
            errors.push(ValidationError::new(
                ValidationCode::InvalidAshift,
                Some("ashift"),
                format!(
                    "{ashift} is not a sector size. Use 12 for the 4 KiB sectors every current \
                     disk has, or leave it unset."
                ),
            ));
        }
    }

    if request.disks.is_empty() {
        errors.push(ValidationError::new(
            ValidationCode::NoDisksChosen,
            Some("disks"),
            "A pool needs at least one disk.",
        ));
        return errors.into_iter().map(|e| e.about(name)).collect();
    }

    // The same disk twice would build a pool that believes it has redundancy
    // across two disks when it has one, which is worse than no redundancy
    // because it looks like some.
    let mut seen: Vec<&str> = Vec::new();
    for disk in &request.disks {
        if seen.contains(&disk.as_str()) {
            errors.push(ValidationError::new(
                ValidationCode::DuplicateDisk,
                Some("disks"),
                format!("\"{disk}\" was chosen more than once."),
            ));
        }
        seen.push(disk);
    }

    if request.disks.len() < request.vdev.min_disks() {
        errors.push(ValidationError::new(
            ValidationCode::NotEnoughDisks,
            Some("disks"),
            format!(
                "{} needs at least {} disks; {} {} chosen.",
                describe(request.vdev),
                request.vdev.min_disks(),
                request.disks.len(),
                if request.disks.len() == 1 {
                    "was"
                } else {
                    "were"
                }
            ),
        ));
    }

    for chosen in &request.disks {
        if !valid_device_path(chosen) && !devices.iter().any(|d| &d.name == chosen) {
            errors.push(ValidationError::new(
                ValidationCode::UnknownDisk,
                Some("disks"),
                format!("\"{chosen}\" is not a device this appliance will build a pool on."),
            ));
            continue;
        }
        let Some(device) = devices
            .iter()
            .find(|d| &d.path == chosen || &d.kernel_path == chosen || &d.name == chosen)
        else {
            // An unreadable device list is a *different* situation and is
            // handled below; a disk missing from a list that has entries is a
            // disk the node does not have.
            if !devices.is_empty() {
                errors.push(ValidationError::new(
                    ValidationCode::UnknownDisk,
                    Some("disks"),
                    format!("This node has no disk called \"{chosen}\"."),
                ));
            }
            continue;
        };

        // The whole reason the picker reports what is on a disk. Building a
        // pool on the one the appliance is running from is the single most
        // effective way to lose a node, and it must never happen by accident.
        if device.in_use && !ack.may_lose_data {
            errors.push(ValidationError::new(
                ValidationCode::DiskInUse,
                Some("disks"),
                format!(
                    "{} already has something on it ({}). Building a pool on it destroys that. \
                     Confirm that you understand this may lose data.",
                    device.name,
                    device.used_by.as_deref().unwrap_or("in use")
                ),
            ));
        }
    }

    errors.into_iter().map(|e| e.about(name)).collect()
}

/// How an arrangement reads in a sentence written for an operator.
fn describe(vdev: VdevKind) -> &'static str {
    match vdev {
        VdevKind::Stripe => "A stripe",
        VdevKind::Mirror => "A mirror",
        VdevKind::Raidz1 => "RAID-Z1",
        VdevKind::Raidz2 => "RAID-Z2",
        VdevKind::Raidz3 => "RAID-Z3",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PoolHealth;

    const TB: u64 = 1_000_000_000_000;

    fn free(name: &str) -> BlockDevice {
        BlockDevice {
            name: name.into(),
            path: format!("/dev/disk/by-id/scsi-{name}"),
            kernel_path: format!("/dev/{name}"),
            size: TB,
            ..BlockDevice::default()
        }
    }

    fn busy(name: &str, why: &str) -> BlockDevice {
        BlockDevice {
            in_use: true,
            used_by: Some(why.into()),
            ..free(name)
        }
    }

    fn node() -> Vec<BlockDevice> {
        vec![
            busy("sda", "mounted at /"),
            free("sdb"),
            free("sdc"),
            free("sdd"),
        ]
    }

    fn request(vdev: VdevKind, disks: &[&str]) -> PoolCreate {
        PoolCreate {
            name: "tank".into(),
            vdev,
            disks: disks.iter().map(|d| d.to_string()).collect(),
            ashift: None,
            compression: Compression::Lz4,
            autotrim: true,
        }
    }

    fn codes(errors: &[ValidationError]) -> Vec<ValidationCode> {
        errors.iter().map(|e| e.code).collect()
    }

    fn nothing() -> Acknowledgements {
        Acknowledgements::default()
    }

    fn acknowledged() -> Acknowledgements {
        Acknowledgements {
            may_lose_data: true,
        }
    }

    #[test]
    fn a_reasonable_pool_is_accepted() {
        let errors = validate_pool(
            &request(VdevKind::Mirror, &["/dev/sdb", "/dev/sdc"]),
            &[],
            &node(),
            nothing(),
        );
        assert!(errors.is_empty(), "{errors:#?}");
    }

    /// The check this whole module exists for.
    #[test]
    fn the_disk_the_appliance_runs_from_is_refused_unless_it_is_acknowledged() {
        let errors = validate_pool(
            &request(VdevKind::Stripe, &["/dev/sda"]),
            &[],
            &node(),
            nothing(),
        );
        assert_eq!(codes(&errors), vec![ValidationCode::DiskInUse]);
        // The message says what is on it, not just that something is.
        assert!(
            errors[0].message.contains("mounted at /"),
            "{}",
            errors[0].message
        );
        assert_eq!(errors[0].field.as_deref(), Some("disks"));

        // Acknowledged, it goes ahead — an operator rebuilding a node has to
        // be able to say so.
        assert!(validate_pool(
            &request(VdevKind::Stripe, &["/dev/sda"]),
            &[],
            &node(),
            acknowledged()
        )
        .is_empty());
    }

    #[test]
    fn every_problem_is_reported_not_just_the_first() {
        let mut bad = request(VdevKind::Raidz2, &["/dev/sda", "/dev/sda"]);
        bad.name = "mirror".into();
        bad.ashift = Some(3);
        let errors = validate_pool(&bad, &[], &node(), nothing());

        assert!(errors.len() >= 4, "{errors:#?}");
        for expected in [
            ValidationCode::InvalidPoolName,
            ValidationCode::InvalidAshift,
            ValidationCode::DuplicateDisk,
            ValidationCode::NotEnoughDisks,
        ] {
            assert!(
                codes(&errors).contains(&expected),
                "{expected:?} {errors:#?}"
            );
        }
        assert!(errors.iter().all(|e| e.field.is_some()));
    }

    /// Two disks in a "raidz2" is legal to `zpool` and is not what anybody
    /// meant — the floors are about meaning, not about what the tool accepts.
    #[test]
    fn an_arrangement_needs_enough_disks_to_mean_anything() {
        let errors = validate_pool(
            &request(VdevKind::Raidz2, &["/dev/sdb", "/dev/sdc"]),
            &[],
            &node(),
            nothing(),
        );
        assert_eq!(codes(&errors), vec![ValidationCode::NotEnoughDisks]);
        assert!(
            errors[0].message.contains("at least 4"),
            "{}",
            errors[0].message
        );

        // And a stripe of one is fine, because that is what a stripe is.
        assert!(validate_pool(
            &request(VdevKind::Stripe, &["/dev/sdb"]),
            &[],
            &node(),
            nothing()
        )
        .is_empty());
    }

    /// The same disk twice looks like redundancy and is not, which is worse
    /// than no redundancy at all.
    #[test]
    fn one_disk_cannot_be_both_halves_of_a_mirror() {
        let errors = validate_pool(
            &request(VdevKind::Mirror, &["/dev/sdb", "/dev/sdb"]),
            &[],
            &node(),
            nothing(),
        );
        assert!(codes(&errors).contains(&ValidationCode::DuplicateDisk));
    }

    #[test]
    fn a_pool_that_already_exists_is_refused_by_name() {
        let existing = vec![Pool {
            name: "tank".into(),
            health: PoolHealth::Online,
            ..Pool::default()
        }];
        let errors = validate_pool(
            &request(VdevKind::Stripe, &["/dev/sdb"]),
            &existing,
            &node(),
            nothing(),
        );
        assert_eq!(codes(&errors), vec![ValidationCode::DuplicatePoolName]);
        assert_eq!(errors[0].pool.as_deref(), Some("tank"));
    }

    #[test]
    fn a_disk_this_node_does_not_have_is_refused() {
        for chosen in ["/dev/sdz", "/etc/passwd", "/dev/../etc/shadow", "-rf"] {
            let errors = validate_pool(
                &request(VdevKind::Stripe, &[chosen]),
                &[],
                &node(),
                nothing(),
            );
            assert!(
                codes(&errors).contains(&ValidationCode::UnknownDisk),
                "{chosen:?} {errors:#?}"
            );
        }
    }

    #[test]
    fn a_pool_with_no_disks_says_so_once_rather_than_for_every_other_rule() {
        let errors = validate_pool(&request(VdevKind::Raidz2, &[]), &[], &node(), nothing());
        assert_eq!(codes(&errors), vec![ValidationCode::NoDisksChosen]);
    }

    /// The value cannot be changed after creation, so a nonsensical one has to
    /// be caught now rather than lived with.
    #[test]
    fn an_ashift_that_is_not_a_sector_size_is_refused() {
        for bad in [0, 8, 17, 64] {
            let mut request = request(VdevKind::Stripe, &["/dev/sdb"]);
            request.ashift = Some(bad);
            assert!(
                codes(&validate_pool(&request, &[], &node(), nothing()))
                    .contains(&ValidationCode::InvalidAshift),
                "{bad}"
            );
        }
        for good in [9, 12, 13, 16] {
            let mut request = request(VdevKind::Stripe, &["/dev/sdb"]);
            request.ashift = Some(good);
            assert!(
                validate_pool(&request, &[], &node(), nothing()).is_empty(),
                "{good}"
            );
        }
    }

    /// An unreadable device list is not an empty node — the same rule
    /// `lumen_virt` applies to a subsystem it cannot see. Refusing every pool
    /// because `/sys/block` could not be read would turn one broken thing
    /// into two.
    #[test]
    fn an_unreadable_disk_list_skips_its_check_rather_than_failing_everything() {
        let errors = validate_pool(
            &request(VdevKind::Mirror, &["/dev/sdb", "/dev/sdc"]),
            &[],
            &[],
            nothing(),
        );
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn a_typo_in_a_request_is_rejected_rather_than_ignored() {
        assert!(serde_json::from_str::<PoolCreate>(
            r#"{"name":"tank","disks":["/dev/sdb"],"compresion":"lz4"}"#
        )
        .is_err());
    }
}
