//! The volume half of the peer channel: what one control plane asks another
//! to do to its zvols and its DRBD.
//!
//! The trait mirrors `lumen_cluster::PeerChannel` deliberately — the control
//! plane implements both on the same TLS client, tests implement both in
//! memory, and the local node is addressed like any other member. A member
//! is prepared whole or not at all: the zvol, the resource file, the
//! metadata, and the up are one call, so the unwind only ever has whole
//! members to reason about.

use std::sync::Mutex;

use async_trait::async_trait;
use lumen_cluster::EnvironmentNode;
use serde::{Deserialize, Serialize};

use crate::error::{DrbdError, Result};

/// Everything one member needs to carry its replica: create the backing
/// zvol, write the resource file (which carries the replication secret —
/// the same travel-in-the-payload, never-in-the-record path as the corosync
/// authkey), size the metadata, and bring the resource up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumePrepare {
    pub cluster: String,
    /// The DRBD resource name, `<cluster>-<volume>`.
    pub resource: String,
    /// The backing zvol's dataset path on this member.
    pub zvol: String,
    /// The byte-exact backing size — identical on every member by
    /// construction.
    pub zvol_bytes: u64,
    pub volblocksize: u64,
    pub minor: u32,
    /// Peer slots for `create-md`.
    pub peers: usize,
    /// The rendered resource file, secret included.
    pub resource_file: String,
}

/// What teardown needs to put a member back exactly as it was.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeTeardown {
    pub resource: String,
    pub zvol: String,
}

/// Grow one member's backing zvol. The resource itself is grown once,
/// afterwards, with [`VolumePeers::grow`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeResizeBacking {
    pub resource: String,
    pub zvol: String,
    pub zvol_bytes: u64,
}

/// One member's snapshot of its backing zvol — taking one, rolling back to
/// one, or dropping one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeSnapshot {
    pub zvol: String,
    pub snapshot: String,
}

/// A re-rendered resource file with [`SECRET_PLACEHOLDER`] where the
/// shared-secret goes — the member substitutes its own on arrival, because
/// the secret never leaves the nodes that hold it. The 2→3 policy flip's
/// payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeApply {
    pub resource: String,
    pub resource_file: String,
}

/// The marker `VolumeApply::resource_file` carries in place of the secret.
pub const SECRET_PLACEHOLDER: &str = "@@LUMEN-SECRET@@";

#[async_trait]
pub trait VolumePeers: Send + Sync {
    /// Make one member carry its replica, whole or not at all.
    async fn prepare(&self, node: &EnvironmentNode, payload: &VolumePrepare) -> Result<()>;

    /// Skip the initial sync of a fresh volume — run once, on one member,
    /// after every member is up.
    async fn prime(&self, node: &EnvironmentNode, resource: &str) -> Result<()>;

    /// Undo a prepare: resource down, file removed, zvol destroyed.
    async fn teardown(&self, node: &EnvironmentNode, payload: &VolumeTeardown) -> Result<()>;

    /// Grow one member's backing device.
    async fn resize_backing(
        &self,
        node: &EnvironmentNode,
        payload: &VolumeResizeBacking,
    ) -> Result<()>;

    /// Let the resource take its grown backing — run once, after every
    /// member's backing has grown.
    async fn grow(&self, node: &EnvironmentNode, resource: &str) -> Result<()>;

    /// Open or close the two-primaries window on one member — the
    /// live-migration guard's reach into each replica.
    async fn two_primaries(
        &self,
        node: &EnvironmentNode,
        resource: &str,
        allow: bool,
    ) -> Result<()>;

    /// Snapshot one member's backing zvol.
    async fn snapshot_backing(
        &self,
        node: &EnvironmentNode,
        payload: &VolumeSnapshot,
    ) -> Result<()>;

    /// Roll one member's backing zvol back — the rollback workflow's
    /// destructive middle, run on exactly one member while the resource is
    /// down everywhere.
    async fn rollback_backing(
        &self,
        node: &EnvironmentNode,
        payload: &VolumeSnapshot,
    ) -> Result<()>;

    /// Drop one member's snapshot.
    async fn drop_snapshot(&self, node: &EnvironmentNode, payload: &VolumeSnapshot) -> Result<()>;

    /// Take the resource down on one member without touching its file or
    /// its zvol — the rollback workflow's opening.
    async fn down_resource(&self, node: &EnvironmentNode, resource: &str) -> Result<()>;

    /// Bring it back up.
    async fn up_resource(&self, node: &EnvironmentNode, resource: &str) -> Result<()>;

    /// Declare the peers of one member out of date — the member that rolled
    /// back is the truth, and everyone else resyncs from it.
    async fn invalidate_remote(&self, node: &EnvironmentNode, resource: &str) -> Result<()>;

