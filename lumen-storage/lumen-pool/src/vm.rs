//! The narrow interface the compute domain consumes — and all of it.
//!
//! `lumen-virt` needs five things from replicated storage: make a disk,
//! recognise one, destroy one, know where a machine using them can run, and
//! hold the handover window around a live migration. This trait is those
//! five and nothing else, defined here so the compute domain's dependency
//! surface stays a single import — it never sees bricks, leases, sockets,
//! or ports.
//!
//! It lived in `lumen-drbd` first, as the seam both storage engines
//! implemented; with that engine retired the pool is the only implementor
//! left, and the seam lives with it. The trait keeps its engine-neutral
//! shape on purpose — `VirtService` holds it as `Arc<dyn VmVolumes>` and
//! has never known which engine answers.
//!
//! One rule this interface enforces rather than assumes: **a machine's disks
//! must be recognisable.** `disk_of` answers `None` for a local zvol and a
//! record for a pooled device, and that answer is how the compute domain
//! tells the disks that move with a machine from the ones that pin it.
//!
//! Identity is deliberately *not* written into the domain document. The
//! pool derives every vdisk's identity from its machine-disk name, and
//! `/dev/ublkb<id>` is the same string on every member; a second copy in
//! `<metadata>` would be a second source of truth to keep honest.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{PoolError, Result};

/// What a live migration is asking of the storage layer at this instant.
///
/// A LumenFS lease handover names its destination, and it distinguishes a
/// migration that completed from one that was abandoned — so the shape
/// carries all three moments, and lets the caller say what it already
/// knows. Migration closes the window on every path out, and has always
/// known whether the move succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationWindow {
    /// Both members may hold the disk open. The machine is still running
    /// here, and `destination` is the node it is moving to.
    Open { destination: String },
    /// The machine is running on the destination now: hand the disk over.
    Accepted,
    /// The migration did not happen. Close the window; the disk stays
    /// exactly where it was.
    Aborted,
}

/// What the compute domain asks for: a disk of `size_bytes`, named by its
/// machine. Placement is the engine's own — the pool puts every slice where
/// its map says, so there is nothing for the caller to choose.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmDiskRequest {
    /// The volume name, `vm-<vmid>-disk-<n>` — the compute domain's naming,
    /// carried through unchanged so the storage page and the machine page
    /// name the same thing.
    pub name: String,
    pub size_bytes: u64,
}

/// What it gets back: enough to place the device in a domain document and to
/// find the volume again later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicatedDisk {
    pub cluster: String,
    pub name: String,
    /// The stable block device — identical on every member, which is what
    /// makes the same domain document valid wherever the machine runs.
    pub device: String,
    pub size_bytes: u64,
    /// The nodes holding a replica — where a machine using this disk can
    /// run.
    pub members: Vec<String>,
}

#[async_trait]
pub trait VmVolumes: Send + Sync {
    /// Create a replicated disk for a machine on this node. No
    /// acknowledgement parameter anywhere on this trait: the compute
    /// domain's own flows carry the acknowledgements, and this interface
    /// trusts them the way `lumen-zfs` already does.
    async fn create_disk(&self, request: &VmDiskRequest) -> Result<ReplicatedDisk>;

    /// The replicated volume behind a device path, when there is one.
    /// `/dev/zvol/...` and anything else answer `None` — this is how the
    /// compute domain tells a replicated disk from a local one.
    async fn disk_of(&self, device: &str) -> Result<Option<ReplicatedDisk>>;

    /// Destroy the volume behind a device path: every replica, then the
    /// record.
    async fn destroy_disk(&self, device: &str) -> Result<()>;

    /// The nodes holding a replica of *every* listed device — where a
    /// machine using all of them can run. Refuses a device that is not
    /// replicated, by name.
    async fn common_members(&self, devices: &[String]) -> Result<Vec<String>>;

    /// Move the live-migration window through one of its states on the
    /// resource behind `device`, on every member.
    async fn migration_window(&self, device: &str, window: MigrationWindow) -> Result<()>;

    /// Make `device` exist on *this* node, because a machine is about to be
    /// started here.
    ///
    /// A pooled device is served by a daemon and exists only where that
    /// daemon has been asked to serve it — at create on the node that made
    /// it, on the destination when a migration window opens, and here
    /// whenever a machine arrives by any other route. An HA restart is
    /// exactly that other route, and so is starting a machine after the
    /// daemon restarted.
    ///
    /// Idempotent by contract: called before every start, including the
    /// ones where the device is already there.
    async fn ensure_local_device(&self, device: &str) -> Result<()>;
}

