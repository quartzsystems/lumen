//! Observed state: the pools and datasets the box actually has.
//!
//! There is no desired state in this crate yet. Pools are created and imported
//! by the operator (and, from part 3, by a privileged executor); everything
//! here reads what is already there, plus the volumes Lumen itself made under
//! `<pool>/lumen/`.

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PoolHealth;

    #[test]
    fn free_space_on_a_pool_that_is_not_there_is_zero_not_a_panic() {
        let state = StorageState {
            node: "lumen".into(),
            pools: vec![Pool {
                name: "rpool".into(),
                health: PoolHealth::Online,
                size: 1_000_000,
                allocated: 400_000,
                free: 600_000,
                ..Pool::default()
            }],
        };
        assert_eq!(state.free_space("rpool"), 600_000);
        assert_eq!(state.free_space("tank"), 0);
        assert!(state.pool("tank").is_none());
    }

    #[test]
    fn a_node_always_has_a_name() {
        assert!(!hostname().is_empty());
    }
}
