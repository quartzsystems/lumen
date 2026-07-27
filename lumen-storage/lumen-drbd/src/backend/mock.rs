//! An in-memory DRBD: resources registered, statuses simulated, and every
//! operation recorded instead of run — nothing touches the machine the tests
//! run on.
//!
//! Compiled unconditionally — the control plane's integration tests build
//! their `AppState` around this backend, and a `cfg(test)` item would be
//! invisible to them.

use std::sync::Mutex;

use async_trait::async_trait;

use super::DrbdBackend;
use crate::error::{DrbdError, Result};
use crate::state::{DeviceStatus, PeerDeviceStatus, PeerStatus, ResourceStatus};

#[derive(Debug, Default)]
struct Inner {
    resources: Vec<ResourceStatus>,
    fail_next: Option<String>,
    // What the node was asked to do, for assertions.
    written: Vec<(String, String)>,
    removed: Vec<String>,
    metadata_created: Vec<(String, usize)>,
    up: Vec<String>,
    down: Vec<String>,
    primed: Vec<String>,
    resized: Vec<String>,
    two_primaries: Vec<(String, bool)>,
    invalidated: Vec<String>,
    reconnected: Vec<(String, bool)>,
    adjusted: Vec<String>,
}

#[derive(Default)]
pub struct MockBackend {
    inner: Mutex<Inner>,
}

impl MockBackend {
    /// A node with no replicated volumes — every fresh install.
    pub fn appliance() -> Self {
        MockBackend::default()
    }

    /// Register a simulated resource status, as `drbdadm up` forming a
    /// healthy pair would eventually produce.
    pub fn register(&self, status: ResourceStatus) {
        let mut inner = self.inner.lock().unwrap();
        inner.resources.retain(|r| r.name != status.name);
        inner.resources.push(status);
    }

    /// Make one resource report a resync in flight at `percent`, with the
    /// counters a rate can be derived from.
    pub fn with_syncing(self, resource: &str, peer: &str, percent: f64, received: u64) -> Self {
        self.register(syncing_resource(resource, peer, percent, received));
        self
    }

    /// A healthy pair for one resource.
    pub fn with_healthy(self, resource: &str, peer: &str, minor: u32) -> Self {
        self.register(healthy_resource(resource, peer, minor));
        self
    }

    /// A three-replica volume that lost its own quorum: suspended.
    pub fn with_no_quorum(self, resource: &str, peers: &[&str]) -> Self {
        self.register(no_quorum_resource(resource, peers));
        self
    }

    /// The next call fails with this reason, once.
    pub fn fail_next(&self, reason: impl Into<String>) {
        self.inner.lock().unwrap().fail_next = Some(reason.into());
    }

    // --- assertion accessors ------------------------------------------------

    pub fn written(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().written.clone()
    }

    pub fn removed(&self) -> Vec<String> {
        self.inner.lock().unwrap().removed.clone()
    }

    pub fn metadata_created(&self) -> Vec<(String, usize)> {
        self.inner.lock().unwrap().metadata_created.clone()
    }

    pub fn brought_up(&self) -> Vec<String> {
        self.inner.lock().unwrap().up.clone()
    }

    pub fn taken_down(&self) -> Vec<String> {
        self.inner.lock().unwrap().down.clone()
    }

    pub fn primed(&self) -> Vec<String> {
        self.inner.lock().unwrap().primed.clone()
    }

    pub fn resized(&self) -> Vec<String> {
        self.inner.lock().unwrap().resized.clone()
    }

    pub fn two_primaries(&self) -> Vec<(String, bool)> {
        self.inner.lock().unwrap().two_primaries.clone()
    }

    pub fn invalidated(&self) -> Vec<String> {
        self.inner.lock().unwrap().invalidated.clone()
    }

    /// Every reconnect, in order: (resource, discarded).
    pub fn reconnected(&self) -> Vec<(String, bool)> {
        self.inner.lock().unwrap().reconnected.clone()
    }

    pub fn adjusted(&self) -> Vec<String> {
        self.inner.lock().unwrap().adjusted.clone()
    }

    fn take_failure(&self) -> Option<DrbdError> {
        self.inner
            .lock()
            .unwrap()
            .fail_next
            .take()
            .map(|reason| DrbdError::Backend(anyhow::anyhow!(reason)))
    }
}

/// A healthy two-sided resource as this node sees it.
pub fn healthy_resource(resource: &str, peer: &str, minor: u32) -> ResourceStatus {
    ResourceStatus {
        name: resource.into(),
        role: "Secondary".into(),
        devices: vec![DeviceStatus {
            minor,
            disk_state: "UpToDate".into(),
            quorum: true,
        }],
        connections: vec![PeerStatus {
            name: peer.into(),
            connection_state: "Connected".into(),
            peer_role: "Secondary".into(),
            devices: vec![PeerDeviceStatus {
                replication_state: "Established".into(),
                peer_disk_state: "UpToDate".into(),
                percent_in_sync: 100.0,
                ..PeerDeviceStatus::default()
            }],
        }],
        ..ResourceStatus::default()
    }
}