/// The seam where no replicated engine is: the standalone appliance, or a
/// clustered node whose cluster carries no pool.
///
/// `disk_of` answers `None` — a local disk is a local disk here, and the
/// compute domain's ordinary flows must keep working. Every verb that only
/// means something on a replicated disk refuses with the sentence an
/// operator can act on, which is exactly what the retired engine's service
/// answered from a node outside any cluster.
pub struct NoReplicatedStorage;

#[async_trait]
impl VmVolumes for NoReplicatedStorage {
    async fn create_disk(&self, _request: &VmDiskRequest) -> Result<ReplicatedDisk> {
        Err(PoolError::Conflict(
            "This node has no pooled storage, and replicated disks exist only on one. Create \
             the pool first, or give the machine a local disk."
                .to_string(),
        ))
    }

    async fn disk_of(&self, _device: &str) -> Result<Option<ReplicatedDisk>> {
        Ok(None)
    }

    async fn destroy_disk(&self, device: &str) -> Result<()> {
        Err(PoolError::NotFound(format!(
            "\"{device}\" is not a replicated volume this node knows."
        )))
    }

    async fn common_members(&self, devices: &[String]) -> Result<Vec<String>> {
        match devices.first() {
            None => Ok(Vec::new()),
            Some(device) => Err(PoolError::Conflict(format!(
                "\"{device}\" is not a replicated volume, so the machine cannot leave this \
                 node."
            ))),
        }
    }

    async fn migration_window(&self, device: &str, _window: MigrationWindow) -> Result<()> {
        Err(PoolError::NotFound(format!(
            "\"{device}\" is not a replicated volume this node knows."
        )))
    }

    async fn ensure_local_device(&self, device: &str) -> Result<()> {
        Err(PoolError::NotFound(format!(
            "\"{device}\" is not a replicated volume this node knows."
        )))
    }
}

// --- an in-memory implementation for the compute domain's tests --------------

/// The compute domain tests against this, never against a real
/// [`crate::PoolService`] — its tests must not need a pool, a daemon, or a
/// cluster to exist.
#[derive(Default)]
pub struct MockVmVolumes {
    inner: std::sync::Mutex<MockVmInner>,
}

#[derive(Default)]
struct MockVmInner {
    /// `None` simulates the standalone appliance: every call refuses.
    cluster: Option<(String, Vec<String>)>,
    next_id: u64,
    disks: Vec<ReplicatedDisk>,
    /// Every window adjustment, in order, as asked for.
    windows: Vec<(String, MigrationWindow)>,
    destroyed: Vec<String>,
    /// Devices readied for a local start, in order.
    ensured: Vec<String>,
    fail_migration_window: bool,
}

impl MockVmVolumes {
    /// The standalone appliance: no cluster, every call explains itself.
    pub fn standalone() -> Self {
        MockVmVolumes::default()
    }

    /// A node inside a cluster whose members are `nodes` (this node first).
    pub fn clustered(cluster: &str, nodes: &[&str]) -> Self {
        MockVmVolumes {
            inner: std::sync::Mutex::new(MockVmInner {
                cluster: Some((
                    cluster.to_string(),
                    nodes.iter().map(|n| (*n).to_string()).collect(),
                )),
                next_id: 1,
                ..MockVmInner::default()
            }),
        }
    }

    /// Make the next window adjustment fail, once — the migration guard's
    /// worst moment.
    pub fn fail_next_window(&self) {
        self.inner.lock().unwrap().fail_migration_window = true;
    }

    pub fn disks(&self) -> Vec<ReplicatedDisk> {
        self.inner.lock().unwrap().disks.clone()
    }

    pub fn destroyed(&self) -> Vec<String> {
        self.inner.lock().unwrap().destroyed.clone()
    }

    /// Devices readied for a local start, in order — how a test sees that a
    /// machine's disks were made to exist before it was started.
    pub fn ensured(&self) -> Vec<String> {
        self.inner.lock().unwrap().ensured.clone()
    }

    /// Every window adjustment, in order, as open-or-closed — the view
    /// that asks only "was the window up at this point".
    pub fn windows(&self) -> Vec<(String, bool)> {
        self.inner
            .lock()
            .unwrap()
            .windows
            .iter()
            .map(|(device, window)| {
                (
                    device.clone(),
                    matches!(window, MigrationWindow::Open { .. }),
                )
            })
            .collect()
    }

    /// Every window adjustment with its full meaning, so a test can tell a
    /// completed migration from an abandoned one.
    pub fn window_states(&self) -> Vec<(String, MigrationWindow)> {
        self.inner.lock().unwrap().windows.clone()
    }

