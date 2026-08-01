//! In-memory storage. Deterministic, no pools, no kernel, no commands run.
//!
//! Compiled unconditionally and exported (see the note in `backend/mod.rs`) so
//! lumen-controlplane's integration tests can inject it the way
//! `tests/auth_flow.rs` injects its mock realm. Every test in this crate,
//! in `lumen-virt`, and in the control plane runs against it, which is what
//! lets `make test` pass on a machine with no storage software installed.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::backend::ZfsBackend;
use crate::error::{Result, ZfsError};
use crate::model::{
    is_lumen_volume, is_reserved_leaf, iso_dataset, iso_mountpoint, lumen_root, valid_pool_name,
    BlockDevice, Dataset, DatasetKind, Pool, PoolHealth, PoolRequest, VolumeRequest,
};

#[derive(Debug, Default)]
struct Inner {
    pools: Vec<Pool>,
    datasets: Vec<Dataset>,
    /// When set, the next create fails — the "pool filled up between the
    /// check and the write" case a caller has to survive.
    fail_next_create: Option<String>,
    /// The disks this pretend node has. Empty by default: a test that cares
    /// about pool creation says which disks exist.
    devices: Vec<BlockDevice>,
    /// Every pool this backend was asked to build, with its request — so a
    /// test can assert on the arrangement rather than on a pool existing.
    created: Vec<PoolRequest>,
    /// Snapshots by dataset path, oldest first.
    snapshots: std::collections::HashMap<String, Vec<crate::model::SnapshotInfo>>,
    /// `(device path, pool)` for every disk an imported pool is built on —
    /// what the real backend reads out of `zpool list -v`, and what a wipe
    /// has to be refused against.
    members: Vec<(String, String)>,
    /// Every disk this backend was asked to clear, in order.
    wiped: Vec<String>,
}

pub struct MockBackend {
    inner: Mutex<Inner>,
}

