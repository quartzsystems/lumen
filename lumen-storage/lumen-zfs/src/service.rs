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
use crate::iso::{IsoLibrary, IsoStoreView, IsoUpload, IsoView};
use crate::model::{
    device_path, is_lumen_volume, is_reserved_leaf, valid_pool_name, Dataset, DatasetKind,
    PoolHealth, VolumeRequest,
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

/// GET /api/storage/iso. Both halves in one answer: what libraries the node
/// has, and what is in them — so a console that has to explain an empty picker
/// never needs a second request to find out why.
#[derive(Debug, Clone, Serialize)]
pub struct IsosResponse {
    pub node: String,
    pub stores: Vec<IsoStoreView>,
    pub images: Vec<IsoView>,
}

pub struct StorageService {
    backend: Arc<dyn ZfsBackend>,
    node: String,
    /// Serializes writes. Two virtual machines being created at once must not
    /// race to the same volume name.
    gate: Mutex<()>,
    isos: IsoLibrary,
}

impl StorageService {
    pub fn new(backend: Arc<dyn ZfsBackend>) -> Self {
        Self {
            backend,
            node: hostname(),
            gate: Mutex::new(()),
            isos: IsoLibrary::default(),
        }
    }

    /// The same service with its media library somewhere else — the seam the
    /// crate's own tests and the control plane's use so neither writes to the
    /// appliance's real directory.
    pub fn with_iso_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.isos = IsoLibrary::new(root);
        self
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

    // --- the installation media library -----------------------------------

    /// Every pool's library and everything in it.
    pub async fn isos(&self) -> Result<IsosResponse> {
        let pools = self.observe().await?.pools;
        let mut stores = Vec::with_capacity(pools.len());
        let mut images = Vec::new();
        for pool in &pools {
            stores.push(self.isos.store(&pool.name).await?);
            images.extend(self.isos.list(&pool.name).await?);
        }
        Ok(IsosResponse {
            node: self.node.clone(),
            stores,
            images,
        })
    }

    /// Make a pool's library, if the node's storage will let us.
    ///
    /// Creating the dataset also mounts it, and the control plane may not be
    /// able to see that mount until it restarts — so the store view is
    /// returned rather than a bare success, and it says which of those two
    /// happened.
    pub async fn create_iso_store(&self, pool: &str) -> Result<IsoStoreView> {
        let _guard = self.gate.lock().await;
        reject_bad_pool(pool)?;
        if !self.pool_exists(pool).await? {
            return Err(ZfsError::NotFound(format!(
                "No pool named \"{pool}\" on this node."
            )));
        }
        self.backend.ensure_iso_store(pool).await?;
        self.isos.store(pool).await
    }

    /// Begin storing an uploaded image. The caller streams into the returned
    /// handle and finishes it; nothing is visible under its real name until
    /// then.
    pub async fn begin_iso_upload(&self, pool: &str, name: &str) -> Result<IsoUpload> {
        reject_bad_pool(pool)?;
        self.isos.begin_upload(pool, name).await
    }

    /// Remove one image.
    pub async fn delete_iso(&self, pool: &str, name: &str) -> Result<()> {
        reject_bad_pool(pool)?;
        self.isos.delete(pool, name).await
    }

    /// The absolute path a domain document points at, checked. The compute
    /// domain calls this rather than building a path of its own, so there is
    /// one rule about what a media path may look like and it lives here.
    pub fn iso_path(&self, pool: &str, name: &str) -> Result<String> {
        reject_bad_pool(pool)?;
        Ok(self.isos.path(pool, name)?.to_string_lossy().into_owned())
    }

    /// Whether a path names a file that is actually in a library on this node.
    /// A machine must not be defined pointing at media that is not there.
    pub async fn iso_exists(&self, path: &str) -> Result<bool> {
        Ok(self
            .isos()
            .await?
            .images
            .iter()
            .any(|image| image.path == path))
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
    if is_reserved_leaf(path) {
        return Err(ZfsError::Conflict(format!(
            "\"{path}\" is this node's installation media library, not a machine's disk."
        )));
    }
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

    /// The library is made, seen, filled, and emptied through the service —
    /// and the pool that has no library says so rather than looking empty for
    /// no reason.
    #[tokio::test]
    async fn the_media_library_is_created_listed_and_emptied() {
        let root = std::env::temp_dir().join(format!(
            "lumen-svc-iso-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let backend = Arc::new(MockBackend::appliance());
        let service = StorageService::new(backend.clone()).with_iso_root(&root);

        // Nothing yet, and the reason is a command an operator can run.
        let before = service.isos().await.unwrap();
        assert_eq!(before.stores.len(), 1);
        assert!(!before.stores[0].ready);
        assert!(before.images.is_empty());

        // The dataset is created on the pool, and the directory now exists.
        let created = service.create_iso_store("rpool").await.unwrap();
        assert!(backend.has_dataset("rpool/lumen/iso"));
        // The mock creates the dataset but not the directory — exactly the
        // "made on the box, not yet visible here" case the view exists for.
        assert!(!created.ready);
        std::fs::create_dir_all(root.join("rpool")).unwrap();
        assert!(service.isos().await.unwrap().stores[0].ready);

        let mut upload = service
            .begin_iso_upload("rpool", "almalinux-10.iso")
            .await
            .unwrap();
        upload.write(b"CD001").await.unwrap();
        upload.finish().await.unwrap();

        let after = service.isos().await.unwrap();
        assert_eq!(after.images.len(), 1);
        assert_eq!(after.images[0].name, "almalinux-10.iso");
        let path = service.iso_path("rpool", "almalinux-10.iso").unwrap();
        assert_eq!(after.images[0].path, path);
        assert!(service.iso_exists(&path).await.unwrap());
        assert!(!service.iso_exists("/etc/passwd").await.unwrap());

        service
            .delete_iso("rpool", "almalinux-10.iso")
            .await
            .unwrap();
        assert!(service.isos().await.unwrap().images.is_empty());
        assert!(!service.iso_exists(&path).await.unwrap());

        std::fs::remove_dir_all(&root).ok();
    }

    /// The library is shaped like a volume and must never be destroyed as one.
    #[tokio::test]
    async fn the_media_library_cannot_be_destroyed_as_a_disk() {
        let (service, backend) = service();
        backend.ensure_iso_store("rpool").await.unwrap();
        let err = service.destroy_volume("rpool/lumen/iso").await.unwrap_err();
        assert!(matches!(err, ZfsError::Conflict(_)), "{err:?}");
        assert!(backend.has_dataset("rpool/lumen/iso"));
        // Nor created as one, which would put a machine's disk where the
        // media lives.
        assert!(service
            .create_volume("rpool", "iso", 1024, None)
            .await
            .is_err());
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