    /// Reconnect one member, discarding its own writes when it is the
    /// split-brain victim.
    async fn reconnect(&self, node: &EnvironmentNode, resource: &str, discard: bool) -> Result<()>;

    /// Hand one member a re-rendered resource file (secret placeholder
    /// included) to substitute, write, and `drbdadm adjust` — live.
    async fn apply_policy(&self, node: &EnvironmentNode, payload: &VolumeApply) -> Result<()>;
}

// --- in-memory peers --------------------------------------------------------

/// Peers that exist only in memory, for tests — this crate's and the control
/// plane's, which is why it is compiled unconditionally like the backend
/// mocks.
#[derive(Default)]
pub struct MockVolumePeers {
    inner: Mutex<MockInner>,
    /// When set, a successful prime registers the formed resource into this
    /// backend, the way the replicas connecting would make it visible to the
    /// coordinator's reads.
    backend: Option<std::sync::Arc<crate::backend::mock::MockBackend>>,
}

#[derive(Default)]
struct MockInner {
    prepared: Vec<(String, VolumePrepare)>,
    primed: Vec<(String, String)>,
    torn_down: Vec<(String, VolumeTeardown)>,
    resized: Vec<(String, VolumeResizeBacking)>,
    grown: Vec<(String, String)>,
    two_primaries: Vec<(String, String, bool)>,
    snapshots: Vec<(String, VolumeSnapshot)>,
    rollbacks: Vec<(String, VolumeSnapshot)>,
    dropped_snapshots: Vec<(String, VolumeSnapshot)>,
    downs: Vec<(String, String)>,
    ups: Vec<(String, String)>,
    invalidated: Vec<(String, String)>,
    reconnects: Vec<(String, String, bool)>,
    applied: Vec<(String, VolumeApply)>,
    fail_prepare: Option<String>,
    fail_teardown: Option<String>,
    fail_resize: Option<String>,
    fail_snapshot: Option<String>,
    fail_up: Option<String>,
}

impl MockVolumePeers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire the peers to a mock backend so a completed create makes the
    /// resource observable, as the replicas connecting does on a real node.
    pub fn with_backend(
        mut self,
        backend: std::sync::Arc<crate::backend::mock::MockBackend>,
    ) -> Self {
        self.backend = Some(backend);
        self
    }

    pub fn fail_prepare_on(self, node: &str) -> Self {
        self.inner.lock().unwrap().fail_prepare = Some(node.to_string());
        self
    }

    pub fn fail_teardown_on(self, node: &str) -> Self {
        self.inner.lock().unwrap().fail_teardown = Some(node.to_string());
        self
    }

    pub fn fail_resize_on(self, node: &str) -> Self {
        self.inner.lock().unwrap().fail_resize = Some(node.to_string());
        self
    }

    pub fn prepared(&self) -> Vec<(String, VolumePrepare)> {
        self.inner.lock().unwrap().prepared.clone()
    }

    pub fn primed(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().primed.clone()
    }

    pub fn torn_down(&self) -> Vec<(String, VolumeTeardown)> {
        self.inner.lock().unwrap().torn_down.clone()
    }

    pub fn resized(&self) -> Vec<(String, VolumeResizeBacking)> {
        self.inner.lock().unwrap().resized.clone()
    }

    pub fn grown(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().grown.clone()
    }

    /// Every two-primaries adjustment, in order: (node, resource, allow).
    pub fn two_primaries(&self) -> Vec<(String, String, bool)> {
        self.inner.lock().unwrap().two_primaries.clone()
    }

    pub fn fail_snapshot_on(self, node: &str) -> Self {
        self.inner.lock().unwrap().fail_snapshot = Some(node.to_string());
        self
    }

    pub fn fail_up_on(self, node: &str) -> Self {
        self.inner.lock().unwrap().fail_up = Some(node.to_string());
        self
    }

    pub fn snapshots(&self) -> Vec<(String, VolumeSnapshot)> {
        self.inner.lock().unwrap().snapshots.clone()
    }

    pub fn rollbacks(&self) -> Vec<(String, VolumeSnapshot)> {
        self.inner.lock().unwrap().rollbacks.clone()
    }

    pub fn dropped_snapshots(&self) -> Vec<(String, VolumeSnapshot)> {
        self.inner.lock().unwrap().dropped_snapshots.clone()
    }

    pub fn downs(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().downs.clone()
    }

    pub fn ups(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().ups.clone()
    }

    pub fn invalidated(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().invalidated.clone()
    }

    /// Every reconnect, in order: (node, resource, discarded).
    pub fn reconnects(&self) -> Vec<(String, String, bool)> {
        self.inner.lock().unwrap().reconnects.clone()
    }

    pub fn applied(&self) -> Vec<(String, VolumeApply)> {
        self.inner.lock().unwrap().applied.clone()
    }
}