impl MockBackend {
    pub fn new(pools: Vec<Pool>, datasets: Vec<Dataset>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                pools,
                datasets,
                ..Inner::default()
            }),
        }
    }

    /// One healthy 1 TiB root pool with nothing of Lumen's in it yet — the
    /// shape of a freshly installed appliance.
    pub fn appliance() -> Self {
        let size = 1_099_511_627_776; // 1 TiB
        let allocated = 8_589_934_592; // 8 GiB, the installed system
        Self::new(
            vec![Pool {
                name: "boot".into(),
                health: PoolHealth::Online,
                size,
                allocated,
                free: size - allocated,
                fragmentation: Some(0),
                dedup_ratio: Some(1.0),
                read_only: false,
            }],
            vec![Dataset {
                name: "boot".into(),
                kind: DatasetKind::Filesystem,
                used: allocated,
                available: Some(size - allocated),
                referenced: 98_304,
                mountpoint: Some("/boot".into()),
                ..Dataset::default()
            }],
        )
    }

    /// A node whose pool is nearly full, for the validator's benefit.
    pub fn nearly_full() -> Self {
        let size = 1_099_511_627_776;
        let free = 1_073_741_824; // 1 GiB
        Self::new(
            vec![Pool {
                name: "boot".into(),
                health: PoolHealth::Online,
                size,
                allocated: size - free,
                free,
                fragmentation: Some(41),
                dedup_ratio: Some(1.0),
                read_only: false,
            }],
            Vec::new(),
        )
    }

    /// Everything the backend currently holds, for assertions.
    pub fn datasets_snapshot(&self) -> Vec<Dataset> {
        self.inner.lock().unwrap().datasets.clone()
    }

    pub fn has_dataset(&self, path: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .datasets
            .iter()
            .any(|d| d.name == path)
    }

    /// Make the next create fail with `reason`.
    pub fn fail_next_create(&self, reason: impl Into<String>) {
        self.inner.lock().unwrap().fail_next_create = Some(reason.into());
    }

    /// Give this pretend node some disks.
    ///
    /// A convenience with the shape that matters: two free disks and one the
    /// system is running from, which is the arrangement every pool-creation
    /// test wants to prove something about.
    pub fn with_disks(self, devices: Vec<BlockDevice>) -> Self {
        self.inner.lock().unwrap().devices = devices;
        self
    }

    /// A node with two free disks and one that holds the operating system.
    pub fn free_disk(name: &str, size: u64) -> BlockDevice {
        BlockDevice {
            name: name.into(),
            path: format!("/dev/disk/by-id/scsi-{name}"),
            kernel_path: format!("/dev/{name}"),
            size,
            model: Some("QEMU HARDDISK".into()),
            serial: None,
            rotational: false,
            removable: false,
            in_use: false,
            used_by: None,
            partitions: 0,
            claimed: false,
            wipeable: false,
            lumenfs: None,
        }
    }

    /// A disk carrying a LumenFS brick and nothing else — claimed by its
    /// own superblock, exactly what a reinstalled node's data disks look
    /// like. `pool_uuid` is the brick's pool, lowercase hex.
    pub fn brick_disk(name: &str, size: u64, pool_uuid: &str) -> BlockDevice {
        let brick = crate::model::LumenBrick {
            pool_uuid: pool_uuid.into(),
            brick_uuid: "cd".repeat(16),
            tier: 0,
            wal_holder: true,
        };
        BlockDevice {
            in_use: true,
            claimed: true,
            used_by: Some(crate::devices::brick_claim(&brick)),
            lumenfs: Some(brick),
            ..Self::free_disk(name, size)
        }
    }

    /// The disk the appliance is running from — the one a pool must never be
    /// built on by accident.
    pub fn busy_disk(name: &str, size: u64) -> BlockDevice {
        BlockDevice {
            in_use: true,
            used_by: Some("mounted at /".into()),
            partitions: 3,
            claimed: true,
            ..Self::free_disk(name, size)
        }
    }

    /// A disk somebody finished with: a partition table and nothing using it.
    ///
    /// The state the whole wipe exists for, and the one that makes a disk
    /// unselectable in the pool picker without saying it can be reclaimed.
    pub fn partitioned_disk(name: &str, size: u64, partitions: usize) -> BlockDevice {
        BlockDevice {
            in_use: true,
            used_by: Some(format!(
                "{partitions} partition{}",
                if partitions == 1 { "" } else { "s" }
            )),
            partitions,
            claimed: false,
            ..Self::free_disk(name, size)
        }
    }

    /// Put a disk into a pool without going through `create_pool` — for a
    /// test that needs a live pool member the disk scan cannot see, which is
    /// the case a wipe must refuse.
    pub fn with_pool_member(self, path: &str, pool: &str) -> Self {
        self.inner
            .lock()
            .unwrap()
            .members
            .push((path.to_string(), pool.to_string()));
        self
    }

    /// Every disk this backend was asked to clear, in order.
    pub fn wiped(&self) -> Vec<String> {
        self.inner.lock().unwrap().wiped.clone()
    }

    /// Every pool this backend was asked to build.
    pub fn created_pools(&self) -> Vec<PoolRequest> {
        self.inner.lock().unwrap().created.clone()
    }

    pub fn has_pool(&self, name: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .pools
            .iter()
            .any(|p| p.name == name)
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::appliance()
    }
}

#[async_trait]
impl ZfsBackend for MockBackend {
    async fn pools(&self) -> Result<Vec<Pool>> {
        Ok(self.inner.lock().unwrap().pools.clone())
    }

    async fn block_devices(&self) -> Result<Vec<BlockDevice>> {
        Ok(self.inner.lock().unwrap().devices.clone())
    }

    async fn pool_members(&self) -> Result<Vec<(String, String)>> {
        Ok(self.inner.lock().unwrap().members.clone())
    }

