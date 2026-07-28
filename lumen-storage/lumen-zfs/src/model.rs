//! What a pool, a dataset, and a volume are, plus the one namespace rule the
//! whole crate is built around.
//!
//! Read-only types this stage, with a single exception: the volumes a virtual
//! machine's disks live on. Those are created and destroyed, and every one of
//! them lives under `<pool>/lumen/`. Nothing outside that prefix is
//! destroyable by anything in this crate — see [`is_lumen_volume`].

use serde::{Deserialize, Serialize};

/// The one directory Lumen owns inside a pool. A dataset outside it belongs to
/// the operator and is never written or removed here.
pub const LUMEN_PREFIX: &str = "lumen";

/// The leaf name of a pool's installation-media library: `<pool>/lumen/iso`.
///
/// Reserved. It is the one dataset under the Lumen prefix that is a filesystem
/// rather than a machine's disk, so [`is_lumen_volume`] deliberately still
/// matches it — and [`is_reserved_leaf`] is what keeps a volume destroy from
/// taking the whole library with it.
pub const ISO_LEAF: &str = "iso";

/// Where every pool's media library is mounted, one directory per pool.
///
/// A fixed path rather than each dataset's natural one because the control
/// plane's unit has to name it: `ProtectSystem=strict` makes the whole
/// hierarchy read-only, and a `ReadWritePaths=` line cannot enumerate pools
/// that do not exist yet. One parent covers every pool the node will ever have.
pub const ISO_MOUNT_ROOT: &str = "/var/lib/lumen/iso";

/// Health as `zpool list` reports it. `Unknown` covers a state a future
/// release adds rather than making an unfamiliar word fatal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolHealth {
    Online,
    Degraded,
    Faulted,
    Offline,
    Removed,
    Unavail,
    #[default]
    Unknown,
}

impl PoolHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            PoolHealth::Online => "online",
            PoolHealth::Degraded => "degraded",
            PoolHealth::Faulted => "faulted",
            PoolHealth::Offline => "offline",
            PoolHealth::Removed => "removed",
            PoolHealth::Unavail => "unavail",
            PoolHealth::Unknown => "unknown",
        }
    }

    /// Parse the word the tool prints, in any case.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_uppercase().as_str() {
            "ONLINE" => PoolHealth::Online,
            "DEGRADED" => PoolHealth::Degraded,
            "FAULTED" => PoolHealth::Faulted,
            "OFFLINE" => PoolHealth::Offline,
            "REMOVED" => PoolHealth::Removed,
            "UNAVAIL" => PoolHealth::Unavail,
            _ => PoolHealth::Unknown,
        }
    }

    /// Anything an operator should be looking at right now.
    pub fn needs_attention(self) -> bool {
        !matches!(self, PoolHealth::Online)
    }
}

/// One pool as the box reports it. Sizes are bytes, always — the console does
/// its own formatting, and a number that has already been rounded to "1.8T"
/// cannot be added up.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pool {
    pub name: String,
    pub health: PoolHealth,
    pub size: u64,
    pub allocated: u64,
    pub free: u64,
    /// Percent, as reported. Absent on a pool that has none to report yet.
    pub fragmentation: Option<u8>,
    /// Deduplication ratio, e.g. 1.0 for a pool that is not deduplicating.
    pub dedup_ratio: Option<f64>,
    pub read_only: bool,
}

impl Pool {
    /// Allocated as a percentage of the pool, rounded. A pool reporting a zero
    /// size is not full, it is unreadable — so it reads as zero.
    pub fn used_percent(&self) -> u8 {
        if self.size == 0 {
            return 0;
        }
        let percent = (self.allocated as f64 / self.size as f64) * 100.0;
        percent.round().clamp(0.0, 100.0) as u8
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatasetKind {
    #[default]
    Filesystem,
    Volume,
    Snapshot,
}

impl DatasetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DatasetKind::Filesystem => "filesystem",
            DatasetKind::Volume => "volume",
            DatasetKind::Snapshot => "snapshot",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "volume" => DatasetKind::Volume,
            "snapshot" => DatasetKind::Snapshot,
            _ => DatasetKind::Filesystem,
        }
    }
}

/// One snapshot of a volume, as `zfs list -t snapshot` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    /// The snapshot's own name — the part after the `@`.
    pub name: String,
    /// Bytes the snapshot holds that the live volume no longer does.
    pub used: u64,
    /// Unix seconds.
    pub created: u64,
}

