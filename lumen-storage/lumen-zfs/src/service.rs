//! The storage domain's one entry point.
//!
//! Pools and what is under them, the volume a virtual machine's disk lives on,
//! and the media library — plus the two operations that build and remove a
//! pool.
//!
//! Those two are the only things in this crate that cannot happen inside the
//! control plane's sandbox: they write `/etc/zfs/zpool.cache`, which
//! `ProtectSystem=strict` makes read-only. They are handed to systemd through
//! [`lumen_sys::exec`] and run outside it, so the unit is unchanged. See
//! docs/system.md.
//!
//! `zpool import`, `export`, `scrub`, and `replace` are still absent — not for
//! want of a mechanism now, but because none of them is a decision this
//! console has anything useful to add to yet.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::backend::ZfsBackend;
use crate::error::{Result, ZfsError};
use crate::iso::{IsoLibrary, IsoStoreView, IsoUpload, IsoView};
use crate::model::{
    device_path, is_lumen_volume, is_reserved_leaf, valid_pool_name, BlockDevice, Dataset,
    DatasetKind, PoolHealth, PoolRequest, VolumeRequest,
};
use crate::state::{hostname, StorageState};
use crate::validate::{
    validate_pool, Acknowledgements, PoolCreate, ValidationCode, ValidationError,
};

/// GET /api/storage/devices — what the create dialog fills its picker from.
#[derive(Debug, Clone, Serialize)]
pub struct DevicesResponse {
    pub node: String,
    pub devices: Vec<BlockDevice>,
    /// The pool this appliance is installed on, so the console can say which
    /// one the Remove control will not touch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_pool: Option<String>,
}

/// One row of the console's pool table. Everything a row needs is here, so
/// rendering never needs a second round trip — the shape
/// `lumen_net::service::LinkView` established.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// The pool the appliance is installed on, which the console will not
    /// destroy. Read once at startup — it cannot change without a reboot, and
    /// a reboot restarts this daemon.
    root_pool: Option<String>,
}

impl StorageService {
    pub fn new(backend: Arc<dyn ZfsBackend>) -> Self {
        Self {
            backend,
            node: hostname(),
            gate: Mutex::new(()),
            isos: IsoLibrary::default(),
            root_pool: crate::state::root_pool(),
        }
    }

    /// Pretend the appliance is installed on this pool. For tests, which have
    /// no `/proc/mounts` of their own to arrange.
    pub fn with_root_pool(mut self, pool: Option<String>) -> Self {
        self.root_pool = pool;
        self
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
        let pools = observed
            .pools
            .into_iter()
            .map(|pool| view_of(pool, self.root_pool.as_deref()))
            .collect();
        Ok(PoolsResponse {
            nodes: vec![NodePools {
                node: self.node.clone(),
                pools,
            }],
        })
    }

    // --- pools ------------------------------------------------------------

    /// Every disk the node has, with what is already on each one.
    ///
    /// The answer the create dialog fills its picker from, and the reason it
    /// can refuse the disk the appliance is running from rather than listing it
    /// next to the empty ones with nothing to tell them apart.
    pub async fn block_devices(&self) -> Result<DevicesResponse> {
        Ok(DevicesResponse {
            node: self.node.clone(),
            devices: self.backend.block_devices().await?,
            root_pool: self.root_pool.clone(),
        })
    }

