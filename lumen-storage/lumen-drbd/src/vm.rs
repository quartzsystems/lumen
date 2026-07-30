//! The narrow interface the compute domain consumes — and all of it.
//!
//! `lumen-virt` needs five things from replicated storage: make a disk,
//! recognise one, destroy one, know where a machine using them can run, and
//! hold the two-primaries window around a live migration. This trait is
//! those five and nothing else, defined here so the compute domain's
//! dependency surface stays a single import — it never sees records,
//! renderers, peers, or ports.
//!
//! One rule this interface enforces rather than assumes: **the machine's own
//! node always holds a replica.** `/dev/drbd<minor>` exists only where a
//! replica does, so a machine whose node is not a member has no device to
//! open — the request is refused, not accommodated with a diskless client.
//!
//! Identity is deliberately *not* written into the domain document. The
//! membership record already carries every volume's identity and placement
//! (gossiped, durable, and readable when a node is dead — the property HA
//! needs), and `/dev/drbd<minor>` is stable on every member; a second copy
//! in `<metadata>` would be a second source of truth to keep honest. The
//! spec predates the record and is deviated from here, on purpose.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{DrbdError, Result};
use crate::service::{DrbdService, VolumeCreate, VolumeMemberCreate};

/// What a live migration is asking of the storage layer at this instant.
///
/// DRBD's window is symmetric — allow two primaries, or do not — but a
/// LumenFS lease handover names its destination, and it distinguishes a
/// migration that completed from one that was abandoned. One shape serves
/// both engines: the DRBD implementation collapses `Accepted` and
/// `Aborted` into "close the window", because which one it was does not
/// change what DRBD must do, while a pooled implementation uses all three.
///
/// It also lets the caller say what it already knows. Migration closes the
/// window on every path out, and has always known whether the move
/// succeeded; the old boolean discarded that at the door.
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
/// machine. `members` empty means "place it for me" — this node plus the
/// least-utilized other member, each on the pool its existing replicas
/// already use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmDiskRequest {
    /// The volume name, `vm-<vmid>-disk-<n>` — the compute domain's naming,
    /// carried through unchanged so the storage page and the machine page
    /// name the same thing.
    pub name: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub members: Vec<VolumeMemberCreate>,
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
}

#[async_trait]
impl VmVolumes for DrbdService {
    async fn create_disk(&self, request: &VmDiskRequest) -> Result<ReplicatedDisk> {
        let (cluster, node) = self.home_cluster()?;
        let members = if request.members.is_empty() {
            self.place_for_node(&cluster, &node)?
        } else {
            if !request.members.iter().any(|m| m.node == node) {
                return Err(DrbdError::Conflict(format!(
                    "The machine runs on \"{node}\", so \"{node}\" must hold a replica — the \
                     disk's device exists only where a replica does."
                )));
            }
            request.members.clone()
        };
        let view = self
            .create_volume(VolumeCreate {
                cluster: cluster.clone(),
                name: request.name.clone(),
                size_bytes: request.size_bytes,
                members,
            })
            .await?;
        Ok(ReplicatedDisk {
            cluster,
            name: view.name,
            device: view.device,
            size_bytes: request.size_bytes,
            members: view.replicas.into_iter().map(|r| r.node).collect(),
        })
    }

    async fn disk_of(&self, device: &str) -> Result<Option<ReplicatedDisk>> {
        let Some(minor) = parse_minor(device) else {
            return Ok(None);
        };
        let Some(membership) = self.membership_or_none()? else {
            return Ok(None);
        };
        for record in &membership.clusters {
            if let Some(volume) = record.volumes.iter().find(|v| v.minor == minor) {
                return Ok(Some(ReplicatedDisk {
                    cluster: record.definition.name.clone(),
                    name: volume.name.clone(),
                    device: device.to_string(),
                    size_bytes: volume.size_bytes,
                    members: volume.replicas.iter().map(|s| s.node.clone()).collect(),
                }));
            }
        }
        Ok(None)
    }

    async fn destroy_disk(&self, device: &str) -> Result<()> {
        let Some(disk) = self.disk_of(device).await? else {
            return Err(DrbdError::NotFound(format!(
                "\"{device}\" is not a replicated volume this environment knows."
            )));
        };
        // The compute domain's own flow carried the acknowledgement; this
        // internal call does not ask twice.
        self.destroy_volume(
            &disk.cluster,
            &disk.name,
            lumen_cluster::Acknowledgements {
                may_lose_data: true,
            },
        )
        .await
    }