/// One dataset or volume under a pool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dataset {
    /// Full path, e.g. `boot/lumen/vm-101-disk-0`.
    pub name: String,
    pub kind: DatasetKind,
    pub used: u64,
    /// Space still available to this dataset. Absent where the tool reports
    /// none (a snapshot).
    pub available: Option<u64>,
    pub referenced: u64,
    /// Logical size of a volume. `None` for a filesystem.
    pub volsize: Option<u64>,
    pub volblocksize: Option<u64>,
    /// `None` for a volume, and for a filesystem that is not mounted.
    pub mountpoint: Option<String>,
}

impl Dataset {
    /// The pool a dataset belongs to: everything before the first `/`.
    pub fn pool(&self) -> &str {
        self.name.split('/').next().unwrap_or(&self.name)
    }

    /// Created by Lumen, and therefore something Lumen may remove.
    pub fn is_lumen_managed(&self) -> bool {
        is_lumen_volume(&self.name)
    }
}

/// How a pool's disks are arranged, and therefore what it survives.
///
/// The five arrangements an operator actually chooses between. Anything
/// further — several vdevs of different shapes, a separate log or cache device,
/// a spare — is a pool built at the command line by somebody who knows exactly
/// what they are doing, and this console does not pretend to offer it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VdevKind {
    /// Every disk's capacity, and no redundancy at all: one disk failing takes
    /// the pool with it.
    #[default]
    Stripe,
    /// Every disk holds the same data. Survives all but one disk failing.
    Mirror,
    /// One disk of parity.
    Raidz1,
    /// Two disks of parity.
    Raidz2,
    /// Three disks of parity.
    Raidz3,
}

impl VdevKind {
    /// The word `zpool create` wants before the disk list. A stripe has none —
    /// bare disks *are* the stripe — which is exactly why it is the dangerous
    /// default and why the console says so out loud.
    pub fn keyword(self) -> Option<&'static str> {
        match self {
            VdevKind::Stripe => None,
            VdevKind::Mirror => Some("mirror"),
            VdevKind::Raidz1 => Some("raidz1"),
            VdevKind::Raidz2 => Some("raidz2"),
            VdevKind::Raidz3 => Some("raidz3"),
        }
    }

    /// The fewest disks this arrangement is meaningful with.
    ///
    /// `zpool` would accept a one-disk mirror or a two-disk raidz1; both are
    /// legal and neither is what anybody meant.
    pub fn min_disks(self) -> usize {
        match self {
            VdevKind::Stripe => 1,
            VdevKind::Mirror => 2,
            VdevKind::Raidz1 => 3,
            VdevKind::Raidz2 => 4,
            VdevKind::Raidz3 => 5,
        }
    }

    /// How many disks may fail before the data is gone.
    pub fn parity(self) -> usize {
        match self {
            VdevKind::Stripe => 0,
            VdevKind::Mirror => 1,
            VdevKind::Raidz1 => 1,
            VdevKind::Raidz2 => 2,
            VdevKind::Raidz3 => 3,
        }
    }

    /// Bytes of the total this arrangement leaves for data, before ZFS's own
    /// overhead. Advisory: the console shows it so a choice can be compared,
    /// and the pool reports the real figure once it exists.
    pub fn usable_bytes(self, disks: usize, smallest: u64) -> u64 {
        if disks == 0 {
            return 0;
        }
        match self {
            // A mirror is as big as one disk, however many there are.
            VdevKind::Mirror => smallest,
            // Everything else is every disk, less the parity ones. A stripe
            // has no parity, so this is all of them.
            kind => smallest.saturating_mul(disks.saturating_sub(kind.parity()) as u64),
        }
    }
}

