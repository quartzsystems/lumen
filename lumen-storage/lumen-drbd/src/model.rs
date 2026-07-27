//! What a replicated volume is made of: names, sizes, ports, and minors.
//!
//! Pure data and pure arithmetic — nothing here talks to DRBD. The stored
//! shape (`VolumeRecord`) lives in `lumen-cluster`, riding the membership
//! record; this module is everything computed *about* one: the byte-exact
//! backing size, the resource and zvol names, and the port and minor
//! allocation against a cluster's existing volumes.

use lumen_cluster::VolumeRecord;

/// The TCP port range replication rides on the Core network, matching the
/// firewalld service the packaging opens there. Twelve ports is twelve
/// volumes per cluster — a v1 ceiling, documented, not an accident.
pub const DRBD_PORT_MIN: u16 = 7788;
pub const DRBD_PORT_MAX: u16 = 7799;

/// Replica bounds. Two is the floor everywhere; three exists only in the
/// quorum regime, where a majority of replicas means something.
pub const MIN_REPLICAS: usize = 2;
pub const MAX_REPLICAS: usize = 3;

/// The volblocksize every backing zvol is created with — the same 16 KiB the
/// compute domain already uses for plain VM disks, so a replicated disk and
/// a local disk behave identically underneath the guest.
pub const VOLBLOCKSIZE: u64 = 16 * 1024;

/// A usable volume name: the same hostname-label shape as a cluster name,
/// because the two are joined into a DRBD resource name and a zvol path.
pub fn valid_volume_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The DRBD resource name: `<cluster>-<volume>`. The cluster prefix is the
/// namespace rule that lets two clusters share a management network without
/// their resources colliding in anyone's tooling or logs.
pub fn resource_name(cluster: &str, volume: &str) -> String {
    format!("{cluster}-{volume}")
}

/// The backing zvol's dataset path on one member: inside the pool's lumen
/// namespace — so `lumen-zfs` will manage and destroy it — under the same
/// cluster-prefixed name as the resource.
pub fn zvol_path(pool: &str, cluster: &str, volume: &str) -> String {
    format!("{pool}/lumen/{}", resource_name(cluster, volume))
}

/// The block device a replicated volume appears as. Stable across every
/// member, which is what makes it a value a domain document can carry.
pub fn device_path(minor: u32) -> String {
    format!("/dev/drbd{minor}")
}

/// DRBD 9 internal metadata for a backing device of `gross` bytes with
/// `peers` peer slots, per the documented formula: the bitmap covers 4 KiB
/// of data per bit per peer, held in 8-sector chunks, plus 72 sectors of
/// superblock and activity log.
fn metadata_bytes(gross: u64, peers: u64) -> u64 {
    let sectors = gross.div_ceil(512);
    let bitmap_sectors = sectors.div_ceil(1 << 18) * 8 * peers;
    (bitmap_sectors + 72) * 512
}

fn round_up(value: u64, multiple: u64) -> u64 {
    value.div_ceil(multiple) * multiple
}

/// The byte-exact backing zvol size for a volume the operator wants `usable`
/// bytes of: usable data plus internal metadata, rounded up to the
/// volblocksize. Computed once, before anything is created on any node —
/// every replica gets this exact number, which is what makes "the backing
/// devices are identical" true by construction rather than by resync.
///
/// The metadata describes the gross device, which includes the metadata, so
/// the answer is a fixed point — reached in one or two rounds, because the
/// bitmap grows five orders of magnitude slower than the data it covers.
pub fn backing_size(usable: u64, replicas: usize) -> u64 {
    let peers = replicas.saturating_sub(1) as u64;
    let mut gross = round_up(usable + metadata_bytes(usable, peers), VOLBLOCKSIZE);
    loop {
        let need = round_up(usable + metadata_bytes(gross, peers), VOLBLOCKSIZE);
        if need == gross {
            return gross;
        }
        gross = need;
    }
}

/// The first replication port not taken by an existing volume, or `None`
/// when the cluster's range is spent.
pub fn allocate_port(existing: &[VolumeRecord]) -> Option<u16> {
    (DRBD_PORT_MIN..=DRBD_PORT_MAX).find(|port| !existing.iter().any(|v| v.port == *port))
}

