//! The storage domain's one entry point.
//!
//! Read-only to the console this stage — pools and what is under them — plus
//! the one write the compute domain needs: the volume a virtual machine's disk
//! lives on. Pool creation, import, and destroy are deliberately absent; they
//! are the operations with no privileged daemon to delegate to, and they are
//! what `lumen-execd` will exist for. See docs/compute.md.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;

use crate::backend::ZfsBackend;
use crate::error::{Result, ZfsError};
use crate::model::{
    device_path, is_lumen_volume, valid_pool_name, Dataset, DatasetKind, PoolHealth, VolumeRequest,
};
use crate::state::{hostname, StorageState};

/// One row of the console's pool table. Everything a row needs is here, so
/// rendering never needs a second round trip — the shape
/// `lumen_net::service::LinkView` established.
#[derive(Debug, Clone, Serialize)]
pub struct PoolView {
    pub name: String,
    pub health: PoolHealth,
    pub size: u64,
    pub allocated: u64,
    pub free: u64,
    /// Allocated as a percentage, computed once here rather than in the
    /// console, so the bar and the number can never disagree.
    pub used_percent: u8,
    pub fragmentation: Option<u8>,
    pub dedup_ratio: Option<f64>,
    pub read_only: bool,
    /// Always false this stage. The field is here from day one so the console
    /// can render the control it will eventually enable, greyed out with a
    /// reason, rather than growing a new column later.
    pub destroyable: bool,
    /// Why not, for a control that explains itself instead of being silently
    /// greyed out.
    pub destroy_blocked_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodePools {
    pub node: String,
    pub pools: Vec<PoolView>,
}

/// GET /api/storage/pools. Grouped by node even with one node, for the same
/// reason `/api/network/interfaces` is: the shape must not change when
/// clustering lands.
#[derive(Debug, Clone, Serialize)]
pub struct PoolsResponse {
    pub nodes: Vec<NodePools>,
}

/// One dataset or volume under a pool.
#[derive(Debug, Clone, Serialize)]
pub struct VolumeView {
    pub name: String,
    pub kind: DatasetKind,
    pub used: u64,
    pub available: Option<u64>,
    pub referenced: u64,
    pub volsize: Option<u64>,
    pub volblocksize: Option<u64>,
    pub mountpoint: Option<String>,
    /// Created by Lumen, and therefore something Lumen may remove.
    pub lumen_managed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumesResponse {
    pub node: String,
    pub pool: String,
    pub volumes: Vec<VolumeView>,
}

pub struct StorageService {
    backend: Arc<dyn ZfsBackend>,
    node: String,
    /// Serializes writes. Two virtual machines being created at once must not
    /// race to the same volume name.
    gate: Mutex<()>,
}

impl StorageService {
    pub fn new(backend: Arc<dyn ZfsBackend>) -> Self {
        Self {
            backend,
            node: hostname(),
            gate: Mutex::new(()),
        }
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    // --- reads -----------------------------------------------------------

    /// What the box has, as the domain's own type.
    pub async fn observe(&self) -> Result<StorageState> {
        Ok(StorageState {
            node: self.node.clone(),
            pools: self.backend.pools().await?,
        })
    }

    pub async fn pools(&self) -> Result<PoolsResponse> {
        let observed = self.observe().await?;
        let pools = observed.pools.into_iter().map(view_of).collect();
        Ok(PoolsResponse {
            nodes: vec![NodePools {
                node: self.node.clone(),
                pools,
            }],
        })
    }

    /// Datasets and volumes under one pool.
    pub async fn volumes(&self, pool: &str) -> Result<VolumesResponse> {
        reject_bad_pool(pool)?;
        let datasets = self.backend.datasets(pool).await?;
        Ok(VolumesResponse {
            node: self.node.clone(),
            pool: pool.to_string(),
            volumes: datasets
                .into_iter()
                // The pool's own root is not a row anyone came here to look
                // at — it is the table, not a line in it.
                .filter(|d| d.name != pool)
                .map(volume_view_of)
                .collect(),
        })
    }

    /// Free space in a pool, for the compute domain's validator. A pool that
    /// is not there has none, so "no such pool" and "pool is full" produce the
    /// same refusal rather than two different failures.
    pub async fn free_space(&self, pool: &str) -> Result<u64> {
        Ok(self.observe().await?.free_space(pool))
    }

    pub async fn pool_exists(&self, pool: &str) -> Result<bool> {
        Ok(self.observe().await?.pool(pool).is_some())
    }

    // --- the one write ----------------------------------------------------

    /// Create the volume a virtual machine's disk lives on, under
    /// `<pool>/lumen/`, creating that parent if this is the first one.
    pub async fn create_volume(
        &self,
        pool: &str,
        name: &str,
        size: u64,
        blocksize: Option<u64>,
    ) -> Result<Dataset> {
        let _guard = self.gate.lock().await;
        reject_bad_pool(pool)?;
        let path = format!("{}/{name}", crate::model::lumen_root(pool));
        reject_outside_namespace(&path)?;
        if size == 0 {
            return Err(ZfsError::Conflict(
                "A volume needs a size greater than zero.".into(),
            ));
        }
        self.backend.ensure_namespace(pool).await?;
        self.backend
            .create_volume(&VolumeRequest {
                path,
                size,
                blocksize,
            })
            .await
    }

    /// Remove a volume Lumen created. Refuses anything else, and says so.
    pub async fn destroy_volume(&self, path: &str) -> Result<()> {
        let _guard = self.gate.lock().await;
        reject_outside_namespace(path)?;
        self.backend.destroy_volume(path).await
    }

    /// The block device a volume appears as, for the domain definition.
    pub fn device_path(&self, dataset: &str) -> String {
        device_path(dataset)
    }
}

fn reject_bad_pool(pool: &str) -> Result<()> {
    if valid_pool_name(pool) {
        return Ok(());
    }
    Err(ZfsError::NotFound(format!(
        "No pool named \"{pool}\" on this node."
    )))
}

fn reject_outside_namespace(path: &str) -> Result<()> {
    if is_lumen_volume(path) {
        return Ok(());
    }
    Err(ZfsError::Conflict(format!(
        "\"{path}\" is not a volume this appliance created, so it will not be removed. Only \
         volumes under a pool's \"lumen\" dataset are managed here."
    )))
}

fn view_of(pool: crate::model::Pool) -> PoolView {
    // Pool creation and removal arrive with the privileged executor in a later
    // stage. Until then every pool says the same thing, in the words an
    // operator can act on rather than as a greyed-out control with no reason.
    let reason = Some(
        "Pools are created and removed from the node itself for now — the console will manage \
         them in a later release."
            .to_string(),
    );
    PoolView {
        used_percent: pool.used_percent(),
        name: pool.name,
        health: pool.health,
        size: pool.size,
        allocated: pool.allocated,
        free: pool.free,
        fragmentation: pool.fragmentation,
        dedup_ratio: pool.dedup_ratio,
        read_only: pool.read_only,
        destroyable: false,
        destroy_blocked_reason: reason,
    }
}

fn volume_view_of(dataset: Dataset) -> VolumeView {
    VolumeView {
        lumen_managed: dataset.is_lumen_managed(),
        name: dataset.name,
        kind: dataset.kind,
        used: dataset.used,
        available: dataset.available,
        referenced: dataset.referenced,
        volsize: dataset.volsize,
        volblocksize: dataset.volblocksize,
        mountpoint: dataset.mountpoint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    fn service() -> (StorageService, Arc<MockBackend>) {
        let backend = Arc::new(MockBackend::appliance());
        (StorageService::new(backend.clone()), backend)
    }

    #[tokio::test]
    async fn pools_are_grouped_by_node_and_carry_what_a_row_needs() {
        let (service, _backend) = service();
        let response = service.pools().await.unwrap();
        assert_eq!(response.nodes.len(), 1);
        let rpool = &response.nodes[0].pools[0];
        assert_eq!(rpool.name, "rpool");
        assert_eq!(rpool.health, PoolHealth::Online);
        assert!(rpool.size > 0);
        assert_eq!(rpool.used_percent, 1);
        // rpool exists and is visibly not destroyable, with the reason said
        // out loud rather than left to the console to invent.
        assert!(!rpool.destroyable);
        assert!(rpool.destroy_blocked_reason.is_some());
    }

    #[tokio::test]
    async fn creating_a_disk_makes_the_namespace_on_the_way_past() {
        let (service, backend) = service();
        let dataset = service
            .create_volume("rpool", "vm-101-disk-0", 8_589_934_592, Some(16_384))
            .await
            .unwrap();
        assert_eq!(dataset.name, "rpool/lumen/vm-101-disk-0");
        assert_eq!(dataset.volsize, Some(8_589_934_592));
        assert!(backend.has_dataset("rpool/lumen"));
        assert_eq!(
            service.device_path(&dataset.name),
            "/dev/zvol/rpool/lumen/vm-101-disk-0"
        );

        // …and it shows up under the pool, with the pool root itself left out.
        let volumes = service.volumes("rpool").await.unwrap();
        assert!(!volumes.volumes.iter().any(|v| v.name == "rpool"));
        let disk = volumes
            .volumes
            .iter()
            .find(|v| v.name == "rpool/lumen/vm-101-disk-0")
            .expect("the disk is listed");
        assert!(disk.lumen_managed);
        assert_eq!(disk.kind, DatasetKind::Volume);
    }

    #[tokio::test]
    async fn a_destroy_outside_the_namespace_is_refused_before_it_reaches_the_box() {
        let (service, backend) = service();
        for path in ["rpool", "rpool/lumen", "rpool/data/important", "../etc"] {
            let err = service.destroy_volume(path).await.unwrap_err();
            assert!(matches!(err, ZfsError::Conflict(_)), "{path}: {err:?}");
        }
        // Nothing was removed on the way to being refused.
        assert!(backend.has_dataset("rpool"));
    }

    #[tokio::test]
    async fn a_disk_can_be_created_and_removed_again() {
        let (service, backend) = service();
        let free_before = service.free_space("rpool").await.unwrap();
        let dataset = service
            .create_volume("rpool", "vm-100-disk-0", 1_073_741_824, None)
            .await
            .unwrap();
        assert!(service.free_space("rpool").await.unwrap() < free_before);

        service.destroy_volume(&dataset.name).await.unwrap();
        assert!(!backend.has_dataset(&dataset.name));
        assert_eq!(service.free_space("rpool").await.unwrap(), free_before);
    }

    #[tokio::test]
    async fn a_pool_that_is_not_there_reads_as_not_found_and_as_no_space() {
        let (service, _backend) = service();
        assert!(matches!(
            service.volumes("tank").await.unwrap_err(),
            ZfsError::NotFound(_)
        ));
        assert_eq!(service.free_space("tank").await.unwrap(), 0);
        assert!(!service.pool_exists("tank").await.unwrap());
        assert!(service.pool_exists("rpool").await.unwrap());
    }

    #[tokio::test]
    async fn a_pool_name_that_is_really_a_path_never_reaches_the_backend() {
        let (service, _backend) = service();
        for pool in ["rpool/lumen", "-rf", "", ".."] {
            assert!(service.volumes(pool).await.is_err(), "{pool}");
            assert!(service
                .create_volume(pool, "vm-1-disk-0", 1024, None)
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn a_zero_sized_volume_is_refused() {
        let (service, _backend) = service();
        assert!(service
            .create_volume("rpool", "vm-101-disk-0", 0, None)
            .await
            .is_err());
    }
}
