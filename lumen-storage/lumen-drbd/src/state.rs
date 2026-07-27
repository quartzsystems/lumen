//! Observed replication state: what DRBD actually reports.
//!
//! Filled from `drbdsetup status --json` — the machine-readable form; the
//! column output is for eyes and changes between releases — and deliberately
//! independent of the volume records: a resource DRBD still carries after
//! its record is gone shows up here, and a record whose resource is down
//! shows up as exactly that. Lumen presents; it does not second-guess.
//!
//! One honesty rule worth its sentence: a node only sees the resources it
//! participates in. From a member that holds no replica of a volume, DRBD
//! reports nothing — so the view marks those replicas unknown rather than
//! guessing, the same way an unreachable cluster is presented, not dropped.

use serde::{Deserialize, Serialize};

use crate::error::{DrbdError, Result};

/// One resource as this node sees it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceStatus {
    pub name: String,
    /// This node's role: `Primary` while something holds the device open —
    /// auto-promote at work — `Secondary` otherwise.
    pub role: String,
    /// I/O is suspended for any reason.
    #[serde(default)]
    pub suspended: bool,
    /// Suspended specifically because the volume lost its own quorum — the
    /// three-replica regime's integrity stop.
    #[serde(default, rename = "suspended-quorum")]
    pub suspended_quorum: bool,
    /// Suspended by the fencing handler — the two-node regime's integrity
    /// stop while the peer is being outdated through Pacemaker.
    #[serde(default, rename = "suspended-fencing")]
    pub suspended_fencing: bool,
    #[serde(default)]
    pub devices: Vec<DeviceStatus>,
    #[serde(default)]
    pub connections: Vec<PeerStatus>,
}

impl ResourceStatus {
    pub fn peer(&self, node: &str) -> Option<&PeerStatus> {
        self.connections.iter().find(|p| p.name == node)
    }

    /// The local disk state — every Lumen resource has exactly one volume.
    pub fn disk_state(&self) -> Option<&str> {
        self.devices.first().map(|d| d.disk_state.as_str())
    }
}

/// The one device under a resource.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceStatus {
    #[serde(default)]
    pub minor: u32,
    /// `UpToDate`, `Inconsistent`, `Outdated`, `Diskless`, …
    #[serde(default, rename = "disk-state")]
    pub disk_state: String,
    /// Whether this device currently has quorum. Meaningful only when the
    /// resource carries the quorum options; DRBD reports `true` otherwise.
    #[serde(default = "default_true")]
    pub quorum: bool,
}

fn default_true() -> bool {
    true
}

/// One peer of a resource, from this node's side of the connection.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerStatus {
    /// The peer's node name.
    #[serde(default)]
    pub name: String,
    /// `Connected`, `Connecting`, `StandAlone`, …
    #[serde(default, rename = "connection-state")]
    pub connection_state: String,
    #[serde(default, rename = "peer-role")]
    pub peer_role: String,
    #[serde(default, rename = "peer_devices")]
    pub devices: Vec<PeerDeviceStatus>,
}

impl PeerStatus {
    pub fn device(&self) -> Option<&PeerDeviceStatus> {
        self.devices.first()
    }
}

/// The peer's side of the one device.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerDeviceStatus {
    /// `Established` when in sync, `SyncSource`/`SyncTarget` during a
    /// resync.
    #[serde(default, rename = "replication-state")]
    pub replication_state: String,
    #[serde(default, rename = "peer-disk-state")]
    pub peer_disk_state: String,
    /// How much of the device the peer agrees about, 0–100.
    #[serde(default = "default_hundred", rename = "percent-in-sync")]
    pub percent_in_sync: f64,
    /// KiB received from this peer since the connection was made — the
    /// counter successive snapshots turn into a resync rate.
    #[serde(default)]
    pub received: u64,
    /// KiB sent to this peer since the connection was made.
    #[serde(default)]
    pub sent: u64,
    /// KiB this side knows the peer does not have.
    #[serde(default, rename = "out-of-sync")]
    pub out_of_sync: u64,
}

fn default_hundred() -> f64 {
    100.0
}

impl PeerDeviceStatus {
    pub fn syncing(&self) -> bool {
        self.replication_state.starts_with("Sync") || self.replication_state.starts_with("Paused")
    }
}

