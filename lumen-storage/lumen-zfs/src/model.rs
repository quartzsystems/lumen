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

/// One dataset or volume under a pool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dataset {
    /// Full path, e.g. `rpool/lumen/vm-101-disk-0`.
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

/// Exactly `<pool>/lumen/<name>`: one pool component, the Lumen prefix, one
/// leaf. This is the guard that makes `destroy_volume` safe to expose — a
/// request naming `rpool`, `rpool/lumen`, `rpool/data/important`, or anything
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_disk_lands_under_the_lumen_prefix() {
        assert_eq!(vm_disk_path("rpool", 101, 0), "rpool/lumen/vm-101-disk-0");
        assert!(is_lumen_volume(&vm_disk_path("rpool", 101, 0)));
        assert_eq!(
            device_path("rpool/lumen/vm-101-disk-0"),
            "/dev/zvol/rpool/lumen/vm-101-disk-0"
        );
        assert_eq!(lumen_root("rpool"), "rpool/lumen");
    }

    /// The guard that stands between a destroy request and the operator's own
    /// data. Everything that is not exactly `<pool>/lumen/<leaf>` is refused.
    #[test]
    fn only_a_lumen_volume_is_destroyable() {
        for allowed in [
            "rpool/lumen/vm-101-disk-0",
            "tank/lumen/vm-100-disk-3",
            "rpool/lumen/anything",
        ] {
            assert!(is_lumen_volume(allowed), "{allowed} should be destroyable");
        }
        for refused in [
            "rpool",
            "rpool/lumen",
            "rpool/data",
            "rpool/data/important",
            "rpool/lumen/vm-101-disk-0/child",
            "rpool/lumen/../data",
            "rpool/../tank/lumen/x",
            "rpool/lumen/vm-101-disk-0@snapshot",
            "rpool/lumen/",
            "/rpool/lumen/x",
            "",
            "rpool/notlumen/x",
            "-rf/lumen/x",
            "rpool/lumen/-rf",
        ] {
            assert!(
                !is_lumen_volume(refused),
                "{refused:?} must NOT be destroyable"
            );
        }
    }

    #[test]
    fn pool_names_that_are_really_something_else_are_refused() {
        assert!(valid_pool_name("rpool"));
        assert!(valid_pool_name("tank-1"));
        assert!(!valid_pool_name(""));
        assert!(!valid_pool_name(".."));
        assert!(!valid_pool_name("rpool/lumen"));
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

    #[test]
    fn a_dataset_knows_its_pool_and_whether_lumen_made_it() {
        let ours = Dataset {
            name: "rpool/lumen/vm-101-disk-0".into(),
            kind: DatasetKind::Volume,
            ..Dataset::default()
        };
        assert_eq!(ours.pool(), "rpool");
        assert!(ours.is_lumen_managed());

        let theirs = Dataset {
            name: "rpool/data".into(),
            ..Dataset::default()
        };
        assert_eq!(theirs.pool(), "rpool");
        assert!(!theirs.is_lumen_managed());
    }
}