    /// Build a pool.
    ///
    /// Every check happens before anything is run, and there is exactly one
    /// operation afterwards — so a rejected request leaves the node's disks
    /// untouched, which is the only failure mode that matters here.
    pub async fn create_pool(
        &self,
        request: PoolCreate,
        ack: Acknowledgements,
    ) -> Result<PoolView> {
        let _guard = self.gate.lock().await;

        let existing = self.observe().await.map(|s| s.pools).unwrap_or_default();
        let devices = self.backend.block_devices().await.unwrap_or_default();
        let errors = validate_pool(&request, &existing, &devices, ack);
        if !errors.is_empty() {
            return Err(ZfsError::Invalid(errors));
        }

        // Resolve each chosen disk to the stable path the node reported for it,
        // never to whatever the request said. A pool built on `/dev/sdb` can
        // come back after a reboot pointing at a different disk; the by-id
        // path is the serial number and does not move.
        let disks: Vec<String> = request
            .disks
            .iter()
            .map(|chosen| {
                devices
                    .iter()
                    .find(|d| &d.path == chosen || &d.kernel_path == chosen || &d.name == chosen)
                    .map(|d| d.path.clone())
                    .unwrap_or_else(|| chosen.clone())
            })
            .collect();

        // Forcing is never the caller's word alone: it is only set when a disk
        // that already has something on it was chosen *and* acknowledged, both
        // of which the validator above has just confirmed.
        let force = disks.iter().any(|path| {
            devices
                .iter()
                .find(|d| &d.path == path)
                .is_some_and(|d| d.in_use)
        });

        let pool = self
            .backend
            .create_pool(&PoolRequest {
                name: request.name.trim().to_string(),
                vdev: request.vdev,
                disks,
                ashift: request.ashift,
                compression: request.compression,
                autotrim: request.autotrim,
                force,
            })
            .await?;

        tracing::info!(pool = %pool.name, vdev = ?request.vdev, "pool created");
        Ok(view_of(pool, self.root_pool.as_deref()))
    }