/// One disk the node has, as a candidate for a pool.
///
/// The whole point of this type is [`BlockDevice::in_use`]: creating a pool on
/// a disk that already holds something destroys it, and the single most useful
/// thing this console can do is refuse to offer the one the operating system is
/// running from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDevice {
    /// The kernel name — `sda`, `nvme0n1`.
    pub name: String,
    /// The stable path a pool should be built on: `/dev/disk/by-id/…` when the
    /// node has one, and `/dev/<name>` only when it does not.
    ///
    /// This matters more than it looks. `/dev/sdb` is whatever the kernel
    /// enumerated second this boot; a pool built on it can come back after a
    /// reboot pointing at a different disk. The by-id path is the serial
    /// number and does not move.
    pub path: String,
    /// `/dev/<name>`, for showing next to the stable path.
    pub kernel_path: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// Spinning rust rather than solid state.
    pub rotational: bool,
    pub removable: bool,
    /// Something is already on it. Never offered without saying what.
    pub in_use: bool,
    /// What is on it, in words: "mounted at /", "in pool boot", "has 3
    /// partitions". Absent when the disk is genuinely empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_by: Option<String>,
    /// How many partitions it carries.
    ///
    /// Split out from `in_use` because the two halves lead to different
    /// offers. A disk that is spoken for only by a partition table is one an
    /// operator can reclaim; a disk something is actively using is not.
    #[serde(default)]
    pub partitions: usize,
    /// A mount or swap has this disk, or one of its partitions, open right
    /// now. The half of `in_use` nothing may override — clearing a partition
    /// table out from under a mounted filesystem takes the node with it.
    #[serde(default)]
    pub claimed: bool,
    /// The console may offer to clear this disk.
    ///
    /// Filled in by the service, never by the `/sys` scan, and that is the
    /// point: an imported ZFS pool does not appear in `/proc/mounts` as the
    /// disk it was built on — its members carry partitions and nothing else —
    /// so a live pool member looks exactly like a reclaimable disk to the
    /// scan. Only the layer that can also ask `zpool` may answer this.
    #[serde(default)]
    pub wipeable: bool,
}

impl BlockDevice {
    /// What the node's own `/sys` view says about clearing this disk: there
    /// is something to clear, and nothing live is using it.
    ///
    /// Deliberately not the final answer — see [`BlockDevice::wipeable`],
    /// which is what the console reads.
    pub fn looks_wipeable(&self) -> bool {
        !self.claimed && self.partitions > 0
    }
}

/// A pool to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolRequest {
    pub name: String,
    pub vdev: VdevKind,
    /// Absolute device paths, already resolved to the stable ones.
    pub disks: Vec<String>,
    /// `ashift`. `None` lets ZFS detect it, which is right for a disk that
    /// reports its physical block size honestly and wrong for the many that do
    /// not — hence [`DEFAULT_ASHIFT`].
    pub ashift: Option<u8>,
    pub compression: Compression,
    pub autotrim: bool,
    /// Build on a disk that already has something on it. Always paired with the
    /// acknowledgement; see the service.
    pub force: bool,
}

/// 4 KiB sectors, which is what every disk made this decade actually has.
///
/// Not detected: a great many drives still report 512-byte sectors for
/// compatibility, and a pool built at `ashift=9` on one of them is slow
/// forever — the value cannot be changed after creation. 12 costs a little
/// space on a genuinely-512-byte disk and is the number every ZFS guide has
/// recommended for a decade. The installer uses it too.
pub const DEFAULT_ASHIFT: u8 = 12;

/// The compression a new pool is created with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    Off,
    /// Fast enough to be free on any processor this appliance runs on, and it
    /// is what the installer sets on the root pool.
    #[default]
    Lz4,
    /// Better ratios, more processor. A sensible choice for bulk storage.
    Zstd,
}

impl Compression {
    pub fn as_str(self) -> &'static str {
        match self {
            Compression::Off => "off",
            Compression::Lz4 => "lz4",
            Compression::Zstd => "zstd",
        }
    }
}

/// What to create. A typed request rather than a string, so the backend builds
/// its argument array from values and never from a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeRequest {
    /// Full dataset path, always under `<pool>/lumen/`.
    pub path: String,
    /// Logical size in bytes.
    pub size: u64,
    /// Volume block size in bytes. `None` leaves the pool default.
    pub blocksize: Option<u64>,
}

/// The dataset a virtual machine's disk lives on: `<pool>/lumen/vm-<vmid>-disk-<n>`.
///
/// Numeric and boring on purpose. Every disk Lumen creates is findable from
/// the VM it belongs to without consulting anything but the name, and the
/// whole set is removable by prefix if a machine ever has to be cleaned up by
/// hand.
pub fn vm_disk_path(pool: &str, vmid: u32, index: u32) -> String {
    format!("{pool}/{LUMEN_PREFIX}/vm-{vmid}-disk-{index}")
}

