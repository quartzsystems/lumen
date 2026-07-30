//! What disks the node has, and which of them are already spoken for.
//!
//! This module exists for one reason: **`zpool create` destroys whatever was on
//! the disks it is given.** A picker that lists `/dev/sda` and `/dev/sdb` with
//! no more information than that is a picker that will eventually be used to
//! reformat the disk the appliance is running from.
//!
//! So every candidate is reported with what is already on it, in words, and the
//! service refuses one that is in use unless the operator says otherwise *and*
//! acknowledges it. The list is what the node actually reports, never a guess.
//!
//! ## `/sys`, not `lsblk`
//!
//! Everything here is a small read under `/sys/block`, `/proc/mounts`, and
//! `/proc/swaps` — all of which `ProtectSystem=strict` leaves readable, and
//! `ProtectKernelTunables` makes read-only rather than unreadable. `lsblk`
//! would be a subprocess and a JSON schema for the same four facts. This is the
//! same reasoning `lumen_sys::state` uses for reading `/etc/passwd` rather than
//! running `getent`.
//!
//! ## Why the by-id path matters
//!
//! `/dev/sdb` is whatever the kernel enumerated second *this boot*. A pool
//! built on it can come back after a reboot pointing at a different disk, and
//! that is a genuinely bad afternoon. `/dev/disk/by-id/…` is the model and
//! serial number and does not move, so [`BlockDevice::path`] is that whenever
//! the node has one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::{BlockDevice, LumenBrick};

/// A sector, as `/sys/block/<name>/size` counts them. Always 512 regardless of
/// the disk's real block size — this is the kernel's unit, not the disk's.
const SECTOR: u64 = 512;

/// Where the node describes itself. Overridable so tests read a directory tree
/// of their own rather than the machine running them.
#[derive(Debug, Clone)]
pub struct DeviceRoots {
    pub sys_block: PathBuf,
    pub dev_by_id: PathBuf,
    pub proc_mounts: PathBuf,
    pub proc_swaps: PathBuf,
    /// Where the device nodes themselves live — read for exactly one thing,
    /// the LumenFS superblock probe.
    pub dev: PathBuf,
}

impl Default for DeviceRoots {
    fn default() -> Self {
        Self {
            sys_block: "/sys/block".into(),
            dev_by_id: "/dev/disk/by-id".into(),
            proc_mounts: "/proc/mounts".into(),
            proc_swaps: "/proc/swaps".into(),
            dev: "/dev".into(),
        }
    }
}

impl DeviceRoots {
    pub fn under(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            sys_block: root.join("sys/block"),
            dev_by_id: root.join("dev/disk/by-id"),
            proc_mounts: root.join("proc/mounts"),
            proc_swaps: root.join("proc/swaps"),
            dev: root.join("dev"),
        }
    }
}

