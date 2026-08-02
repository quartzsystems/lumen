//! What has been done on this node, and by whom.
//!
//! Every mutating VM route records one entry here after the domain has
//! answered — successes and refusals alike, because "who tried to stop this
//! machine and was told no" is as much a part of the history as the stop that
//! happened. The log is the console's Tasks table; it is not an audit
//! subsystem, and it deliberately stays small: a capped in-memory list backed
//! by one JSON-lines file in the state dir, so history survives a daemon
//! restart without dragging in a database.
//!
//! ## Not every entry is about a machine
//!
//! It began as one — the table is still a machine's history when read through
//! `/api/vms/{vmid}/tasks` — but an update installed on a node is exactly the
//! kind of thing an operator comes to a log to find, and it belongs to no
//! machine. So [`TaskRecord::vmid`] is optional, and [`TaskLog::event`] is how
//! something that happened *to the node* is written down.
//!
//! Records already on disk carry a `vmid` and keep meaning what they meant;
//! `serde(default)` is what lets an old file and a new one be read by the same
//! code, which matters because the file outlives the upgrade that changed it.

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How much history the log keeps, across all machines. Old entries fall off
/// the far end; nobody scrolls a thousand rows deep into a VM's past.
const CAPACITY: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Ok,
    Error,
}

/// One thing that was asked of this node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: u64,
    /// The machine it was about, when it was about one. `None` is an entry
    /// about the node itself — an update installed, a set of packages that
    /// would not resolve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmid: Option<u32>,
    /// The verb, as the API spells it: `start`, `stop`, `update`, …
    pub action: String,
    /// One sentence for the table, describing what was asked.
    pub detail: String,
    /// Who asked, as the Proxmox-style principal `user@realm`.
    pub user: String,
    /// When, in unix seconds.
    pub time: u64,
    pub status: TaskStatus,
    /// The domain's own words for why it did not happen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

struct Inner {
    records: VecDeque<TaskRecord>,
    next_id: u64,
    /// Appends since the file was last rewritten whole. Once this reaches
    /// [`CAPACITY`] the file is compacted from memory, which bounds it at
    /// roughly twice the history actually kept.
    appended: usize,
}

/// The log itself. Constructed once and shared behind [`crate::AppState`].
pub struct TaskLog {
    /// Absent in tests, which want the recording without the disk.
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl TaskLog {
    /// Open the log at `path`, reading whatever history is already there. A
    /// file that is missing or partly unreadable starts the log from what
    /// could be read — history is not worth refusing to boot over.
    pub fn open(path: PathBuf) -> Self {
        let mut records = VecDeque::new();
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                if let Ok(record) = serde_json::from_str::<TaskRecord>(line) {
                    records.push_back(record);
                }
            }
        }
        while records.len() > CAPACITY {
            records.pop_front();
        }
        let next_id = records.iter().map(|r| r.id + 1).max().unwrap_or(1);
        Self {
            path: Some(path),
            inner: Mutex::new(Inner {
                records,
                next_id,
                appended: 0,
            }),
        }
    }

    /// A log that remembers but never writes, for tests.
    pub fn ephemeral() -> Self {
        Self {
            path: None,
            inner: Mutex::new(Inner {
                records: VecDeque::new(),
                next_id: 1,
                appended: 0,
            }),
        }
    }

    /// Record one action against a machine. `error` carries the domain's
    /// message when it was refused; `None` means it happened.
    pub fn record(
        &self,
        vmid: u32,
        action: &str,
        detail: String,
        user: String,
        error: Option<String>,
    ) {
        self.write(Some(vmid), action, detail, user, error);
    }

    /// Record one thing that happened to the node rather than to a machine.
    ///
    /// Same log, same table, same window: an operator asking "what happened on
    /// this node" is asking one question, and answering it from two lists they
    /// have to interleave by hand would be the console making them do the sort.
    pub fn event(&self, action: &str, detail: String, user: String, error: Option<String>) {
        self.write(None, action, detail, user, error);
    }

    fn write(
        &self,
        vmid: Option<u32>,
        action: &str,
        detail: String,
        user: String,
        error: Option<String>,
    ) {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut inner = self.inner.lock().expect("task log lock poisoned");
        let record = TaskRecord {
            id: inner.next_id,
            vmid,
            action: action.to_string(),
            detail,
            user,
            time,
            status: if error.is_none() {
                TaskStatus::Ok
            } else {
                TaskStatus::Error
            },
            error,
        };
        inner.next_id += 1;
        inner.records.push_back(record.clone());
        while inner.records.len() > CAPACITY {
            inner.records.pop_front();
        }
        inner.appended += 1;

        // A failure to persist must not fail the action it describes — the
        // machine has already done the thing. Complain and carry on.
        if let Some(path) = &self.path {
            let result = if inner.appended >= CAPACITY {
                inner.appended = 0;
                rewrite(path, &inner.records)
            } else {
                append(path, &record)
            };
            if let Err(err) = result {
                tracing::warn!(
                    "could not persist the task log at {}: {err}",
                    path.display()
                );
            }
        }
    }

    /// One machine's history, newest first.
    pub fn for_vm(&self, vmid: u32) -> Vec<TaskRecord> {
        let inner = self.inner.lock().expect("task log lock poisoned");
        inner
            .records
            .iter()
            .filter(|record| record.vmid == Some(vmid))
            .rev()
            .cloned()
            .collect()
    }

    /// The whole node's history, newest first, capped at `limit`.
    ///
    /// What the dashboard's log panel reads. Unlike [`Self::for_vm`] this one
    /// takes a limit, because the caller is showing a window onto the log
    /// rather than the whole of one machine's past — and [`CAPACITY`] entries
    /// is more than any panel wants to be handed.
    pub fn recent(&self, limit: usize) -> Vec<TaskRecord> {
        let inner = self.inner.lock().expect("task log lock poisoned");
        inner.records.iter().rev().take(limit).cloned().collect()
    }
}