/// A resync in flight, this node the target.
pub fn syncing_resource(resource: &str, peer: &str, percent: f64, received: u64) -> ResourceStatus {
    ResourceStatus {
        name: resource.into(),
        role: "Secondary".into(),
        devices: vec![DeviceStatus {
            minor: 1,
            disk_state: "Inconsistent".into(),
            quorum: true,
        }],
        connections: vec![PeerStatus {
            name: peer.into(),
            connection_state: "Connected".into(),
            peer_role: "Primary".into(),
            devices: vec![PeerDeviceStatus {
                replication_state: "SyncTarget".into(),
                peer_disk_state: "UpToDate".into(),
                percent_in_sync: percent,
                received,
                ..PeerDeviceStatus::default()
            }],
        }],
        ..ResourceStatus::default()
    }
}

/// A quorum volume that lost its majority.
pub fn no_quorum_resource(resource: &str, peers: &[&str]) -> ResourceStatus {
    ResourceStatus {
        name: resource.into(),
        role: "Primary".into(),
        suspended: true,
        suspended_quorum: true,
        devices: vec![DeviceStatus {
            minor: 1,
            disk_state: "UpToDate".into(),
            quorum: false,
        }],
        connections: peers
            .iter()
            .map(|peer| PeerStatus {
                name: (*peer).into(),
                connection_state: "Connecting".into(),
                peer_role: "Unknown".into(),
                devices: vec![PeerDeviceStatus {
                    replication_state: "Off".into(),
                    peer_disk_state: "DUnknown".into(),
                    percent_in_sync: 100.0,
                    ..PeerDeviceStatus::default()
                }],
            })
            .collect(),
        ..ResourceStatus::default()
    }
}

#[async_trait]
impl DrbdBackend for MockBackend {
    async fn status(&self) -> Result<Vec<ResourceStatus>> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        Ok(self.inner.lock().unwrap().resources.clone())
    }

    async fn write_resource(&self, resource: &str, content: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner
            .lock()
            .unwrap()
            .written
            .push((resource.to_string(), content.to_string()));
        Ok(())
    }

    async fn remove_resource_file(&self, resource: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner
            .lock()
            .unwrap()
            .removed
            .push(resource.to_string());
        Ok(())
    }

    async fn create_metadata(&self, resource: &str, peers: usize) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner
            .lock()
            .unwrap()
            .metadata_created
            .push((resource.to_string(), peers));
        Ok(())
    }

    async fn up(&self, resource: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner.lock().unwrap().up.push(resource.to_string());
        Ok(())
    }

    async fn down(&self, resource: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        inner.down.push(resource.to_string());
        inner.resources.retain(|r| r.name != resource);
        Ok(())
    }

    async fn skip_initial_sync(&self, resource: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner.lock().unwrap().primed.push(resource.to_string());
        Ok(())
    }

    async fn resize(&self, resource: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner
            .lock()
            .unwrap()
            .resized
            .push(resource.to_string());
        Ok(())
    }

    async fn set_two_primaries(&self, resource: &str, allow: bool) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner
            .lock()
            .unwrap()
            .two_primaries
            .push((resource.to_string(), allow));
        Ok(())
    }

    async fn invalidate_remote(&self, resource: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner
            .lock()
            .unwrap()
            .invalidated
            .push(resource.to_string());
        Ok(())
    }

    async fn read_resource(&self, resource: &str) -> Result<String> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .written
            .iter()
            .rev()
            .find(|(name, _)| name == resource)
            .map(|(_, content)| content.clone())
            .unwrap_or_else(|| {
                format!("resource \"{resource}\" {{ shared-secret \"mock-secret\"; }}")
            }))
    }

    async fn adjust(&self, resource: &str) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        self.inner
            .lock()
            .unwrap()
            .adjusted
            .push(resource.to_string());
        Ok(())
    }

    async fn reconnect(&self, resource: &str, discard: bool) -> Result<()> {
        if let Some(err) = self.take_failure() {
            return Err(err);
        }
        let mut inner = self.inner.lock().unwrap();
        inner.reconnected.push((resource.to_string(), discard));
        // A reconnect leaves StandAlone behind: the simulated resource
        // reports Connected again.
        if let Some(status) = inner.resources.iter_mut().find(|r| r.name == resource) {
            for peer in &mut status.connections {
                peer.connection_state = "Connected".into();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_appliance_has_no_resources_and_records_what_it_is_asked() {
        let backend = MockBackend::appliance();
        assert!(backend.status().await.unwrap().is_empty());
        backend
            .write_resource("alpha-v0", "resource…")
            .await
            .unwrap();
        backend.up("alpha-v0").await.unwrap();
        backend.register(healthy_resource("alpha-v0", "alpha-2", 1));
        assert_eq!(backend.status().await.unwrap().len(), 1);
        backend.down("alpha-v0").await.unwrap();
        assert!(
            backend.status().await.unwrap().is_empty(),
            "down removes it"
        );
        assert_eq!(backend.brought_up(), vec!["alpha-v0"]);
    }

    #[tokio::test]
    async fn a_failure_is_injected_once() {
        let backend = MockBackend::appliance();
        backend.fail_next("the kernel module is not loaded");
        assert!(backend.status().await.is_err());
        assert!(backend.status().await.is_ok());
    }
}