    /// Devices whose handover window is open right now — zero after any
    /// completed migration, success or failure.
    pub fn open_windows(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        let mut open: Vec<String> = Vec::new();
        for (device, window) in &inner.windows {
            if matches!(window, MigrationWindow::Open { .. }) {
                if !open.contains(device) {
                    open.push(device.clone());
                }
            } else {
                open.retain(|d| d != device);
            }
        }
        open
    }
}

#[async_trait]
impl VmVolumes for MockVmVolumes {
    async fn create_disk(&self, request: &VmDiskRequest) -> Result<ReplicatedDisk> {
        let mut inner = self.inner.lock().unwrap();
        let Some((cluster, nodes)) = inner.cluster.clone() else {
            return Err(PoolError::Conflict(
                "This node has not joined an environment, and replicated volumes exist only \
                 inside one."
                    .to_string(),
            ));
        };
        let id = inner.next_id;
        inner.next_id += 1;
        let disk = ReplicatedDisk {
            cluster,
            name: request.name.clone(),
            device: crate::model::device_path(id),
            size_bytes: request.size_bytes,
            members: nodes,
        };
        inner.disks.push(disk.clone());
        Ok(disk)
    }

    async fn disk_of(&self, device: &str) -> Result<Option<ReplicatedDisk>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .disks
            .iter()
            .find(|d| d.device == device)
            .cloned())
    }

    async fn destroy_disk(&self, device: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let Some(index) = inner.disks.iter().position(|d| d.device == device) else {
            return Err(PoolError::NotFound(format!(
                "\"{device}\" is not a replicated volume this environment knows."
            )));
        };
        let disk = inner.disks.remove(index);
        inner.destroyed.push(disk.name);
        Ok(())
    }

    async fn common_members(&self, devices: &[String]) -> Result<Vec<String>> {
        let inner = self.inner.lock().unwrap();
        let mut common: Option<Vec<String>> = None;
        for device in devices {
            let Some(disk) = inner.disks.iter().find(|d| &d.device == device) else {
                return Err(PoolError::Conflict(format!(
                    "\"{device}\" is not a replicated volume, so the machine cannot leave this \
                     node."
                )));
            };
            common = Some(match common {
                None => disk.members.clone(),
                Some(current) => current
                    .into_iter()
                    .filter(|node| disk.members.contains(node))
                    .collect(),
            });
        }
        Ok(common.unwrap_or_default())
    }

    async fn ensure_local_device(&self, device: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.disks.iter().any(|disk| disk.device == device) {
            return Err(PoolError::NotFound(format!(
                "\"{device}\" is not a replicated volume this environment knows."
            )));
        }
        inner.ensured.push(device.to_string());
        Ok(())
    }

    async fn migration_window(&self, device: &str, window: MigrationWindow) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if matches!(window, MigrationWindow::Open { .. }) && inner.fail_migration_window {
            inner.fail_migration_window = false;
            return Err(PoolError::Conflict(
                "the peer refused to open the window".to_string(),
            ));
        }
        inner.windows.push((device.to_string(), window));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_mock_tracks_windows_the_way_the_guard_needs() {
        let volumes = MockVmVolumes::clustered("alpha", &["alpha-1", "alpha-2"]);
        let disk = volumes
            .create_disk(&VmDiskRequest {
                name: "vm-1-disk-0".into(),
                size_bytes: 1 << 30,
            })
            .await
            .unwrap();
        volumes
            .migration_window(
                &disk.device,
                MigrationWindow::Open {
                    destination: "lumen02".to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(volumes.open_windows(), vec![disk.device.clone()]);
        volumes
            .migration_window(&disk.device, MigrationWindow::Accepted)
            .await
            .unwrap();
        assert!(volumes.open_windows().is_empty());
    }

    #[tokio::test]
    async fn the_standalone_mock_refuses_like_a_node_with_no_pool() {
        let volumes = MockVmVolumes::standalone();
        let err = volumes
            .create_disk(&VmDiskRequest {
                name: "vm-1-disk-0".into(),
                size_bytes: 1 << 30,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("environment"), "{err}");
        assert!(volumes.disk_of("/dev/ublkb1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn no_replicated_storage_recognises_nothing_and_refuses_the_rest() {
        let seam = NoReplicatedStorage;
        // A local disk must stay a local disk: `None`, never an error.
        assert!(seam
            .disk_of("/dev/zvol/data/vm-1-disk-0")
            .await
            .unwrap()
            .is_none());
        assert!(seam.common_members(&[]).await.unwrap().is_empty());
        let err = seam
            .common_members(&["/dev/ublkb1".to_string()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot leave"), "{err}");
        let err = seam
            .create_disk(&VmDiskRequest {
                name: "vm-1-disk-0".into(),
                size_bytes: 1 << 30,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no pooled storage"), "{err}");
    }
}