/// Kernel names that are never a disk somebody builds a pool on.
///
/// `zd*` is the important one: those are ZFS's own volumes, and offering a
/// guest's disk as a candidate for a new pool would be a spectacular way to
/// lose a virtual machine.
fn is_a_real_disk(name: &str) -> bool {
    const NEVER: [&str; 7] = ["loop", "ram", "zram", "dm-", "md", "sr", "zd"];
    !NEVER.iter().any(|prefix| name.starts_with(prefix))
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// What a LumenFS probe of one disk's first bytes found.
enum BrickProbe {
    /// No brick here.
    None,
    /// A brick of another format version. Spoken for — but the only fact its
    /// layout is trusted for is its version number.
    Foreign(u32),
    /// A brick this release can read: whose pool, which tier.
    Known(LumenBrick),
}

/// Read the disk's own first sector and ask whether a brick superblock is
/// in it. Failure to open or read is an ordinary "no": a scan must never
/// error a whole disk list because one device refused a read.
fn lumenfs_probe(dev: &Path) -> BrickProbe {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(dev) else {
        return BrickProbe::None;
    };
    let mut head = [0u8; 144];
    if file.read_exact(&mut head).is_err() {
        return BrickProbe::None;
    }
    match lumen_fs::Superblock::decode(&head) {
        Ok(Some(sb)) => BrickProbe::Known(LumenBrick {
            pool_uuid: hex(&sb.pool_uuid),
            brick_uuid: hex(&sb.brick_uuid),
            tier: sb.tier,
            wal_holder: sb.wal_holder,
        }),
        Ok(None) => BrickProbe::None,
        Err(lumen_fs::FsError::UnsupportedVersion(version)) => BrickProbe::Foreign(version),
        Err(_) => BrickProbe::None,
    }
}

fn hex(bytes: &[u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every disk on the node, with what is on it.
pub async fn list(roots: &DeviceRoots) -> Vec<BlockDevice> {
    let roots = roots.clone();
    // A directory of small synchronous reads; doing it on a blocking thread
    // keeps it off the runtime rather than pretending it is async.
    tokio::task::spawn_blocking(move || scan(&roots))
        .await
        .unwrap_or_default()
}

fn scan(roots: &DeviceRoots) -> Vec<BlockDevice> {
    let by_id = stable_paths(&roots.dev_by_id);
    let claims = claims(roots);

    let Ok(entries) = std::fs::read_dir(&roots.sys_block) else {
        return Vec::new();
    };

    let mut disks: Vec<BlockDevice> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_a_real_disk(&name) {
                return None;
            }
            let dir = entry.path();
            let size = read_trimmed(&dir.join("size"))?
                .parse::<u64>()
                .ok()?
                .saturating_mul(SECTOR);
            // A zero-size entry is a card reader with no card in it, not a
            // disk somebody can build a pool on.
            if size == 0 {
                return None;
            }

            let kernel_path = format!("/dev/{name}");
            let partitions = partitions_of(&dir, &name);

            // What is on it: the disk itself, or any of its partitions. A
            // partition being mounted is what makes the whole disk unusable,
            // and it is the case that matters — the root filesystem is never
            // on the whole disk.
            let mut used_by: Vec<String> = Vec::new();
            for candidate in std::iter::once(name.clone()).chain(partitions.iter().cloned()) {
                if let Some(claim) = claims.get(&candidate) {
                    used_by.push(claim.clone());
                }
            }
            // A LumenFS brick claims its whole disk, and the superblock is
            // the record — read off the platter exactly as mounts and swap
            // are read from /proc, so a brick can never be offered as free
            // because a config file forgot it. Counted as claimed: the scan
            // cannot tell a serving pool's brick from an abandoned one, the
            // same blindness it has toward imported ZFS pools, and the same
            // conservative answer — the pool destroy workflow wipes its own
            // bricks through its own guards.
            let lumenfs = match lumenfs_probe(&roots.dev.join(&name)) {
                BrickProbe::Known(brick) => {
                    used_by.push(format!(
                        "LumenFS brick (pool {}, tier {})",
                        &brick.pool_uuid[..8],
                        brick.tier
                    ));
                    Some(brick)
                }
                BrickProbe::Foreign(version) => {
                    used_by.push(format!(
                        "LumenFS brick (format v{version}, not this release's)"
                    ));
                    None
                }
                BrickProbe::None => None,
            };
            // Whether anything live has it, recorded before the partition
            // count is folded into the same sentence. The two are the same
            // word to a reader and different answers to a wipe.
            let claimed = !used_by.is_empty();
            if used_by.is_empty() && !partitions.is_empty() {
                used_by.push(format!(
                    "{} partition{}",
                    partitions.len(),
                    if partitions.len() == 1 { "" } else { "s" }
                ));
            }
            used_by.sort();
            used_by.dedup();

            Some(BlockDevice {
                claimed,
                partitions: partitions.len(),
                // The scan does not get a vote on this: it cannot see an
                // imported pool. The service decides; see `BlockDevice`.
                wipeable: false,
                path: by_id
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| kernel_path.clone()),
                model: read_trimmed(&dir.join("device/model")),
                serial: read_trimmed(&dir.join("device/serial")),
                rotational: read_trimmed(&dir.join("queue/rotational")).as_deref() == Some("1"),
                removable: read_trimmed(&dir.join("removable")).as_deref() == Some("1"),
                in_use: !used_by.is_empty(),
                used_by: (!used_by.is_empty()).then(|| used_by.join(", ")),
                kernel_path,
                name,
                size,
                lumenfs,
            })
        })
        .collect();

    // Free disks first — they are the ones an operator is looking for — then by
    // name so the order does not change between reads.
    disks.sort_by(|a, b| a.in_use.cmp(&b.in_use).then_with(|| a.name.cmp(&b.name)));
    disks
}

/// The `/dev` paths of one disk's partitions, in order.
///
/// Public because clearing a disk has to clear them too: a ZFS label lives on
/// the partition, not on the disk, so wiping the table alone leaves labels
/// behind that the next `zpool create` will find and object to.
pub fn partition_paths(roots: &DeviceRoots, disk: &str) -> Vec<String> {
    partitions_of(&roots.sys_block.join(disk), disk)
        .into_iter()
        .map(|part| format!("/dev/{part}"))
        .collect()
}