/// Parse `drbdsetup status --json`: a JSON array, one entry per resource
/// this node participates in. An empty array — no resources at all — is a
/// node with no replicated volumes, not an error.
pub fn parse_status(json: &str) -> Result<Vec<ResourceStatus>> {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(trimmed).map_err(|err| {
        DrbdError::backend(anyhow::anyhow!(
            "drbdsetup answered something that is not its JSON: {err}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// drbd-utils 9.x on EL10: a healthy two-replica resource, this node
    /// Secondary, everything UpToDate. Trimmed to the fields the parser
    /// reads plus the neighbours it must ignore.
    const STATUS_HEALTHY: &str = r#"[
  {
    "name": "alpha-vm-101-disk-0",
    "node-id": 0,
    "role": "Secondary",
    "suspended": false,
    "suspended-user": false,
    "suspended-no-data": false,
    "suspended-fencing": false,
    "suspended-quorum": false,
    "force-io-failures": false,
    "write-ordering": "flush",
    "devices": [
      {
        "volume": 0,
        "minor": 1,
        "disk-state": "UpToDate",
        "client": false,
        "quorum": true,
        "size": 10486784,
        "read": 0,
        "written": 331776,
        "al-writes": 12,
        "bm-writes": 0,
        "upper-pending": 0,
        "lower-pending": 0
      }
    ],
    "connections": [
      {
        "peer-node-id": 1,
        "name": "alpha-2",
        "connection-state": "Connected",
        "congested": false,
        "peer-role": "Primary",
        "ap-in-flight": 0,
        "rs-in-flight": 0,
        "peer_devices": [
          {
            "volume": 0,
            "replication-state": "Established",
            "peer-disk-state": "UpToDate",
            "peer-client": false,
            "resync-suspended": "no",
            "received": 0,
            "sent": 331776,
            "out-of-sync": 0,
            "pending": 0,
            "unacked": 0,
            "has-sync-details": false,
            "has-online-verify-details": false,
            "percent-in-sync": 100.00
          }
        ]
      }
    ]
  }
]"#;

    /// The same resource mid-resync: this node is the SyncTarget at 42.7%,
    /// receiving from the peer.
    const STATUS_SYNCING: &str = r#"[
  {
    "name": "alpha-vm-101-disk-0",
    "node-id": 1,
    "role": "Secondary",
    "suspended": false,
    "devices": [
      { "volume": 0, "minor": 1, "disk-state": "Inconsistent", "quorum": true, "size": 10486784 }
    ],
    "connections": [
      {
        "peer-node-id": 0,
        "name": "alpha-1",
        "connection-state": "Connected",
        "peer-role": "Primary",
        "peer_devices": [
          {
            "volume": 0,
            "replication-state": "SyncTarget",
            "peer-disk-state": "UpToDate",
            "received": 4481024,
            "sent": 0,
            "out-of-sync": 6010880,
            "percent-in-sync": 42.70
          }
        ]
      }
    ]
  }
]"#;

    /// A three-replica quorum volume that lost its majority: I/O suspended,
    /// quorum gone, both peers unreachable.
    const STATUS_NO_QUORUM: &str = r#"[
  {
    "name": "beta-vm-7-disk-0",
    "node-id": 2,
    "role": "Primary",
    "suspended": true,
    "suspended-quorum": true,
    "devices": [
      { "volume": 0, "minor": 2, "disk-state": "UpToDate", "quorum": false, "size": 5242880 }
    ],
    "connections": [
      {
        "peer-node-id": 0,
        "name": "beta-1",
        "connection-state": "Connecting",
        "peer-role": "Unknown",
        "peer_devices": [
          { "volume": 0, "replication-state": "Off", "peer-disk-state": "DUnknown", "percent-in-sync": 100.00 }
        ]
      },
      {
        "peer-node-id": 1,
        "name": "beta-2",
        "connection-state": "Connecting",
        "peer-role": "Unknown",
        "peer_devices": [
          { "volume": 0, "replication-state": "Off", "peer-disk-state": "DUnknown", "percent-in-sync": 100.00 }
        ]
      }
    ]
  }
]"#;

    #[test]
    fn a_healthy_resource_parses_whole() {
        let resources = parse_status(STATUS_HEALTHY).unwrap();
        assert_eq!(resources.len(), 1);
        let res = &resources[0];
        assert_eq!(res.name, "alpha-vm-101-disk-0");
        assert_eq!(res.role, "Secondary");
        assert!(!res.suspended);
        assert_eq!(res.disk_state(), Some("UpToDate"));
        assert_eq!(res.devices[0].minor, 1);
        assert!(res.devices[0].quorum);

        let peer = res.peer("alpha-2").unwrap();
        assert_eq!(peer.connection_state, "Connected");
        assert_eq!(peer.peer_role, "Primary");
        let device = peer.device().unwrap();
        assert!(!device.syncing());
        assert_eq!(device.percent_in_sync, 100.0);
    }

    #[test]
    fn a_resync_reports_its_direction_progress_and_counters() {
        let resources = parse_status(STATUS_SYNCING).unwrap();
        let device = resources[0].peer("alpha-1").unwrap().device().unwrap();
        assert!(device.syncing());
        assert_eq!(device.replication_state, "SyncTarget");
        assert!((device.percent_in_sync - 42.70).abs() < 0.001);
        assert_eq!(device.received, 4_481_024);
        assert_eq!(device.out_of_sync, 6_010_880);
        assert_eq!(resources[0].disk_state(), Some("Inconsistent"));
    }

    #[test]
    fn a_lost_quorum_reads_as_suspended_with_the_reason() {
        let resources = parse_status(STATUS_NO_QUORUM).unwrap();
        let res = &resources[0];
        assert!(res.suspended && res.suspended_quorum);
        assert!(!res.devices[0].quorum);
        assert!(res
            .connections
            .iter()
            .all(|p| p.connection_state == "Connecting"));
    }

    #[test]
    fn no_resources_is_an_empty_answer_and_garbage_is_an_error() {
        assert_eq!(parse_status("[]").unwrap().len(), 0);
        assert_eq!(parse_status("").unwrap().len(), 0);
        assert!(parse_status("not json").is_err());
    }
}
