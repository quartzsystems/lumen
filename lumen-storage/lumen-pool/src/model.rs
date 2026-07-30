//! Names, ids, and device paths — and the one decision that keeps them
//! from needing a record.
//!
//! The compute domain names a machine's disk `vm-<vmid>-disk-<n>`, and it
//! has always done so. That name carries everything the storage layer needs
//! to know about identity, so this module makes the mapping **invertible**
//! rather than remembered:
//!
//! ```text
//!   vm-7-disk-3   →  vdisk 0x0703  →  /dev/ublkb1795
//!   /dev/ublkb1795 →  vdisk 0x0703  →  vm-7-disk-3
//! ```
//!
//! A vdisk id is the machine id shifted up by a byte with the disk index
//! in the low byte, and the ublk device id *is* the vdisk id. One
//! allocation, three names for it, and no lookup table anywhere.
//!
//! ## Why this is not a record
//!
//! docs/lumenfs.md's survey expected a sibling record type on
//! `ClusterRecord` mapping vdisks to names — the shape `VolumeRecord` has.
//! DRBD needs that because `/dev/drbd<minor>` says nothing about which
//! machine owns it: the minor is allocated from a pool, so the association
//! must be written down. Derivation removes the need, and with it a second
//! source of truth that could disagree with the engine about what exists.
//! The engine already knows which vdisks it has; asking it and deriving the
//! name cannot go stale, whereas a record can.
//!
//! **Recorded deviation**, then: no vdisk record. The pool, brick, and
//! slice-map records that phases 4 and 5 need are unaffected — those
//! describe things no id can encode.
//!
//! The cost is a stated ceiling, which is the honest trade: 256 disks per
//! machine, and machine ids below 2^24 so the device id stays inside the
//! `u32` the ublk driver takes. A machine with 257 disks is not a
//! deployment, it is a mistake, and it is refused by name rather than
//! silently aliased onto another machine's disk.

use serde::{Deserialize, Serialize};

/// Disks per machine. The low byte of a vdisk id.
pub const DISKS_PER_MACHINE: u64 = 256;

/// The largest machine id whose disks still fit a `u32` ublk device id.
pub const MAX_VMID: u64 = (u32::MAX as u64) / DISKS_PER_MACHINE;

/// A machine's disk, by the only two numbers that identify it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskName {
    pub vmid: u64,
    pub index: u64,
}

impl DiskName {
    /// Parse the compute domain's name. Anything else is not ours, and
    /// saying so is how [`crate::PoolService::disk_of`] stays a predicate.
    pub fn parse(name: &str) -> Option<DiskName> {
        let rest = name.strip_prefix("vm-")?;
        let (vmid, index) = rest.split_once("-disk-")?;
        let name = DiskName {
            vmid: vmid.parse().ok()?,
            index: index.parse().ok()?,
        };
        name.valid().then_some(name)
    }

    /// The vdisk id, and the ublk device id, which are the same number.
    pub fn vdisk(&self) -> Option<u64> {
        self.valid()
            .then(|| self.vmid * DISKS_PER_MACHINE + self.index)
    }

    /// Recover the name from the id — the inverse, and the reason no record
    /// is needed.
    pub fn from_vdisk(vdisk: u64) -> Option<DiskName> {
        let name = DiskName {
            vmid: vdisk / DISKS_PER_MACHINE,
            index: vdisk % DISKS_PER_MACHINE,
        };
        name.valid().then_some(name)
    }

    fn valid(&self) -> bool {
        self.vmid > 0 && self.vmid <= MAX_VMID && self.index < DISKS_PER_MACHINE
    }
}

impl std::fmt::Display for DiskName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "vm-{}-disk-{}", self.vmid, self.index)
    }
}

/// The guest device path for a vdisk — identical on every member, which is
/// what lets one domain document be valid wherever the machine runs.
pub fn device_path(vdisk: u64) -> String {
    format!("/dev/ublkb{vdisk}")
}

