//! Observed state: what the hypervisor says about the machines it holds.
//!
//! Deliberately separate from [`crate::model`]: a domain someone defined with
//! `virsh` still appears here, and so does the live vCPU and memory count of a
//! running machine, which no configuration can tell you.

use serde::{Deserialize, Serialize};

/// Where a machine is. A coarse projection of the hypervisor's own state
/// enumeration — the console shows what an operator can act on, not the full
/// ladder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainState {
    Running,
    /// Paused by an operator or by the hypervisor.
    Paused,
    /// Defined and not running.
    ShutOff,
    /// Stopped because the guest crashed.
    Crashed,
    /// Saved to disk and resumable.
    Suspended,
    #[default]
    Unknown,
}

impl DomainState {
    pub fn as_str(self) -> &'static str {
        match self {
            DomainState::Running => "running",
            DomainState::Paused => "paused",
            DomainState::ShutOff => "shut_off",
            DomainState::Crashed => "crashed",
            DomainState::Suspended => "suspended",
            DomainState::Unknown => "unknown",
        }
    }

    /// The guest has a processor executing right now, so a hardware change
    /// either applies live or waits for a restart.
    pub fn is_running(self) -> bool {
        matches!(self, DomainState::Running | DomainState::Paused)
    }

    /// Nothing is executing, so anything may be changed freely.
    pub fn is_off(self) -> bool {
        matches!(self, DomainState::ShutOff | DomainState::Crashed)
    }
}

/// What a running machine is actually using. Absent when it is not running.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainRuntime {
    pub vcpus: u32,
    pub memory_kib: u64,
    pub max_memory_kib: u64,
    /// Processor time consumed since the machine started, in nanoseconds.
    pub cpu_time_ns: u64,
}

/// One domain as the hypervisor reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedDomain {
    pub name: String,
    pub uuid: Option<String>,
    pub state: DomainState,
    /// Defined rather than transient. A transient domain disappears when it
    /// stops, so Lumen never creates one.
    pub persistent: bool,
    /// The hypervisor's autostart flag — where "start on boot" actually lives.
    /// It is not part of the domain document, so it is read and written on its
    /// own rather than rendered.
    pub autostart: bool,
    pub runtime: Option<DomainRuntime>,
    /// The **stored** domain document, even while the machine is running.
    ///
    /// A running machine has two: the one it booted with and the one it will
    /// boot with next. The console edits the second, so that is the one every
    /// view is built from; the difference between them is reported as changes
    /// waiting for a restart rather than hidden by showing the old document.
    pub xml: String,
    /// When the running machine was started, seconds since the epoch.
    ///
    /// Read from metadata the hypervisor holds for the running machine only,
    /// so it survives a control-plane restart and disappears by itself when
    /// the machine stops. `None` when the machine is not running, or when it
    /// was started by something other than Lumen.
    pub started_at: Option<u64>,
}

/// What the node itself has, for the checks that compare a machine against it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub node: String,
    /// Logical processors — the number a machine's vCPU count is measured
    /// against.
    pub cpus: u32,
    pub memory_mib: u64,
    /// What the hypervisor reports itself as, for the console's footer and for
    /// a support conversation.
    pub hypervisor_version: Option<String>,
}

/// Memory kept back for the node itself. A machine asking for more than the
/// host has, minus this, is refused: the hypervisor would accept the
/// definition and then fail to start it, or start it and take the node down
/// with it.
pub const HOST_MEMORY_RESERVE_MIB: u64 = 2048;

impl HostInfo {
    /// The largest a single machine may be.
    pub fn assignable_memory_mib(&self) -> u64 {
        self.memory_mib.saturating_sub(HOST_MEMORY_RESERVE_MIB)
    }
}

/// This node's name. Read the way `lumen_net` reads it — `ProtectKernelTunables`
/// makes `/proc/sys` read-only, not unreadable.
pub fn hostname() -> String {
    lumen_net::backend::nm::hostname()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_answers_the_two_questions_the_console_asks() {
        assert!(DomainState::Running.is_running());
        assert!(DomainState::Paused.is_running());
        assert!(!DomainState::ShutOff.is_running());
        assert!(DomainState::ShutOff.is_off());
        assert!(DomainState::Crashed.is_off());
        // Unknown is neither: a machine we cannot place is one we do not act
        // on, rather than one we assume is safe to change.
        assert!(!DomainState::Unknown.is_running());
        assert!(!DomainState::Unknown.is_off());
    }

    #[test]
    fn the_host_keeps_memory_back_for_itself() {
        let host = HostInfo {
            memory_mib: 16_384,
            ..HostInfo::default()
        };
        assert_eq!(host.assignable_memory_mib(), 16_384 - 2048);

        // A node with less memory than the reserve has none to assign, rather
        // than underflowing into an enormous number.
        let tiny = HostInfo {
            memory_mib: 512,
            ..HostInfo::default()
        };
        assert_eq!(tiny.assignable_memory_mib(), 0);
    }

    #[test]
    fn a_node_always_has_a_name() {
        assert!(!hostname().is_empty());
    }
}