fn append(path: &PathBuf, record: &TaskRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")
}

fn rewrite(path: &PathBuf, records: &VecDeque<TaskRecord>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    for record in records {
        text.push_str(&serde_json::to_string(record).map_err(std::io::Error::other)?);
        text.push('\n');
    }
    fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_filters_by_machine() {
        let log = TaskLog::ephemeral();
        log.record(
            100,
            "start",
            "Start the machine".into(),
            "root@lumen".into(),
            None,
        );
        log.record(
            101,
            "stop",
            "Stop the machine".into(),
            "root@lumen".into(),
            None,
        );
        log.record(
            100,
            "reset",
            "Reset the machine".into(),
            "root@lumen".into(),
            Some("The machine is not running.".into()),
        );

        let tasks = log.for_vm(100);
        assert_eq!(tasks.len(), 2);
        // Newest first.
        assert_eq!(tasks[0].action, "reset");
        assert_eq!(tasks[0].status, TaskStatus::Error);
        assert_eq!(
            tasks[0].error.as_deref(),
            Some("The machine is not running.")
        );
        assert_eq!(tasks[1].action, "start");
        assert_eq!(tasks[1].status, TaskStatus::Ok);
        assert!(log.for_vm(999).is_empty());

        // The node-wide view is every machine's history in one sequence,
        // newest first, and the limit cuts from the old end rather than the
        // new one.
        let recent = log.recent(10);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].action, "reset");
        assert_eq!(recent[2].action, "start");
        assert_eq!(log.recent(2).len(), 2);
        assert_eq!(log.recent(2)[0].action, "reset");
        assert!(log.recent(0).is_empty());
    }

    #[test]
    fn an_event_belongs_to_the_node_and_not_to_a_machine() {
        let log = TaskLog::ephemeral();
        log.record(100, "start", "Start the machine".into(), "root@lumen".into(), None);
        log.event(
            "update",
            "Installed 12 updates".into(),
            "root@lumen".into(),
            None,
        );

        // It is in the node's history, at the top, because it is the newest
        // thing that happened.
        let recent = log.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].action, "update");
        assert_eq!(recent[0].vmid, None);

        // And in no machine's, because it was about none of them. A machine's
        // Tasks table must not grow rows it cannot explain.
        assert_eq!(log.for_vm(100).len(), 1);
        assert_eq!(log.for_vm(100)[0].action, "start");
    }

    #[test]
    fn history_survives_a_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "lumen-cp-tasklog-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("vm-tasks.jsonl");

        let log = TaskLog::open(path.clone());
        log.record(
            100,
            "start",
            "Start the machine".into(),
            "root@lumen".into(),
            None,
        );
        drop(log);

        let reopened = TaskLog::open(path);
        let tasks = reopened.for_vm(100);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].action, "start");
        // A fresh record continues the identifier sequence rather than
        // colliding with what was loaded.
        reopened.record(
            100,
            "stop",
            "Stop the machine".into(),
            "root@lumen".into(),
            None,
        );
        let tasks = reopened.for_vm(100);
        assert!(tasks[0].id > tasks[1].id);

        let _ = fs::remove_dir_all(&dir);
    }
}