    async fn wipe_disk(&self, device: &BlockDevice) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        // The disk becomes what a cleared one is: no partitions, nothing on
        // it, and free for a pool — so a read after this sees what a real one
        // would rather than the state the test started with.
        if let Some(disk) = inner.devices.iter_mut().find(|d| d.name == device.name) {
            disk.partitions = 0;
            disk.claimed = false;
            disk.in_use = false;
            disk.used_by = None;
            disk.wipeable = false;
            // The identity sectors are zeroed too: the next scan sees no
            // brick, exactly as the real backend's dd leaves it.
            disk.lumenfs = None;
        }
        inner.wiped.push(device.name.clone());
        Ok(())
    }

    async fn create_pool(&self, request: &PoolRequest) -> Result<Pool> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(reason) = inner.fail_next_create.take() {
            return Err(ZfsError::Conflict(reason));
        }
        if inner.pools.iter().any(|p| p.name == request.name) {
            return Err(ZfsError::Conflict(format!(
                "cannot create '{}': pool already exists",
                request.name
            )));
        }

        // A size the arithmetic in the dialog would have predicted, so a test
        // can check the console is not shown a number that came from nowhere.
        let smallest = request
            .disks
            .iter()
            .filter_map(|path| {
                inner
                    .devices
                    .iter()
                    .find(|d| &d.path == path || &d.kernel_path == path)
                    .map(|d| d.size)
            })
            .min()
            .unwrap_or(0);
        let size = request.vdev.usable_bytes(request.disks.len(), smallest);

        let pool = Pool {
            name: request.name.clone(),
            health: PoolHealth::Online,
            size,
            allocated: 0,
            free: size,
            fragmentation: Some(0),
            dedup_ratio: Some(1.0),
            read_only: false,
        };
        inner.pools.push(pool.clone());
        inner.datasets.push(Dataset {
            name: request.name.clone(),
            kind: DatasetKind::Filesystem,
            used: 0,
            available: Some(size),
            referenced: 98_304,
            // Created with mountpoint=none, exactly as the real one is.
            mountpoint: None,
            ..Dataset::default()
        });
        inner.created.push(request.clone());
        Ok(pool)
    }

    async fn destroy_pool(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.pools.iter().any(|p| p.name == name) {
            return Err(ZfsError::NotFound(format!(
                "No pool named \"{name}\" on this node."
            )));
        }
        inner.pools.retain(|p| p.name != name);
        let prefix = format!("{name}/");
        inner
            .datasets
            .retain(|d| d.name != name && !d.name.starts_with(&prefix));
        Ok(())
    }

    async fn datasets(&self, pool: &str) -> Result<Vec<Dataset>> {
        if !valid_pool_name(pool) {
            return Err(ZfsError::Conflict(format!(
                "\"{pool}\" is not a usable pool name."
            )));
        }
        let inner = self.inner.lock().unwrap();
        if !inner.pools.iter().any(|p| p.name == pool) {
            return Err(ZfsError::NotFound(format!(
                "No pool named \"{pool}\" on this node."
            )));
        }
        let prefix = format!("{pool}/");
        Ok(inner
            .datasets
            .iter()
            .filter(|d| d.name == pool || d.name.starts_with(&prefix))
            .cloned()
            .collect())
    }

    async fn ensure_namespace(&self, pool: &str) -> Result<()> {
        let root = lumen_root(pool);
        let mut inner = self.inner.lock().unwrap();
        if !inner.pools.iter().any(|p| p.name == pool) {
            return Err(ZfsError::NotFound(format!(
                "No pool named \"{pool}\" on this node."
            )));
        }
        if inner.datasets.iter().any(|d| d.name == root) {
            return Ok(());
        }
        let available = inner.pools.iter().find(|p| p.name == pool).map(|p| p.free);
        inner.datasets.push(Dataset {
            name: root,
            kind: DatasetKind::Filesystem,
            used: 98_304,
            available,
            referenced: 98_304,
            ..Dataset::default()
        });
        Ok(())
    }

    async fn ensure_iso_store(&self, pool: &str) -> Result<String> {
        self.ensure_namespace(pool).await?;
        let dataset = iso_dataset(pool);
        let mountpoint = iso_mountpoint(pool);
        let mut inner = self.inner.lock().unwrap();
        if inner.datasets.iter().any(|d| d.name == dataset) {
            return Ok(mountpoint);
        }
        let available = inner.pools.iter().find(|p| p.name == pool).map(|p| p.free);
        inner.datasets.push(Dataset {
            name: dataset,
            kind: DatasetKind::Filesystem,
            used: 98_304,
            available,
            referenced: 98_304,
            mountpoint: Some(mountpoint.clone()),
            ..Dataset::default()
        });
        Ok(mountpoint)
    }

    async fn create_volume(&self, request: &VolumeRequest) -> Result<Dataset> {
        if is_reserved_leaf(&request.path) {
            return Err(ZfsError::Conflict(format!(
                "\"{}\" is this node's installation media library.",
                request.path
            )));
        }
        if !is_lumen_volume(&request.path) {
            return Err(ZfsError::Conflict(format!(
                "\"{}\" is outside the namespace this appliance manages.",
                request.path
            )));
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(reason) = inner.fail_next_create.take() {
            return Err(ZfsError::Backend(anyhow::anyhow!("{reason}")));
        }
        if inner.datasets.iter().any(|d| d.name == request.path) {
            return Err(ZfsError::Conflict(format!(
                "\"{}\" already exists.",
                request.path
            )));
        }
        let pool_name = request.path.split('/').next().unwrap_or_default();
        let Some(pool) = inner.pools.iter_mut().find(|p| p.name == pool_name) else {
            return Err(ZfsError::NotFound(format!(
                "No pool named \"{pool_name}\" on this node."
            )));
        };
        if request.size > pool.free {
            return Err(ZfsError::Conflict(format!(
                "\"{pool_name}\" has less free space than the volume asks for."
            )));
        }
        // A volume reserves its whole size up front, the way a thick zvol
        // does, so a second create sees the space genuinely gone.
        pool.free -= request.size;
        pool.allocated += request.size;
        let available = pool.free;

        let dataset = Dataset {
            name: request.path.clone(),
            kind: DatasetKind::Volume,
            used: request.size,
            available: Some(available),
            referenced: 56,
            volsize: Some(request.size),
            volblocksize: Some(request.blocksize.unwrap_or(16_384)),
            mountpoint: None,
        };
        inner.datasets.push(dataset.clone());
        Ok(dataset)
    }

    async fn destroy_volume(&self, path: &str) -> Result<()> {
        if is_reserved_leaf(path) || !is_lumen_volume(path) {
            return Err(ZfsError::Conflict(format!(
                "\"{path}\" is not a volume this appliance created, so it will not be removed."
            )));
        }
        let mut inner = self.inner.lock().unwrap();
        let Some(index) = inner.datasets.iter().position(|d| d.name == path) else {
            return Err(ZfsError::NotFound(format!("No volume named \"{path}\".")));
        };
        let removed = inner.datasets.remove(index);
        let pool_name = removed
            .name
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        if let Some(pool) = inner.pools.iter_mut().find(|p| p.name == pool_name) {
            let size = removed.volsize.unwrap_or(0);
            pool.free += size;
            pool.allocated = pool.allocated.saturating_sub(size);
        }
        Ok(())
    }

    async fn snapshot_volume(&self, path: &str, snapshot: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.datasets.iter().any(|d| d.name == path) {
            return Err(ZfsError::NotFound(format!("No volume named \"{path}\".")));
        }
        let snapshots = inner.snapshots.entry(path.to_string()).or_default();
        if snapshots.iter().any(|s| s.name == snapshot) {
            return Err(ZfsError::Conflict(format!(
                "\"{path}@{snapshot}\" already exists."
            )));
        }
        let created = 1_785_000_000 + snapshots.len() as u64;
        snapshots.push(crate::model::SnapshotInfo {
            name: snapshot.to_string(),
            used: 0,
            created,
        });
        Ok(())
    }

    async fn rollback_volume(&self, path: &str, snapshot: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let Some(snapshots) = inner.snapshots.get_mut(path) else {
            return Err(ZfsError::NotFound(format!("\"{path}\" has no snapshots.")));
        };
        let Some(index) = snapshots.iter().position(|s| s.name == snapshot) else {
            return Err(ZfsError::NotFound(format!(
                "No snapshot named \"{snapshot}\" on \"{path}\"."
            )));
        };
        // -r semantics: everything after the target goes with the rollback.
        snapshots.truncate(index + 1);
        Ok(())
    }

    async fn destroy_snapshot(&self, path: &str, snapshot: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let Some(snapshots) = inner.snapshots.get_mut(path) else {
            return Err(ZfsError::NotFound(format!(
                "No snapshot named \"{snapshot}\" on \"{path}\"."
            )));
        };
        let before = snapshots.len();
        snapshots.retain(|s| s.name != snapshot);
        if snapshots.len() == before {
            return Err(ZfsError::NotFound(format!(
                "No snapshot named \"{snapshot}\" on \"{path}\"."
            )));
        }
        Ok(())
    }

    async fn snapshots(&self, path: &str) -> Result<Vec<crate::model::SnapshotInfo>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .snapshots
            .get(path)
            .cloned()
            .unwrap_or_default())
    }

    async fn resize_volume(&self, path: &str, size: u64) -> Result<()> {
        if !is_lumen_volume(path) {
            return Err(ZfsError::Conflict(format!(
                "\"{path}\" is outside the namespace this appliance manages."
            )));
        }
        let mut inner = self.inner.lock().unwrap();
        let Some(index) = inner.datasets.iter().position(|d| d.name == path) else {
            return Err(ZfsError::NotFound(format!("No volume named \"{path}\".")));
        };
        let old = inner.datasets[index].volsize.unwrap_or(0);
        let grown = size.saturating_sub(old);
        let pool_name = path.split('/').next().unwrap_or_default().to_string();
        if let Some(pool) = inner.pools.iter_mut().find(|p| p.name == pool_name) {
            if grown > pool.free {
                return Err(ZfsError::Conflict(format!(
                    "\"{pool_name}\" has less free space than the growth asks for."
                )));
            }
            pool.free -= grown;
            pool.allocated += grown;
        }
        let dataset = &mut inner.datasets[index];
        dataset.volsize = Some(size);
        dataset.used = size;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vm_disk_path;

    fn request(path: &str, size: u64) -> VolumeRequest {
        VolumeRequest {
            path: path.into(),
            size,
            blocksize: None,
        }
    }

    #[tokio::test]
    async fn a_created_volume_takes_the_space_it_reserves() {
        let backend = MockBackend::appliance();
        let before = backend.pools().await.unwrap()[0].free;

        backend.ensure_namespace("boot").await.unwrap();
        let path = vm_disk_path("boot", 101, 0);
        let volume = backend
            .create_volume(&request(&path, 34_359_738_368))
            .await
            .unwrap();

        assert_eq!(volume.kind, DatasetKind::Volume);
        assert_eq!(volume.volsize, Some(34_359_738_368));
        assert!(backend.has_dataset(&path));
        assert_eq!(
            backend.pools().await.unwrap()[0].free,
            before - 34_359_738_368
        );

        backend.destroy_volume(&path).await.unwrap();
        assert!(!backend.has_dataset(&path));
        assert_eq!(backend.pools().await.unwrap()[0].free, before);
    }

    #[tokio::test]
    async fn creating_the_namespace_twice_is_success() {
        let backend = MockBackend::appliance();
        backend.ensure_namespace("boot").await.unwrap();
        backend.ensure_namespace("boot").await.unwrap();
        assert_eq!(
            backend
                .datasets_snapshot()
                .iter()
                .filter(|d| d.name == "boot/lumen")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn nothing_outside_the_namespace_can_be_created_or_destroyed() {
        let backend = MockBackend::appliance();
        assert!(backend
            .create_volume(&request("boot/data/important", 1024))
            .await
            .is_err());
        assert!(backend.destroy_volume("boot/data").await.is_err());
        assert!(backend.destroy_volume("boot").await.is_err());
    }

    #[tokio::test]
    async fn a_volume_larger_than_the_pool_is_refused() {
        let backend = MockBackend::nearly_full();
        backend.ensure_namespace("boot").await.unwrap();
        let err = backend
            .create_volume(&request(&vm_disk_path("boot", 100, 0), 8_589_934_592))
            .await
            .unwrap_err();
        assert!(matches!(err, ZfsError::Conflict(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_pool_that_is_not_there_is_a_not_found() {
        let backend = MockBackend::appliance();
        let err = backend.datasets("tank").await.unwrap_err();
        assert!(matches!(err, ZfsError::NotFound(_)), "{err:?}");
    }
}