/// The partitions of one disk: subdirectories carrying a `partition` file.
fn partitions_of(dir: &Path, name: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let child = entry.file_name().to_string_lossy().into_owned();
            (child.starts_with(name) && entry.path().join("partition").is_file()).then_some(child)
        })
        .collect();
    out.sort();
    out
}

/// Kernel name → what has it, for everything the node is currently using.
fn claims(roots: &DeviceRoots) -> HashMap<String, String> {
    let mut claims = HashMap::new();

    // Mounted filesystems. The device is the first field and the mount point
    // is the second; anything that is not a path under /dev is a virtual
    // filesystem and has no disk behind it.
    if let Ok(mounts) = std::fs::read_to_string(&roots.proc_mounts) {
        for line in mounts.lines() {
            let mut fields = line.split_whitespace();
            let (Some(source), Some(target)) = (fields.next(), fields.next()) else {
                continue;
            };
            if let Some(device) = kernel_name(source) {
                claims.insert(device, format!("mounted at {target}"));
            }
        }
    }

    // Swap. A disk holding swap is in use just as much as one holding a
    // filesystem, and it is easy to forget.
    if let Ok(swaps) = std::fs::read_to_string(&roots.proc_swaps) {
        for line in swaps.lines().skip(1) {
            if let Some(device) = line.split_whitespace().next().and_then(kernel_name) {
                claims.insert(device, "in use as swap".to_string());
            }
        }
    }

    claims
}

/// `/dev/sda3` → `sda3`. Anything that is not a device path is not a disk.
fn kernel_name(source: &str) -> Option<String> {
    source
        .strip_prefix("/dev/")
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .map(str::to_string)
}