#[async_trait]
impl VolumePeers for MockVolumePeers {
    async fn prepare(&self, node: &EnvironmentNode, payload: &VolumePrepare) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_prepare.as_deref() == Some(node.name.as_str()) {
            return Err(DrbdError::Conflict(format!(
                "the zvol on \"{}\" could not be created",
                node.name
            )));
        }
        inner.prepared.push((node.name.clone(), payload.clone()));
        Ok(())
    }

    async fn prime(&self, node: &EnvironmentNode, resource: &str) -> Result<()> {
        let (peer, minor) = {
            let mut inner = self.inner.lock().unwrap();
            inner.primed.push((node.name.clone(), resource.to_string()));
            let peer = inner
                .prepared
                .iter()
                .find(|(prepared_node, p)| p.resource == resource && prepared_node != &node.name)
                .map(|(n, _)| n.clone());
            let minor = inner
                .prepared
                .iter()
                .find(|(_, p)| p.resource == resource)
                .map(|(_, p)| p.minor)
                .unwrap_or(1);
            (peer, minor)
        };
        if let (Some(backend), Some(peer)) = (&self.backend, peer) {
            backend.register(crate::backend::mock::healthy_resource(
                resource, &peer, minor,
            ));
        }
        Ok(())
    }

    async fn teardown(&self, node: &EnvironmentNode, payload: &VolumeTeardown) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_teardown.as_deref() == Some(node.name.as_str()) {
            return Err(DrbdError::Conflict(format!(
                "\"{}\" is not answering",
                node.name
            )));
        }
        inner.torn_down.push((node.name.clone(), payload.clone()));
        Ok(())
    }

    async fn resize_backing(
        &self,
        node: &EnvironmentNode,
        payload: &VolumeResizeBacking,
    ) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_resize.as_deref() == Some(node.name.as_str()) {
            return Err(DrbdError::Conflict(format!(
                "the zvol on \"{}\" could not grow",
                node.name
            )));
        }
        inner.resized.push((node.name.clone(), payload.clone()));
        Ok(())
    }

    async fn grow(&self, node: &EnvironmentNode, resource: &str) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .grown
            .push((node.name.clone(), resource.to_string()));
        Ok(())
    }

    async fn two_primaries(
        &self,
        node: &EnvironmentNode,
        resource: &str,
        allow: bool,
    ) -> Result<()> {
        self.inner.lock().unwrap().two_primaries.push((
            node.name.clone(),
            resource.to_string(),
            allow,
        ));
        Ok(())
    }

    async fn snapshot_backing(
        &self,
        node: &EnvironmentNode,
        payload: &VolumeSnapshot,
    ) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_snapshot.as_deref() == Some(node.name.as_str()) {
            return Err(DrbdError::Conflict(format!(
                "the snapshot on \"{}\" failed",
                node.name
            )));
        }
        inner.snapshots.push((node.name.clone(), payload.clone()));
        Ok(())
    }

    async fn rollback_backing(
        &self,
        node: &EnvironmentNode,
        payload: &VolumeSnapshot,
    ) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .rollbacks
            .push((node.name.clone(), payload.clone()));
        Ok(())
    }

    async fn drop_snapshot(&self, node: &EnvironmentNode, payload: &VolumeSnapshot) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .dropped_snapshots
            .push((node.name.clone(), payload.clone()));
        Ok(())
    }

    async fn down_resource(&self, node: &EnvironmentNode, resource: &str) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .downs
            .push((node.name.clone(), resource.to_string()));
        Ok(())
    }

    async fn up_resource(&self, node: &EnvironmentNode, resource: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_up.as_deref() == Some(node.name.as_str()) {
            return Err(DrbdError::Conflict(format!(
                "\"{}\" would not come back up",
                node.name
            )));
        }
        inner.ups.push((node.name.clone(), resource.to_string()));
        Ok(())
    }

    async fn invalidate_remote(&self, node: &EnvironmentNode, resource: &str) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .invalidated
            .push((node.name.clone(), resource.to_string()));
        Ok(())
    }

    async fn reconnect(&self, node: &EnvironmentNode, resource: &str, discard: bool) -> Result<()> {
        self.inner.lock().unwrap().reconnects.push((
            node.name.clone(),
            resource.to_string(),
            discard,
        ));
        Ok(())
    }

    async fn apply_policy(&self, node: &EnvironmentNode, payload: &VolumeApply) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .applied
            .push((node.name.clone(), payload.clone()));
        Ok(())
    }
}
