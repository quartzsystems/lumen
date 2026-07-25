//! Observed state: the pools and datasets the box actually has.
//!
//! There is no desired state in this crate. Everything here reads what is
//! already on the node — the pools it has, and among them the one it is
//! installed on, which is the one the console will not destroy.

use serde::{Deserialize, Serialize};

use crate::model::{Dataset, Pool};

/// Everything observed on one node.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageState {
    /// Hostname of the node these pools belong to.
    pub node: String,
    pub pools: Vec<Pool>,
}

impl StorageState {
    pub fn pool(&self, name: &str) -> Option<&Pool> {
        self.pools.iter().find(|p| p.name == name)
    }

    /// Space a new volume could take. A pool that is not there has none, which
    /// is what makes "the pool does not exist" and "the pool is full" produce
    /// the same refusal from the validator rather than a panic.
    pub fn free_space(&self, pool: &str) -> u64 {
        self.pool(pool).map(|p| p.free).unwrap_or(0)
    }
}

/// The datasets under one pool, as observed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolContents {
    pub pool: String,
    pub datasets: Vec<Dataset>,
}

/// This node's name.
///
/// Read from `/proc/sys/kernel/hostname`, which `ProtectKernelTunables=yes`
/// leaves readable — it only makes `/proc/sys` read-only. `lumen-net` reads it
/// the same way; duplicating six lines beats making the storage domain depend
/// on the networking one for a string.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "lumen".to_string())
}

/// The pool this appliance is installed on, if it is on one.
///
/// Read from `/proc/mounts`: the filesystem mounted at `/` with type `zfs` has
/// a source like `boot/ROOT/lumen`, and everything before the first `/` is the
/// pool. `ProtectSystem=strict` leaves `/proc` readable.
///
/// This is the one pool the console will not destroy. Knowing which it is
/// costs six lines and is the difference between a Remove control that is
/// merely dangerous and one that can end the appliance.
pub fn root_pool() -> Option<String> {
    root_pool_in(&std::fs::read_to_string("/proc/mounts").ok()?)
}

/// The same, over the contents of the file — so it is the thing under test.
pub fn root_pool_in(mounts: &str) -> Option<String> {
    mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let source = fields.next()?;
        let target = fields.next()?;
        let kind = fields.next()?;
        // A ZFS dataset's source is a dataset path, never something under
        // /dev — which is exactly what tells it apart from the ext4 root of a
        // node installed some other way.
        (target == "/" && kind == "zfs" && !source.starts_with('/'))
            .then(|| source.split('/').next().unwrap_or(source).to_string())
            .filter(|pool| !pool.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PoolHealth;

    #[test]
    fn the_pool_the_appliance_runs_from_is_the_one_mounted_at_root() {
        let mounts = "proc /proc proc rw 0 0\n\
                      boot/ROOT/lumen / zfs rw,relatime 0 0\n\
                      /dev/sda2 /boot ext4 rw 0 0\n\
                      boot/lumen/iso /var/lib/lumen/iso/boot zfs rw 0 0\n";
        assert_eq!(root_pool_in(mounts).as_deref(), Some("boot"));
    }

    /// A node installed on ext4, or one this cannot read, has no root pool —
    /// and then every pool is destroyable, which is the honest answer rather
    /// than protecting an arbitrary one.
    #[test]
    fn a_node_that_is_not_on_zfs_at_all_has_no_root_pool() {
        assert_eq!(root_pool_in("/dev/sda2 / ext4 rw 0 0\n"), None);
        assert_eq!(root_pool_in(""), None);
        // A ZFS dataset mounted somewhere that is not / is not the root.
        assert_eq!(root_pool_in("tank/data /srv zfs rw 0 0\n"), None);
    }

    #[test]
    fn free_space_on_a_pool_that_is_not_there_is_zero_not_a_panic() {
        let state = StorageState {
            node: "lumen".into(),
            pools: vec![Pool {
                name: "boot".into(),
                health: PoolHealth::Online,
                size: 1_000_000,
                allocated: 400_000,
                free: 600_000,
                ..Pool::default()
            }],
        };
        assert_eq!(state.free_space("boot"), 600_000);
        assert_eq!(state.free_space("tank"), 0);
        assert!(state.pool("tank").is_none());
    }

    #[test]
    fn a_node_always_has_a_name() {
        assert!(!hostname().is_empty());
    }
}
