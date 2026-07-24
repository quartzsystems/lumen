//! On-disk state for the networking domain: three small JSON files under the
//! control plane's state dir (`LUMEN_CP_STATE_DIR`, which `StateDirectory=`
//! already makes writable under `ProtectSystem=strict`).
//!
//! No database, by design — the whole networking state of one appliance is a
//! few kilobytes of JSON, and a database would be one more thing to keep
//! consistent with what NetworkManager actually did.
//!
//!   desired.json     the configuration the operator has committed
//!   pending.json     the staged target, absent when nothing is staged
//!   checkpoint.json  the outstanding rollback point, absent when none is

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::backend::CheckpointId;
use crate::error::Result;
use crate::model::NetworkDesiredState;

/// A checkpoint the daemon is waiting to have confirmed. Persisted so a
/// control-plane restart inside the window adopts it instead of orphaning it —
/// NetworkManager holds the actual rollback out of process, so the change is
/// still being watched even while we are not running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: CheckpointId,
    /// Absolute deadline, seconds since the epoch. The console counts down to
    /// this rather than to a duration it was handed, so a slow request or a
    /// page reload cannot drift the number it shows.
    pub confirm_deadline: u64,
    /// The window in force, after any extensions.
    pub rollback_secs: u32,
    /// The state that was applied, so a confirm can commit exactly it.
    pub target: NetworkDesiredState,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// `state_dir` is the control plane's state dir; networking gets a
    /// subdirectory of its own.
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("network"),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn read<T: for<'de> Deserialize<'de>>(&self, name: &str) -> Result<Option<T>> {
        let path = self.path(name);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(anyhow::Error::from(err)
                    .context(format!("reading {}", path.display()))
                    .into())
            }
        };
        // A corrupt file must not wedge the daemon at boot: log it and start
        // from nothing, which re-seeds from the box itself.
        match serde_json::from_str(&text) {
            Ok(value) => Ok(Some(value)),
            Err(err) => {
                tracing::error!(path = %path.display(), "ignoring unreadable state: {err}");
                Ok(None)
            }
        }
    }

    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.path(name);
        // Write-then-rename: a power cut during an apply must not leave half
        // a JSON document behind.
        let temporary = path.with_extension("json.new");
        let text = serde_json::to_string_pretty(value).context("serializing network state")?;
        fs::write(&temporary, text).with_context(|| format!("writing {}", temporary.display()))?;
        fs::rename(&temporary, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.path(name)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Both documents go through `migrate` on the way out, so nothing above
    /// the store ever sees the older shape.
    fn read_state(&self, name: &str) -> Result<Option<NetworkDesiredState>> {
        let mut state: Option<NetworkDesiredState> = self.read(name)?;
        if let Some(state) = state.as_mut() {
            state.migrate();
        }
        Ok(state)
    }

    pub fn desired(&self) -> Result<Option<NetworkDesiredState>> {
        self.read_state("desired.json")
    }

    pub fn set_desired(&self, state: &NetworkDesiredState) -> Result<()> {
        self.write("desired.json", state)
    }

    pub fn pending(&self) -> Result<Option<NetworkDesiredState>> {
        self.read_state("pending.json")
    }

    pub fn set_pending(&self, state: &NetworkDesiredState) -> Result<()> {
        self.write("pending.json", state)
    }

    pub fn clear_pending(&self) -> Result<()> {
        self.remove("pending.json")
    }

    pub fn checkpoint(&self) -> Result<Option<CheckpointRecord>> {
        let mut record: Option<CheckpointRecord> = self.read("checkpoint.json")?;
        if let Some(record) = record.as_mut() {
            record.target.migrate();
        }
        Ok(record)
    }

    pub fn set_checkpoint(&self, record: &CheckpointRecord) -> Result<()> {
        self.write("checkpoint.json", record)
    }

    pub fn clear_checkpoint(&self) -> Result<()> {
        self.remove("checkpoint.json")
    }
}

/// Seconds since the epoch.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IpConfig, ManagementRef, Nic};

    fn temp_dir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "lumen-net-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        base
    }

    fn sample() -> NetworkDesiredState {
        NetworkDesiredState {
            nics: vec![Nic {
                name: "nic0".into(),
                ip: IpConfig::Dhcp,
                ..Nic::default()
            }],
            management: ManagementRef {
                link: "nic0".into(),
                ..ManagementRef::default()
            },
            ..NetworkDesiredState::default()
        }
    }

    #[test]
    fn round_trips_and_reports_absence() {
        let dir = temp_dir();
        let store = Store::new(&dir);
        assert_eq!(store.desired().unwrap(), None);
        assert_eq!(store.pending().unwrap(), None);
        assert_eq!(store.checkpoint().unwrap(), None);

        store.set_desired(&sample()).unwrap();
        assert_eq!(store.desired().unwrap(), Some(sample()));

        store.set_pending(&sample()).unwrap();
        store.clear_pending().unwrap();
        assert_eq!(store.pending().unwrap(), None);

        let record = CheckpointRecord {
            id: CheckpointId("/checkpoint/1".into()),
            confirm_deadline: now_unix() + 60,
            rollback_secs: 60,
            target: sample(),
        };
        store.set_checkpoint(&record).unwrap();
        assert_eq!(store.checkpoint().unwrap(), Some(record));
        store.clear_checkpoint().unwrap();
        assert_eq!(store.checkpoint().unwrap(), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_file_reads_as_absent_rather_than_wedging_the_daemon() {
        let dir = temp_dir();
        let store = Store::new(&dir);
        store.set_desired(&sample()).unwrap();
        fs::write(dir.join("network/desired.json"), "{ not json").unwrap();
        assert_eq!(store.desired().unwrap(), None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The migration runs on the way out of the store, so nothing above it
    /// ever has to know the older shape existed.
    #[test]
    fn a_document_written_by_an_older_appliance_is_migrated_on_read() {
        let dir = temp_dir();
        let store = Store::new(&dir);
        fs::create_dir_all(dir.join("network")).unwrap();
        fs::write(
            dir.join("network/desired.json"),
            r#"{"nics":[{"name":"nic0"}],
                "management":{"link":"nic0","ip":{"mode":"dhcp"}}}"#,
        )
        .unwrap();
        let desired = store.desired().unwrap().expect("it parses");
        assert_eq!(desired.ip_of("nic0"), IpConfig::Dhcp);
        assert!(desired.management.legacy_ip.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_something_that_is_not_there_is_not_an_error() {
        let dir = temp_dir();
        let store = Store::new(&dir);
        store.clear_pending().unwrap();
        store.clear_checkpoint().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