/// Kernel name → the stable path that points at it.
///
/// `/dev/disk/by-id` holds several links per disk — by WWN, by model and
/// serial, sometimes by path. The model-and-serial one is the readable one, so
/// it wins; `wwn-` is the fallback, and `-part` links are skipped because they
/// point at partitions rather than at disks.
fn stable_paths(by_id: &Path) -> HashMap<String, String> {
    let Ok(entries) = std::fs::read_dir(by_id) else {
        return HashMap::new();
    };
    let mut best: HashMap<String, String> = HashMap::new();

    for entry in entries.flatten() {
        let link = entry.file_name().to_string_lossy().into_owned();
        if link.contains("-part") {
            continue;
        }
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let Some(device) = target.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let path = format!("/dev/disk/by-id/{link}");
        match best.get(&device) {
            // A name with a serial in it beats a bare WWN, which is a number
            // nobody can match to a disk in a rack.
            Some(existing) if !existing.contains("/wwn-") => {}
            _ => {
                best.insert(device, path);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A node with three disks: one the system is running from, one holding
    /// swap, and one that is genuinely free — plus the entries that are not
    /// disks at all.
    fn node(tag: &str) -> (DeviceRoots, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "lumen-zfs-devices-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        let roots = DeviceRoots::under(&root);
        fs::create_dir_all(&roots.sys_block).unwrap();
        fs::create_dir_all(root.join("proc")).unwrap();

        let disk = |name: &str, sectors: u64, rotational: &str, parts: &[&str]| {
            let dir = roots.sys_block.join(name);
            fs::create_dir_all(dir.join("queue")).unwrap();
            fs::create_dir_all(dir.join("device")).unwrap();
            fs::write(dir.join("size"), format!("{sectors}\n")).unwrap();
            fs::write(dir.join("removable"), "0\n").unwrap();
            fs::write(dir.join("queue/rotational"), format!("{rotational}\n")).unwrap();
            fs::write(dir.join("device/model"), "SAMSUNG MZ7L3\n").unwrap();
            for part in parts {
                let part_dir = dir.join(part);
                fs::create_dir_all(&part_dir).unwrap();
                fs::write(part_dir.join("partition"), "1\n").unwrap();
            }
        };

        // 1 TiB each.
        disk("sda", 2_147_483_648, "1", &["sda1", "sda2", "sda3"]);
        disk("sdb", 2_147_483_648, "0", &["sdb1"]);
        disk("nvme0n1", 2_147_483_648, "0", &[]);
        // Not disks: a loopback, a CD, and a ZFS volume belonging to a guest.
        disk("loop0", 1024, "0", &[]);
        disk("sr0", 1024, "1", &[]);
        disk("zd0", 2_147_483_648, "0", &[]);
        // A card reader with nothing in it.
        disk("sdz", 0, "1", &[]);

        fs::write(
            &roots.proc_mounts,
            "proc /proc proc rw 0 0\n\
             /dev/sda3 / xfs rw 0 0\n\
             /dev/sda1 /boot/efi vfat rw 0 0\n\
             tmpfs /run tmpfs rw 0 0\n",
        )
        .unwrap();
        fs::write(
            &roots.proc_swaps,
            "Filename\t\t\t\tType\t\tSize\tUsed\tPriority\n\
             /dev/sdb1                               partition\t8388604\t0\t-2\n",
        )
        .unwrap();

        (roots, root)
    }

    #[test]
    fn the_disk_the_appliance_is_running_from_is_reported_as_in_use() {
        let (roots, root) = node("in-use");
        let disks = scan(&roots);
        let find = |name: &str| disks.iter().find(|d| d.name == name).cloned();

        let sda = find("sda").expect("sda should be listed");
        assert!(sda.in_use, "the root disk must never look free");
        let used_by = sda.used_by.unwrap();
        assert!(used_by.contains("mounted at /"), "{used_by}");

        // Swap counts, and it is the one that is easy to forget.
        let sdb = find("sdb").expect("sdb should be listed");
        assert!(sdb.in_use);
        assert!(sdb.used_by.unwrap().contains("swap"));

        // And the genuinely empty one is offered without a warning on it.
        let nvme = find("nvme0n1").expect("nvme0n1 should be listed");
        assert!(!nvme.in_use);
        assert_eq!(nvme.used_by, None);
        assert!(!nvme.rotational);
        assert_eq!(nvme.size, 1024 * 1024 * 1024 * 1024);

        let _ = fs::remove_dir_all(root);
    }

    /// `zd*` is a guest's own disk. Offering one as a candidate for a new pool
    /// would destroy a virtual machine.
    #[test]
    fn things_that_are_not_disks_are_not_offered_at_all() {
        let (roots, root) = node("not-disks");
        let disks = scan(&roots);
        let names: Vec<&str> = disks.iter().map(|d| d.name.as_str()).collect();

        assert!(names.contains(&"sda"));
        assert!(names.contains(&"nvme0n1"));
        for never in ["loop0", "sr0", "zd0", "sdz"] {
            assert!(!names.contains(&never), "{never} must not be offered");
        }
        let _ = fs::remove_dir_all(root);
    }

    /// A disk with partitions and nothing mounted is still not empty, and
    /// saying "3 partitions" is the difference between an informed choice and
    /// a destroyed one.
    #[test]
    fn partitions_alone_are_enough_to_call_a_disk_spoken_for() {
        let (roots, root) = node("partitions");
        // Nothing mounted anywhere.
        fs::write(&roots.proc_mounts, "proc /proc proc rw 0 0\n").unwrap();
        fs::write(&roots.proc_swaps, "Filename\tType\tSize\tUsed\tPriority\n").unwrap();

        let disks = scan(&roots);
        let sda = disks.iter().find(|d| d.name == "sda").unwrap();
        assert!(sda.in_use);
        assert_eq!(sda.used_by.as_deref(), Some("3 partitions"));

        let nvme = disks.iter().find(|d| d.name == "nvme0n1").unwrap();
        assert!(!nvme.in_use, "no partitions and nothing mounted");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn free_disks_come_first_because_they_are_what_you_are_looking_for() {
        let (roots, root) = node("order");
        let disks = scan(&roots);
        assert_eq!(disks.first().unwrap().name, "nvme0n1");
        assert!(disks.iter().skip(1).all(|d| d.in_use));
        let _ = fs::remove_dir_all(root);
    }

    /// The path a pool is built on has to survive a reboot re-enumerating the
    /// disks, and `/dev/sdb` does not.
    #[test]
    fn a_pool_is_built_on_the_name_that_does_not_move() {
        let (roots, root) = node("by-id");
        fs::create_dir_all(&roots.dev_by_id).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let target = root.join("dev/nvme0n1");
            fs::create_dir_all(root.join("dev")).unwrap();
            fs::write(&target, "").unwrap();
            // Both links point at the same disk; the readable one wins.
            symlink(&target, roots.dev_by_id.join("wwn-0x5002538e40a1b2c3")).unwrap();
            symlink(
                &target,
                roots
                    .dev_by_id
                    .join("nvme-Samsung_SSD_990_PRO_2TB_S7DPNJ0X"),
            )
            .unwrap();
            // A partition link, which points at a partition and not a disk.
            symlink(&target, roots.dev_by_id.join("nvme-Samsung-part1")).unwrap();

            let disks = scan(&roots);
            let nvme = disks.iter().find(|d| d.name == "nvme0n1").unwrap();
            assert_eq!(
                nvme.path,
                "/dev/disk/by-id/nvme-Samsung_SSD_990_PRO_2TB_S7DPNJ0X"
            );
            assert_eq!(nvme.kernel_path, "/dev/nvme0n1");

            // A disk with no stable name still has to be offerable.
            let sda = disks.iter().find(|d| d.name == "sda").unwrap();
            assert_eq!(sda.path, "/dev/sda");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_node_that_reports_nothing_is_a_node_with_no_disks_rather_than_a_failure() {
        assert!(scan(&DeviceRoots::under("/nonexistent-lumen-test-root")).is_empty());
    }

    /// The other owner a data disk can have. A brick carries no partitions
    /// and appears in no /proc file, so without the probe it scans as the
    /// freest disk on the node — which is exactly how it would get eaten.
    #[test]
    fn a_lumenfs_brick_is_reported_as_owned_by_its_pool() {
        let (roots, root) = node("lumenfs-brick");
        fs::create_dir_all(&roots.dev).unwrap();
        let sb = lumen_fs::Superblock {
            block_size: 16 * 1024,
            segment_size: 1 << 20,
            segment_area_start: 1 << 20,
            segment_count: 7,
            generation: 1,
            wal_start: 16384,
            wal_size: 64 * 1024,
            pool_uuid: [0xAB; 16],
            brick_uuid: [0xCD; 16],
            tier: 1,
            wal_holder: false,
        };
        let mut head = vec![0u8; 4096];
        let encoded = sb.encode();
        head[..encoded.len()].copy_from_slice(&encoded);
        fs::write(roots.dev.join("nvme0n1"), &head).unwrap();

        let disks = scan(&roots);
        let nvme = disks.iter().find(|d| d.name == "nvme0n1").unwrap();
        assert!(nvme.in_use, "a brick's disk must never look free");
        assert!(nvme.claimed, "the scan cannot tell a serving brick apart");
        let used_by = nvme.used_by.clone().unwrap();
        assert!(
            used_by.contains("LumenFS brick (pool abababab, tier 1)"),
            "{used_by}"
        );
        let brick = nvme.lumenfs.clone().expect("the typed fact rides along");
        assert_eq!(brick.pool_uuid, "ab".repeat(16));
        assert_eq!(brick.brick_uuid, "cd".repeat(16));
        assert_eq!(brick.tier, 1);
        assert!(!brick.wal_holder);
        let _ = fs::remove_dir_all(root);
    }

    /// A brick of another format version is still a brick — spoken for by
    /// name, with no facts invented from a layout this release cannot read.
    #[test]
    fn a_brick_of_another_format_version_is_still_spoken_for() {
        let (roots, root) = node("lumenfs-foreign");
        fs::create_dir_all(&roots.dev).unwrap();
        let mut head = vec![0u8; 4096];
        head[0..8].copy_from_slice(b"LUMENFS\0");
        head[8..12].copy_from_slice(&1u32.to_le_bytes());
        fs::write(roots.dev.join("nvme0n1"), &head).unwrap();

        let disks = scan(&roots);
        let nvme = disks.iter().find(|d| d.name == "nvme0n1").unwrap();
        assert!(nvme.in_use && nvme.claimed);
        let used_by = nvme.used_by.clone().unwrap();
        assert!(
            used_by.contains("LumenFS brick (format v1, not this release's)"),
            "{used_by}"
        );
        assert_eq!(nvme.lumenfs, None);
        let _ = fs::remove_dir_all(root);
    }

    /// Anything short, unreadable, or plainly not a superblock leaves the
    /// disk exactly as the rest of the scan judged it.
    #[test]
    fn a_disk_with_no_brick_on_it_is_untouched_by_the_probe() {
        let (roots, root) = node("lumenfs-none");
        fs::create_dir_all(&roots.dev).unwrap();
        // A first sector full of filesystem, not brick.
        fs::write(roots.dev.join("nvme0n1"), vec![0x42u8; 4096]).unwrap();
        // And a device whose read comes up short.
        fs::write(roots.dev.join("sdb"), b"tiny").unwrap();

        let disks = scan(&roots);
        let nvme = disks.iter().find(|d| d.name == "nvme0n1").unwrap();
        assert!(!nvme.in_use);
        assert_eq!(nvme.lumenfs, None);
        let sdb = disks.iter().find(|d| d.name == "sdb").unwrap();
        assert_eq!(sdb.lumenfs, None);
        let _ = fs::remove_dir_all(root);
    }
}