/// The block device a volume appears as once it exists.
pub fn device_path(dataset: &str) -> String {
    format!("/dev/zvol/{dataset}")
}

/// The parent dataset Lumen's volumes hang off, created on demand.
pub fn lumen_root(pool: &str) -> String {
    format!("{pool}/{LUMEN_PREFIX}")
}

/// A pool's media library dataset: `<pool>/lumen/iso`.
pub fn iso_dataset(pool: &str) -> String {
    format!("{pool}/{LUMEN_PREFIX}/{ISO_LEAF}")
}

/// Where that dataset is mounted: `/var/lib/lumen/iso/<pool>`.
pub fn iso_mountpoint(pool: &str) -> String {
    format!("{ISO_MOUNT_ROOT}/{pool}")
}

/// A leaf under `<pool>/lumen/` that is Lumen's own furniture rather than a
/// machine's disk. Creating and destroying volumes must step around these.
pub fn is_reserved_leaf(path: &str) -> bool {
    path.split('/').nth(2) == Some(ISO_LEAF) && path.split('/').count() == 3
}

/// A usable name for a file in the media library.
///
/// This is the guard between an operator-supplied name and a path handed to
/// the filesystem: one path component, no traversal, no separators, no control
/// characters, and an `.iso` suffix so the library cannot be used as a general
/// file drop.
pub fn valid_iso_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !name.is_empty()
        && name.len() <= 255
        && lower.ends_with(".iso")
        && lower.len() > 4
        && !name.starts_with('.')
        && !name.starts_with('-')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name
            .chars()
            .any(|c| c.is_control() || c == '\0' || c.is_whitespace() && c != ' ')
}

/// Exactly `<pool>/lumen/<name>`: one pool component, the Lumen prefix, one
/// leaf. This is the guard that makes `destroy_volume` safe to expose — a
/// request naming `boot`, `boot/lumen`, `boot/data/important`, or anything
/// with a traversal component in it is not a Lumen volume and is refused.
pub fn is_lumen_volume(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() != 3 {
        return false;
    }
    let (pool, prefix, leaf) = (parts[0], parts[1], parts[2]);
    prefix == LUMEN_PREFIX
        && !pool.is_empty()
        && !leaf.is_empty()
        && [pool, leaf].iter().all(|part| {
            *part != "."
                && *part != ".."
                // A component that starts with "-" would be read as a flag by
                // the tool the path is handed to, whatever quoting is used.
                && !part.starts_with('-')
                && !part
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || c == '@' || c == '%')
        })
}

/// A legal pool name, checked before it is ever handed to a command. Pool
/// names are operator-supplied through a path parameter, so this is the line
/// between "names a pool" and "names something else entirely".
pub fn valid_pool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == ':' || c == ' '
        })
        && !name.contains('/')
}

/// Names `zpool` itself reserves, plus the ones that would be ambiguous in the
/// command that creates the pool.
///
/// `mirror`, `raidz*`, `draid*`, `spare`, `log`, `cache`, and `special` are all
/// vdev keywords: a pool called `mirror` produces a command line where the pool
/// name and a vdev type are the same word. `zpool` refuses them and so does
/// this, before the command is ever built.
const RESERVED_POOL_NAMES: [&str; 12] = [
    "mirror", "raidz", "raidz1", "raidz2", "raidz3", "draid", "draid1", "draid2", "draid3",
    "spare", "log", "cache",
];

/// A name for a pool this appliance is about to **create**.
///
/// Stricter than [`valid_pool_name`], deliberately. That one is the guard on a
/// name arriving in a URL for a pool that already exists, and it has to accept
/// whatever an operator built at the command line years ago. This one is a
/// choice being made now, so it can insist on the subset that will not cause
/// trouble later: a leading letter, and nothing that reads as a vdev keyword.
pub fn valid_new_pool_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 64
        && first.is_ascii_alphabetic()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == ':')
        && !RESERVED_POOL_NAMES.contains(&name.to_ascii_lowercase().as_str())
        // `zpool` reads a leading c[0-9] as a Solaris controller name.
        && !(first == 'c' && name.chars().nth(1).is_some_and(|c| c.is_ascii_digit()))
}