/// The vdisk behind a device path, or `None` for a path this engine did not
/// make. A local zvol, a DRBD minor, and a stray string all answer `None`.
pub fn vdisk_of_device(device: &str) -> Option<u64> {
    device.strip_prefix("/dev/ublkb")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_and_an_id_are_the_same_fact_in_two_forms() {
        let name = DiskName::parse("vm-7-disk-3").unwrap();
        assert_eq!(name, DiskName { vmid: 7, index: 3 });
        let vdisk = name.vdisk().unwrap();
        assert_eq!(vdisk, 7 * 256 + 3);
        assert_eq!(DiskName::from_vdisk(vdisk).unwrap(), name);
        assert_eq!(name.to_string(), "vm-7-disk-3");
        assert_eq!(device_path(vdisk), "/dev/ublkb1795");
        assert_eq!(vdisk_of_device("/dev/ublkb1795"), Some(vdisk));
    }

    #[test]
    fn the_round_trip_holds_for_every_index_and_a_spread_of_machines() {
        for vmid in [1u64, 2, 99, 1000, 65535, MAX_VMID] {
            for index in 0..DISKS_PER_MACHINE {
                let name = DiskName { vmid, index };
                let vdisk = name.vdisk().expect("valid");
                assert_eq!(DiskName::from_vdisk(vdisk), Some(name));
                assert_eq!(
                    vdisk_of_device(&device_path(vdisk)),
                    Some(vdisk),
                    "device path did not round trip for {name}"
                );
                // And no two disks anywhere share an id.
                assert_eq!(vdisk, vmid * 256 + index);
            }
        }
    }

    #[test]
    fn ids_never_collide_across_machines() {
        // The failure this encoding must not have: machine 1's disk 256
        // aliasing machine 2's disk 0. There is no disk 256.
        assert!(DiskName {
            vmid: 1,
            index: 256
        }
        .vdisk()
        .is_none());
        let mut seen = std::collections::HashSet::new();
        for vmid in 1..=40u64 {
            for index in 0..DISKS_PER_MACHINE {
                let vdisk = DiskName { vmid, index }.vdisk().unwrap();
                assert!(seen.insert(vdisk), "id {vdisk} was handed out twice");
            }
        }
    }

    #[test]
    fn a_device_path_this_engine_did_not_make_is_not_ours() {
        // `disk_of` leans on this being a predicate, not a guess.
        for foreign in [
            "/dev/drbd1",
            "/dev/zvol/lumen/vm-7-disk-0",
            "/dev/ublkb",
            "/dev/ublkbx",
            "/dev/ublkc7",
            "/dev/sda",
            "",
            "ublkb7",
        ] {
            assert_eq!(vdisk_of_device(foreign), None, "{foreign} was claimed");
        }
    }

    #[test]
    fn names_outside_the_convention_are_refused_rather_than_reshaped() {
        for wrong in [
            "vm-0-disk-0",        // machine 0 does not exist
            "vm-7-disk-256",      // past the per-machine ceiling
            "vm--disk-1",         // no machine id
            "vm-7-disk-",         // no index
            "vm-7-disk",          // not the pattern
            "vm-7",               // not the pattern
            "vm-x-disk-1",        // not a number
            "vm-7-disk-x",        // not a number
            "iso-library-disk-1", // somebody else's naming
            "",
        ] {
            assert!(DiskName::parse(wrong).is_none(), "{wrong} was accepted");
        }
        // And the ceiling on machine ids is real, because the device id has
        // to fit what the ublk driver takes.
        assert!(DiskName {
            vmid: MAX_VMID + 1,
            index: 0
        }
        .vdisk()
        .is_none());
        assert!(
            DiskName {
                vmid: MAX_VMID,
                index: 255
            }
            .vdisk()
            .unwrap()
                <= u32::MAX as u64
        );
    }
}
