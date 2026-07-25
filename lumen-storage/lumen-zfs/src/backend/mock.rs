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
    Dataset, DatasetKind, Pool, PoolHealth, VolumeRequest,
};

#[derive(Debug, Default)]
struct Inner {
    pools: Vec<Pool>,
    datasets: Vec<Dataset>,
    /// When set, the next create fails — the "pool filled up between the
    /// check and the write" case a caller has to survive.
    fail_next_create: Option<String>,
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
                fail_next_create: None,
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
                name: "rpool".into(),
                health: PoolHealth::Online,
                size,
                allocated,
                free: size - allocated,
                fragmentation: Some(0),
                dedup_ratio: Some(1.0),
                read_only: false,
            }],
            vec![Dataset {
                name: "rpool".into(),
                kind: DatasetKind::Filesystem,
                used: allocated,
                available: Some(size - allocated),
                referenced: 98_304,
                mountpoint: Some("/rpool".into()),
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
                name: "rpool".into(),
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

        backend.ensure_namespace("rpool").await.unwrap();
        let path = vm_disk_path("rpool", 101, 0);
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
        backend.ensure_namespace("rpool").await.unwrap();
        backend.ensure_namespace("rpool").await.unwrap();
        assert_eq!(
            backend
                .datasets_snapshot()
                .iter()
                .filter(|d| d.name == "rpool/lumen")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn nothing_outside_the_namespace_can_be_created_or_destroyed() {
        let backend = MockBackend::appliance();
        assert!(backend
            .create_volume(&request("rpool/data/important", 1024))
            .await
            .is_err());
        assert!(backend.destroy_volume("rpool/data").await.is_err());
        assert!(backend.destroy_volume("rpool").await.is_err());
    }

    #[tokio::test]
    async fn a_volume_larger_than_the_pool_is_refused() {
        let backend = MockBackend::nearly_full();
        backend.ensure_namespace("rpool").await.unwrap();
        let err = backend
            .create_volume(&request(&vm_disk_path("rpool", 100, 0), 8_589_934_592))
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