/// A device path this appliance will hand to `zpool create`.
///
/// Absolute, under `/dev`, one clean path with no traversal and nothing that
/// could be read as a flag. The operator picks from a list the node produced,
/// so this is defence against a request that did not come from that list rather
/// than against a typo.
pub fn valid_device_path(path: &str) -> bool {
    path.starts_with("/dev/")
        && path.len() <= 255
        && !path.contains("..")
        && !path.ends_with('/')
        && !path
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\0')
        && path
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && !part.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_disk_lands_under_the_lumen_prefix() {
        assert_eq!(vm_disk_path("boot", 101, 0), "boot/lumen/vm-101-disk-0");
        assert!(is_lumen_volume(&vm_disk_path("boot", 101, 0)));
        assert_eq!(
            device_path("boot/lumen/vm-101-disk-0"),
            "/dev/zvol/boot/lumen/vm-101-disk-0"
        );
        assert_eq!(lumen_root("boot"), "boot/lumen");
    }

    /// The guard that stands between a destroy request and the operator's own
    /// data. Everything that is not exactly `<pool>/lumen/<leaf>` is refused.
    #[test]
    fn only_a_lumen_volume_is_destroyable() {
        for allowed in [
            "boot/lumen/vm-101-disk-0",
            "tank/lumen/vm-100-disk-3",
            "boot/lumen/anything",
        ] {
            assert!(is_lumen_volume(allowed), "{allowed} should be destroyable");
        }
        for refused in [
            "boot",
            "boot/lumen",
            "boot/data",
            "boot/data/important",
            "boot/lumen/vm-101-disk-0/child",
            "boot/lumen/../data",
            "boot/../tank/lumen/x",
            "boot/lumen/vm-101-disk-0@snapshot",
            "boot/lumen/",
            "/boot/lumen/x",
            "",
            "boot/notlumen/x",
            "-rf/lumen/x",
            "boot/lumen/-rf",
        ] {
            assert!(
                !is_lumen_volume(refused),
                "{refused:?} must NOT be destroyable"
            );
        }
    }

    /// The media library sits under the same prefix as the machine disks, so
    /// the destroy guard has to know it apart from one — otherwise removing a
    /// disk could name the library and take every ISO on the node with it.
    #[test]
    fn the_media_library_is_reserved_furniture_not_a_volume() {
        assert_eq!(iso_dataset("boot"), "boot/lumen/iso");
        assert_eq!(iso_mountpoint("boot"), "/var/lib/lumen/iso/boot");
        assert!(is_lumen_volume("boot/lumen/iso"), "shape-wise it matches");
        assert!(is_reserved_leaf("boot/lumen/iso"));
        assert!(!is_reserved_leaf("boot/lumen/vm-100-disk-0"));
        assert!(!is_reserved_leaf("boot/lumen/iso/nested"));
        assert!(!is_reserved_leaf("boot/iso"));
    }

    #[test]
    fn an_iso_name_is_one_component_and_says_what_it_is() {
        for allowed in [
            "almalinux-10.iso",
            "Windows Server 2025.iso",
            "virtio-win-0.1.271.ISO",
        ] {
            assert!(valid_iso_name(allowed), "{allowed}");
        }
        for refused in [
            "",
            ".iso",
            "notanimage.img",
            "../../etc/passwd.iso",
            "sub/dir.iso",
            "sub\\dir.iso",
            ".hidden.iso",
            "-rf.iso",
            "line\nbreak.iso",
        ] {
            assert!(!valid_iso_name(refused), "{refused:?} must be refused");
        }
    }

    #[test]
    fn pool_names_that_are_really_something_else_are_refused() {
        assert!(valid_pool_name("boot"));
        assert!(valid_pool_name("tank-1"));
        assert!(!valid_pool_name(""));
        assert!(!valid_pool_name(".."));
        assert!(!valid_pool_name("boot/lumen"));
        assert!(!valid_pool_name("-rf"), "must not look like a flag");
        assert!(!valid_pool_name("a\nb"));
        assert!(!valid_pool_name("$(reboot)"));
    }

    #[test]
    fn used_percent_never_divides_by_zero() {
        let empty = Pool::default();
        assert_eq!(empty.used_percent(), 0);
        let half = Pool {
            size: 1000,
            allocated: 500,
            ..Pool::default()
        };
        assert_eq!(half.used_percent(), 50);
    }

    #[test]
    fn health_round_trips_through_the_word_the_tool_prints() {
        assert_eq!(PoolHealth::parse("ONLINE"), PoolHealth::Online);
        assert_eq!(PoolHealth::parse("degraded"), PoolHealth::Degraded);
        assert_eq!(PoolHealth::parse("something-new"), PoolHealth::Unknown);
        assert!(PoolHealth::Degraded.needs_attention());
        assert!(!PoolHealth::Online.needs_attention());
    }

    /// A name being chosen now can insist on the subset that will not cause
    /// trouble, which the guard on an existing pool's name cannot.
    #[test]
    fn a_new_pool_name_may_not_be_a_word_the_command_already_uses() {
        for allowed in ["tank", "boot", "vmdata", "pool-1", "a.b:c"] {
            assert!(valid_new_pool_name(allowed), "{allowed}");
        }
        for refused in [
            "",
            "mirror",
            "RAIDZ2",
            "spare",
            "log",
            "cache",
            "1tank",
            "c0t0d0",
            "-rf",
            "has space",
            "has/slash",
            "$(reboot)",
            &"x".repeat(65),
        ] {
            assert!(!valid_new_pool_name(refused), "{refused:?} must be refused");
        }
        // The looser guard still accepts a pool somebody made years ago.
        assert!(valid_pool_name("my pool"));
        assert!(!valid_new_pool_name("my pool"));
    }

    #[test]
    fn a_device_is_an_absolute_path_under_dev_and_nothing_else() {
        for allowed in [
            "/dev/sda",
            "/dev/nvme0n1",
            "/dev/disk/by-id/nvme-Samsung_SSD_990_PRO_2TB_S7DPNJ0X",
        ] {
            assert!(valid_device_path(allowed), "{allowed}");
        }
        for refused in [
            "",
            "sda",
            "/etc/passwd",
            "/dev/../etc/passwd",
            "/dev/",
            "/dev/-rf",
            "/dev/sd a",
            "/dev//sda",
        ] {
            assert!(!valid_device_path(refused), "{refused:?} must be refused");
        }
    }

    /// The number an operator compares arrangements by, and the one they are
    /// most likely to get wrong in their head.
    #[test]
    fn an_arrangement_says_what_it_costs_and_what_it_survives() {
        const TB: u64 = 1_000_000_000_000;

        // Four 1 TB disks, four ways.
        assert_eq!(VdevKind::Stripe.usable_bytes(4, TB), 4 * TB);
        assert_eq!(VdevKind::Stripe.parity(), 0);
        assert_eq!(VdevKind::Mirror.usable_bytes(4, TB), TB, "however many");
        assert_eq!(VdevKind::Raidz1.usable_bytes(4, TB), 3 * TB);
        assert_eq!(VdevKind::Raidz2.usable_bytes(4, TB), 2 * TB);

        // The floors are the point where the arrangement stops meaning
        // anything, not the point where zpool refuses.
        assert_eq!(VdevKind::Stripe.min_disks(), 1);
        assert_eq!(VdevKind::Mirror.min_disks(), 2);
        assert_eq!(VdevKind::Raidz1.min_disks(), 3);
        assert_eq!(VdevKind::Raidz3.min_disks(), 5);

        // A stripe is bare disks, which is exactly why it is the risky one.
        assert_eq!(VdevKind::Stripe.keyword(), None);
        assert_eq!(VdevKind::Raidz2.keyword(), Some("raidz2"));

        // No disks is no space, rather than an underflow into an enormous
        // number.
        assert_eq!(VdevKind::Raidz2.usable_bytes(0, TB), 0);
        assert_eq!(VdevKind::Raidz2.usable_bytes(1, TB), 0);
    }

    #[test]
    fn a_dataset_knows_its_pool_and_whether_lumen_made_it() {
        let ours = Dataset {
            name: "boot/lumen/vm-101-disk-0".into(),
            kind: DatasetKind::Volume,
            ..Dataset::default()
        };
        assert_eq!(ours.pool(), "boot");
        assert!(ours.is_lumen_managed());

        let theirs = Dataset {
            name: "boot/data".into(),
            ..Dataset::default()
        };
        assert_eq!(theirs.pool(), "boot");
        assert!(!theirs.is_lumen_managed());
    }
}