    /// Destroy one, and everything on it.
    ///
    /// The pool the appliance is installed on is refused outright — the same
    /// rule the account page keeps for `root`. Nothing the console offers may
    /// take the appliance away from the operator using it.
    pub async fn destroy_pool(&self, name: &str, ack: Acknowledgements) -> Result<()> {
        let _guard = self.gate.lock().await;
        reject_bad_pool(name)?;

        if self.root_pool.as_deref() == Some(name) {
            return Err(ZfsError::Conflict(format!(
                "\"{name}\" is the pool this appliance is installed on and cannot be destroyed \
                 from the console."
            )));
        }
        if !ack.may_lose_data {
            return Err(ZfsError::Invalid(vec![ValidationError::new(
                ValidationCode::UnacknowledgedDestructiveOperation,
                None,
                format!(
                    "Destroying \"{name}\" removes every dataset, volume, and snapshot on it. \
                     There is no undo. Confirm that you understand this may lose data."
                ),
            )]));
        }
        if !self.pool_exists(name).await? {
            return Err(ZfsError::NotFound(format!(
                "No pool named \"{name}\" on this node."
            )));
        }

        self.backend.destroy_pool(name).await?;
        tracing::warn!(pool = %name, "pool destroyed");
        Ok(())
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

    /// Grow a volume Lumen created. Grow only: a shrunk block device under a
    /// running guest is data loss with extra steps, so shrinking is refused
    /// here and everywhere above.
    pub async fn resize_volume(&self, path: &str, size: u64) -> Result<()> {
        let _guard = self.gate.lock().await;
        reject_outside_namespace(path)?;
        let current = self
            .backend
            .datasets(path.split('/').next().unwrap_or_default())
            .await?
            .into_iter()
            .find(|d| d.name == path)
            .ok_or_else(|| ZfsError::NotFound(format!("No volume named \"{path}\".")))?;
        if size <= current.volsize.unwrap_or(0) {
            return Err(ZfsError::Conflict(format!(
                "A volume only grows: \"{path}\" is already {} bytes.",
                current.volsize.unwrap_or(0)
            )));
        }
        self.backend.resize_volume(path, size).await
    }

    /// The block device a volume appears as, for the domain definition.
    pub fn device_path(&self, dataset: &str) -> String {
        device_path(dataset)
    }

    // --- snapshots --------------------------------------------------------

    /// Snapshot a volume Lumen created. Crash-consistent, and named by the
    /// caller — the storage domain has no opinion about snapshot schedules.
    pub async fn snapshot_volume(&self, path: &str, snapshot: &str) -> Result<()> {
        let _guard = self.gate.lock().await;
        reject_outside_namespace(path)?;
        self.backend.snapshot_volume(path, snapshot).await
    }

    /// Roll a volume back, discarding everything after the snapshot. The
    /// acknowledgement lives with the caller; the namespace check lives
    /// here, like every other volume verb.
    pub async fn rollback_volume(&self, path: &str, snapshot: &str) -> Result<()> {
        let _guard = self.gate.lock().await;
        reject_outside_namespace(path)?;
        self.backend.rollback_volume(path, snapshot).await
    }

    pub async fn destroy_snapshot(&self, path: &str, snapshot: &str) -> Result<()> {
        let _guard = self.gate.lock().await;
        reject_outside_namespace(path)?;
        self.backend.destroy_snapshot(path, snapshot).await
    }

    pub async fn volume_snapshots(&self, path: &str) -> Result<Vec<crate::model::SnapshotInfo>> {
        reject_outside_namespace(path)?;
        self.backend.snapshots(path).await
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

fn view_of(pool: crate::model::Pool, root_pool: Option<&str>) -> PoolView {
    // The pool the appliance itself is running from is never destroyable from
    // the console. Everything else is, with the acknowledgement — and this is
    // the same rule the account page keeps for `root`: nothing offered here may
    // take the appliance away from the operator using it.
    let blocked = match root_pool {
        Some(root) if root == pool.name => Some(format!(
            "\"{}\" is the pool this appliance is installed on.",
            pool.name
        )),
        _ => None,
    };
    PoolView {
        used_percent: pool.used_percent(),
        destroyable: blocked.is_none(),
        destroy_blocked_reason: blocked,
        name: pool.name,
        health: pool.health,
        size: pool.size,
        allocated: pool.allocated,
        free: pool.free,
        fragmentation: pool.fragmentation,
        dedup_ratio: pool.dedup_ratio,
        read_only: pool.read_only,
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
    use crate::model::VdevKind;

    /// The appliance's own node: one pool, which it is installed on.
    fn service() -> (StorageService, Arc<MockBackend>) {
        let backend = Arc::new(MockBackend::appliance());
        (
            StorageService::new(backend.clone()).with_root_pool(Some("boot".into())),
            backend,
        )
    }

    /// A node with disks to build a pool on: one holding the system, three
    /// free.
    fn node_with_disks() -> (StorageService, Arc<MockBackend>) {
        const TB: u64 = 1_000_000_000_000;
        let backend = Arc::new(MockBackend::appliance().with_disks(vec![
            MockBackend::busy_disk("sda", TB),
            MockBackend::free_disk("sdb", TB),
            MockBackend::free_disk("sdc", TB),
            MockBackend::free_disk("sdd", TB),
        ]));
        (
            StorageService::new(backend.clone()).with_root_pool(Some("boot".into())),
            backend,
        )
    }

    fn pool_create(name: &str, vdev: VdevKind, disks: &[&str]) -> PoolCreate {
        PoolCreate {
            name: name.into(),
            vdev,
            disks: disks.iter().map(|d| d.to_string()).collect(),
            ashift: None,
            compression: crate::model::Compression::Lz4,
            autotrim: true,
        }
    }

    fn acknowledged() -> Acknowledgements {
        Acknowledgements {
            may_lose_data: true,
        }
    }

    #[tokio::test]
    async fn pools_are_grouped_by_node_and_carry_what_a_row_needs() {
        let (service, _backend) = service();
        let response = service.pools().await.unwrap();
        assert_eq!(response.nodes.len(), 1);
        let boot = &response.nodes[0].pools[0];
        assert_eq!(boot.name, "boot");
        assert_eq!(boot.health, PoolHealth::Online);
        assert!(boot.size > 0);
        assert_eq!(boot.used_percent, 1);
        // This is the pool the appliance is installed on, so it is visibly not
        // destroyable with the reason said out loud rather than left to the
        // console to invent.
        assert!(!boot.destroyable);
        assert!(boot
            .destroy_blocked_reason
            .as_deref()
            .unwrap()
            .contains("installed on"));
    }

    #[tokio::test]
    async fn a_pool_is_built_on_the_disks_that_were_chosen() {
        let (service, backend) = node_with_disks();
        let pool = service
            .create_pool(
                pool_create("tank", VdevKind::Raidz1, &["sdb", "sdc", "sdd"]),
                Acknowledgements::default(),
            )
            .await
            .unwrap();

        assert_eq!(pool.name, "tank");
        // Not the pool the appliance runs from, so this one can be removed.
        assert!(pool.destroyable);
        assert!(backend.has_pool("tank"));

        let built = backend.created_pools();
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].vdev, VdevKind::Raidz1);
        // The picker offered kernel names; what was built is the stable path,
        // because /dev/sdb is whatever the kernel enumerated second this boot.
        assert_eq!(
            built[0].disks,
            [
                "/dev/disk/by-id/scsi-sdb",
                "/dev/disk/by-id/scsi-sdc",
                "/dev/disk/by-id/scsi-sdd"
            ]
        );
        // Nothing was forced, because nothing needed to be.
        assert!(!built[0].force);
    }

    /// The check the whole picker exists for.
    #[tokio::test]
    async fn the_disk_the_appliance_runs_from_is_refused_unless_acknowledged() {
        let (service, backend) = node_with_disks();
        let err = service
            .create_pool(
                pool_create("tank", VdevKind::Stripe, &["sda"]),
                Acknowledgements::default(),
            )
            .await
            .unwrap_err();

        let ZfsError::Invalid(errors) = err else {
            panic!("expected a validation failure");
        };
        assert_eq!(errors[0].code, ValidationCode::DiskInUse);
        assert!(backend.created_pools().is_empty(), "nothing may have run");

        // Acknowledged, it goes ahead — and only then is -f set.
        service
            .create_pool(
                pool_create("tank", VdevKind::Stripe, &["sda"]),
                acknowledged(),
            )
            .await
            .unwrap();
        assert!(backend.created_pools()[0].force);
    }

    #[tokio::test]
    async fn a_rejected_pool_never_touches_a_disk() {
        let (service, backend) = node_with_disks();
        // A vdev keyword for a name, two disks for a raidz2, and one of them
        // twice.
        let err = service
            .create_pool(
                pool_create("mirror", VdevKind::Raidz2, &["sdb", "sdb"]),
                acknowledged(),
            )
            .await
            .unwrap_err();
        let ZfsError::Invalid(errors) = err else {
            panic!("expected a validation failure");
        };
        assert!(errors.len() >= 3, "{errors:#?}");
        assert!(backend.created_pools().is_empty());
    }

    /// The same rule the account page keeps for `root`: nothing the console
    /// offers may take the appliance away from the operator using it.
    #[tokio::test]
    async fn the_pool_the_appliance_is_installed_on_is_never_destroyed() {
        let (service, backend) = service();
        let err = service
            .destroy_pool("boot", acknowledged())
            .await
            .unwrap_err();
        assert!(matches!(err, ZfsError::Conflict(_)), "{err:?}");
        assert!(err.to_string().contains("installed on"), "{err}");
        assert!(backend.has_pool("boot"), "it must still be there");
    }

    #[tokio::test]
    async fn destroying_a_pool_needs_the_acknowledgement() {
        let (service, backend) = node_with_disks();
        service
            .create_pool(
                pool_create("tank", VdevKind::Mirror, &["sdb", "sdc"]),
                Acknowledgements::default(),
            )
            .await
            .unwrap();

        let err = service
            .destroy_pool("tank", Acknowledgements::default())
            .await
            .unwrap_err();
        let ZfsError::Invalid(errors) = err else {
            panic!("expected a validation failure");
        };
        assert_eq!(
            errors[0].code,
            ValidationCode::UnacknowledgedDestructiveOperation
        );
        assert!(backend.has_pool("tank"));

        service.destroy_pool("tank", acknowledged()).await.unwrap();
        assert!(!backend.has_pool("tank"));
    }

    #[tokio::test]
    async fn destroying_a_pool_that_is_not_there_is_a_not_found() {
        let (service, _backend) = service();
        assert!(matches!(
            service
                .destroy_pool("tank", acknowledged())
                .await
                .unwrap_err(),
            ZfsError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn the_picker_reports_what_is_already_on_each_disk() {
        let (service, _backend) = node_with_disks();
        let response = service.block_devices().await.unwrap();
        assert_eq!(response.root_pool.as_deref(), Some("boot"));

        let sda = response.devices.iter().find(|d| d.name == "sda").unwrap();
        assert!(sda.in_use);
        assert_eq!(sda.used_by.as_deref(), Some("mounted at /"));

        let sdb = response.devices.iter().find(|d| d.name == "sdb").unwrap();
        assert!(!sdb.in_use);
    }

    #[tokio::test]
    async fn creating_a_disk_makes_the_namespace_on_the_way_past() {
        let (service, backend) = service();
        let dataset = service
            .create_volume("boot", "vm-101-disk-0", 8_589_934_592, Some(16_384))
            .await
            .unwrap();
        assert_eq!(dataset.name, "boot/lumen/vm-101-disk-0");
        assert_eq!(dataset.volsize, Some(8_589_934_592));
        assert!(backend.has_dataset("boot/lumen"));
        assert_eq!(
            service.device_path(&dataset.name),
            "/dev/zvol/boot/lumen/vm-101-disk-0"
        );

        // …and it shows up under the pool, with the pool root itself left out.
        let volumes = service.volumes("boot").await.unwrap();
        assert!(!volumes.volumes.iter().any(|v| v.name == "boot"));
        let disk = volumes
            .volumes
            .iter()
            .find(|v| v.name == "boot/lumen/vm-101-disk-0")
            .expect("the disk is listed");
        assert!(disk.lumen_managed);
        assert_eq!(disk.kind, DatasetKind::Volume);
    }

    #[tokio::test]
    async fn a_destroy_outside_the_namespace_is_refused_before_it_reaches_the_box() {
        let (service, backend) = service();
        for path in ["boot", "boot/lumen", "boot/data/important", "../etc"] {
            let err = service.destroy_volume(path).await.unwrap_err();
            assert!(matches!(err, ZfsError::Conflict(_)), "{path}: {err:?}");
        }
        // Nothing was removed on the way to being refused.
        assert!(backend.has_dataset("boot"));
    }

    #[tokio::test]
    async fn a_disk_can_be_created_and_removed_again() {
        let (service, backend) = service();
        let free_before = service.free_space("boot").await.unwrap();
        let dataset = service
            .create_volume("boot", "vm-100-disk-0", 1_073_741_824, None)
            .await
            .unwrap();
        assert!(service.free_space("boot").await.unwrap() < free_before);

        service.destroy_volume(&dataset.name).await.unwrap();
        assert!(!backend.has_dataset(&dataset.name));
        assert_eq!(service.free_space("boot").await.unwrap(), free_before);
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
        assert!(service.pool_exists("boot").await.unwrap());
    }

    #[tokio::test]
    async fn a_pool_name_that_is_really_a_path_never_reaches_the_backend() {
        let (service, _backend) = service();
        for pool in ["boot/lumen", "-rf", "", ".."] {
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
        let created = service.create_iso_store("boot").await.unwrap();
        assert!(backend.has_dataset("boot/lumen/iso"));
        // The mock creates the dataset but not the directory — exactly the
        // "made on the box, not yet visible here" case the view exists for.
        assert!(!created.ready);
        std::fs::create_dir_all(root.join("boot")).unwrap();
        assert!(service.isos().await.unwrap().stores[0].ready);

        let mut upload = service
            .begin_iso_upload("boot", "almalinux-10.iso")
            .await
            .unwrap();
        upload.write(b"CD001").await.unwrap();
        upload.finish().await.unwrap();

        let after = service.isos().await.unwrap();
        assert_eq!(after.images.len(), 1);
        assert_eq!(after.images[0].name, "almalinux-10.iso");
        let path = service.iso_path("boot", "almalinux-10.iso").unwrap();
        assert_eq!(after.images[0].path, path);
        assert!(service.iso_exists(&path).await.unwrap());
        assert!(!service.iso_exists("/etc/passwd").await.unwrap());

        service
            .delete_iso("boot", "almalinux-10.iso")
            .await
            .unwrap();
        assert!(service.isos().await.unwrap().images.is_empty());
        assert!(!service.iso_exists(&path).await.unwrap());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn snapshots_are_taken_listed_rolled_back_and_removed() {
        let (service, _) = service();
        let disk = service
            .create_volume("boot", "vm-101-disk-0", 1_073_741_824, None)
            .await
            .unwrap();

        service.snapshot_volume(&disk.name, "before").await.unwrap();
        service.snapshot_volume(&disk.name, "after").await.unwrap();
        let listed = service.volume_snapshots(&disk.name).await.unwrap();
        assert_eq!(
            listed.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["before", "after"]
        );

        // Rolling back to the older snapshot takes the newer one with it —
        // zfs rollback -r semantics, which is what "roll back" means.
        service.rollback_volume(&disk.name, "before").await.unwrap();
        let listed = service.volume_snapshots(&disk.name).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "before");

        service
            .destroy_snapshot(&disk.name, "before")
            .await
            .unwrap();
        assert!(service
            .volume_snapshots(&disk.name)
            .await
            .unwrap()
            .is_empty());
        // Nothing outside the namespace is snapshottable.
        assert!(service.snapshot_volume("boot", "x").await.is_err());
    }

    #[tokio::test]
    async fn a_volume_grows_but_never_shrinks() {
        let (service, _) = service();
        let disk = service
            .create_volume("boot", "vm-101-disk-0", 8_589_934_592, Some(16_384))
            .await
            .unwrap();

        service
            .resize_volume(&disk.name, 17_179_869_184)
            .await
            .unwrap();
        let volumes = service.volumes("boot").await.unwrap();
        let grown = volumes
            .volumes
            .iter()
            .find(|v| v.name == disk.name)
            .unwrap();
        assert_eq!(grown.volsize, Some(17_179_869_184));

        // The same size and a smaller size are both refusals.
        let err = service
            .resize_volume(&disk.name, 17_179_869_184)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only grows"), "{err}");
        assert!(service.resize_volume(&disk.name, 1024).await.is_err());
        // And nothing outside the namespace is resizable.
        assert!(service.resize_volume("boot", 1 << 40).await.is_err());
    }

    /// The library is shaped like a volume and must never be destroyed as one.
    #[tokio::test]
    async fn the_media_library_cannot_be_destroyed_as_a_disk() {
        let (service, backend) = service();
        backend.ensure_iso_store("boot").await.unwrap();
        let err = service.destroy_volume("boot/lumen/iso").await.unwrap_err();
        assert!(matches!(err, ZfsError::Conflict(_)), "{err:?}");
        assert!(backend.has_dataset("boot/lumen/iso"));
        // Nor created as one, which would put a machine's disk where the
        // media lives.
        assert!(service
            .create_volume("boot", "iso", 1024, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_zero_sized_volume_is_refused() {
        let (service, _backend) = service();
        assert!(service
            .create_volume("boot", "vm-101-disk-0", 0, None)
            .await
            .is_err());
    }
}