    async fn common_members(&self, devices: &[String]) -> Result<Vec<String>> {
        let mut common: Option<Vec<String>> = None;
        for device in devices {
            let Some(disk) = self.disk_of(device).await? else {
                return Err(DrbdError::Conflict(format!(
                    "\"{device}\" is not a replicated volume, so the machine cannot leave this \
                     node."
                )));
            };
            common = Some(match common {
                None => disk.members,
                Some(current) => current
                    .into_iter()
                    .filter(|node| disk.members.contains(node))
                    .collect(),
            });
        }
        Ok(common.unwrap_or_default())
    }

    async fn migration_window(&self, device: &str, window: MigrationWindow) -> Result<()> {
        let Some(disk) = self.disk_of(device).await? else {
            return Err(DrbdError::NotFound(format!(
                "\"{device}\" is not a replicated volume this environment knows."
            )));
        };
        // DRBD has one switch, so both endings collapse to closing it: a
        // closed window is a machine that cannot have a second writer,
        // whether the migration landed or not.
        let allow = matches!(window, MigrationWindow::Open { .. });
        self.adjust_two_primaries(&disk.cluster, &disk.name, allow)
            .await
    }
}

/// The minor out of `/dev/drbd<minor>`, and nothing looser.
fn parse_minor(device: &str) -> Option<u32> {
    device.strip_prefix("/dev/drbd")?.parse().ok()
}

// --- an in-memory implementation for the compute domain's tests --------------

/// The compute domain tests against this, never against a real
/// [`DrbdService`] — its tests must not need an environment, a membership
/// record, or a cluster to exist.
#[derive(Default)]
pub struct MockVmVolumes {
    inner: std::sync::Mutex<MockVmInner>,
}

#[derive(Default)]
struct MockVmInner {
    /// `None` simulates the standalone appliance: every call refuses.
    cluster: Option<(String, Vec<String>)>,
    next_minor: u32,
    disks: Vec<ReplicatedDisk>,
    /// Every window adjustment, in order, as asked for.
    windows: Vec<(String, MigrationWindow)>,
    destroyed: Vec<String>,
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
                next_minor: 1,
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

    /// Devices whose two-primaries window is open right now — zero after
    /// any completed migration, success or failure.
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
            return Err(DrbdError::Conflict(
                "This node has not joined an environment, and replicated volumes exist only \
                 inside one."
                    .to_string(),
            ));
        };
        let members = if request.members.is_empty() {
            nodes.iter().take(2).cloned().collect()
        } else {
            request.members.iter().map(|m| m.node.clone()).collect()
        };
        let minor = inner.next_minor;
        inner.next_minor += 1;
        let disk = ReplicatedDisk {
            cluster,
            name: request.name.clone(),
            device: crate::model::device_path(minor),
            size_bytes: request.size_bytes,
            members,
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
            return Err(DrbdError::NotFound(format!(
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
                return Err(DrbdError::Conflict(format!(
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

    async fn migration_window(&self, device: &str, window: MigrationWindow) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if matches!(window, MigrationWindow::Open { .. }) && inner.fail_migration_window {
            inner.fail_migration_window = false;
            return Err(DrbdError::Conflict(
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

    #[test]
    fn a_device_path_parses_to_its_minor_and_nothing_looser() {
        assert_eq!(parse_minor("/dev/drbd3"), Some(3));
        assert_eq!(parse_minor("/dev/drbd12"), Some(12));
        assert_eq!(parse_minor("/dev/zvol/boot/lumen/vm-1-disk-0"), None);
        assert_eq!(parse_minor("/dev/drbd"), None);
        assert_eq!(parse_minor("/dev/drbd3x"), None);
    }

    #[tokio::test]
    async fn the_mock_tracks_windows_the_way_the_guard_needs() {
        let volumes = MockVmVolumes::clustered("alpha", &["alpha-1", "alpha-2"]);
        let disk = volumes
            .create_disk(&VmDiskRequest {
                name: "vm-1-disk-0".into(),
                size_bytes: 1 << 30,
                members: Vec::new(),
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
    async fn the_standalone_mock_refuses_like_the_real_service() {
        let volumes = MockVmVolumes::standalone();
        let err = volumes
            .create_disk(&VmDiskRequest {
                name: "vm-1-disk-0".into(),
                size_bytes: 1 << 30,
                members: Vec::new(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("environment"), "{err}");
        assert!(volumes.disk_of("/dev/drbd1").await.unwrap().is_none());
    }
}