/// The first free device minor, starting at 1. Per cluster is enough: a node
/// is in at most one cluster, so two clusters' minors never meet on a node.
pub fn allocate_minor(existing: &[VolumeRecord]) -> u32 {
    (1..)
        .find(|minor| !existing.iter().any(|v| v.minor == *minor))
        .expect("u32 minors do not run out")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_cluster::{VolumeRecord, VolumeSeat};

    fn record(name: &str, port: u16, minor: u32) -> VolumeRecord {
        VolumeRecord {
            name: name.into(),
            size_bytes: 1 << 30,
            zvol_bytes: 1 << 30,
            port,
            minor,
            replicas: vec![VolumeSeat {
                node: "alpha-1".into(),
                pool: "tank".into(),
            }],
        }
    }

    /// The sizing invariants, held across a spread of sizes and both replica
    /// counts: the backing device is a volblocksize multiple, it is strictly
    /// larger than the data by at least the metadata it must hold, and the
    /// usable portion never shrinks below what was asked for.
    #[test]
    fn the_backing_size_carries_the_data_the_metadata_and_nothing_less() {
        let sizes = [
            1u64 << 20,
            (1 << 30) - 1,
            1 << 30,
            10 << 30,
            (100 << 30) + 4096,
            1 << 40,
        ];
        for &usable in &sizes {
            for replicas in MIN_REPLICAS..=MAX_REPLICAS {
                let gross = backing_size(usable, replicas);
                assert_eq!(gross % VOLBLOCKSIZE, 0, "usable={usable} r={replicas}");
                let peers = (replicas - 1) as u64;
                assert!(
                    gross >= usable + metadata_bytes(gross, peers),
                    "usable={usable} r={replicas}: {gross} cannot hold data plus metadata"
                );
                // Not absurdly padded either: within one block of tight.
                assert!(
                    gross < usable + metadata_bytes(gross, peers) + VOLBLOCKSIZE,
                    "usable={usable} r={replicas}: {gross} over-padded"
                );
            }
        }
    }

    /// A third replica needs a second peer's bitmap, so it costs more
    /// metadata — never less.
    #[test]
    fn more_replicas_never_need_a_smaller_backing_device() {
        for shift in [20u32, 30, 33, 37] {
            let usable = 1u64 << shift;
            assert!(backing_size(usable, 3) >= backing_size(usable, 2));
        }
    }

    #[test]
    fn ports_allocate_from_the_range_and_exhaust_honestly() {
        assert_eq!(allocate_port(&[]), Some(DRBD_PORT_MIN));
        let taken: Vec<VolumeRecord> = (DRBD_PORT_MIN..=DRBD_PORT_MAX)
            .enumerate()
            .map(|(i, port)| record(&format!("v{i}"), port, i as u32 + 1))
            .collect();
        assert_eq!(allocate_port(&taken), None, "the range is twelve wide");
        // A hole in the middle is reused before anything else.
        let mut sparse = taken;
        sparse.remove(3);
        assert_eq!(allocate_port(&sparse), Some(DRBD_PORT_MIN + 3));
    }

    #[test]
    fn minors_start_at_one_and_fill_holes() {
        assert_eq!(allocate_minor(&[]), 1);
        let existing = vec![record("a", 7788, 1), record("b", 7789, 3)];
        assert_eq!(allocate_minor(&existing), 2);
    }

    #[test]
    fn names_and_paths_carry_the_cluster_prefix() {
        assert!(valid_volume_name("vm-101-disk-0"));
        assert!(!valid_volume_name("VM"));
        assert!(!valid_volume_name("9lives"));
        assert!(!valid_volume_name(""));
        assert!(!valid_volume_name("trailing-"));
        assert_eq!(
            resource_name("alpha", "vm-101-disk-0"),
            "alpha-vm-101-disk-0"
        );
        assert_eq!(
            zvol_path("tank", "alpha", "vm-101-disk-0"),
            "tank/lumen/alpha-vm-101-disk-0"
        );
        assert_eq!(device_path(4), "/dev/drbd4");
    }
}
