//! What the virtualization domain needs from the hypervisor, and nothing more.
//!
//! Three implementations, exactly as `lumen_net::backend`: [`libvirt`] drives
//! the real hypervisor, [`mock`] is an in-memory model used by every test in
//! this crate and every control-plane API test, and [`unavailable`] reports
//! why there is no hypervisor rather than panicking.
//!
//! Like `lumen_net`'s, the mock is compiled unconditionally and exported
//! rather than hidden behind `#[cfg(test)]`: a `cfg(test)` item is invisible to
//! another crate's integration tests, and the control plane's tests need it.
//!
//! ## Why this surface and no more
//!
//! Everything above this module works in domain documents and machine
//! configurations. The trait is the seam where that turns into hypervisor
//! calls, and it is deliberately narrow: connect, list, look up, define,
//! undefine, the five lifecycle verbs, the autostart flag, and the four
//! changes that can reach a running machine. Whether the binding underneath is
//! the published one or an in-tree one is invisible from here — see
//! docs/compute.md for which was chosen and why.
//!
//! ## Stored versus running
//!
//! There is exactly one way a change is persisted: [`VirtBackend::define`]
//! with the whole document. Nothing else writes the stored configuration.
//!
//! The `*_live` calls are purely additive on top of that: they ask the
//! hypervisor to also apply the change to the machine that is running right
//! now, and they return the hypervisor's own error when it cannot. **The
//! caller decides what to do with that failure** — the service records the
//! change as waiting for a restart. Keeping that policy above the trait is
//! what makes the mock and the real backend answer identically, and keeping
//! persistence in one call is what stops a device being added twice.

pub mod libvirt;
pub mod mock;
pub mod unavailable;

use async_trait::async_trait;

use crate::domain_caps::CpuModels;
use crate::error::Result;
use crate::state::{HostInfo, ObservedDomain};

#[async_trait]
pub trait VirtBackend: Send + Sync {
    /// Processors, memory, and the hypervisor's own version.
    async fn host(&self) -> Result<HostInfo>;

    /// The processor models this node can actually run, as the hypervisor
    /// computes them from the host silicon and the emulator.
    ///
    /// Here rather than in a table of our own for the reason in
    /// [`crate::domain_caps`]: a model this box cannot run must be visibly not
    /// offered, not accepted and then refused at start time.
    async fn cpu_models(&self) -> Result<CpuModels>;

    /// Every domain this node holds, running or not, with its document.
    async fn domains(&self) -> Result<Vec<ObservedDomain>>;

    /// One domain by name.
    async fn domain(&self, name: &str) -> Result<ObservedDomain>;

    /// Define a persistent domain, or replace the definition of one that
    /// already exists. Never creates a transient domain — a transient one
    /// disappears when it stops, and a machine that vanishes because somebody
    /// shut it down is not a machine.
    async fn define(&self, xml: &str) -> Result<()>;

    /// Remove the definition. The machine must not be running.
    async fn undefine(&self, name: &str) -> Result<()>;

    /// Change a machine's name, keeping its identity and its disks. The
    /// hypervisor allows this only while the machine is stopped, which is why
    /// the service refuses a rename of a running one rather than pretending it
    /// can wait for a restart.
    async fn rename(&self, name: &str, new_name: &str) -> Result<()>;

    async fn start(&self, name: &str) -> Result<()>;

    /// Ask the guest to shut down and let it decide when. Needs power
    /// management in the guest; a guest that ignores it simply keeps running.
    async fn shutdown(&self, name: &str) -> Result<()>;

    /// Stop it now, the equivalent of pulling the power.
    async fn destroy(&self, name: &str) -> Result<()>;

    /// Ask the guest to restart itself.
    async fn reboot(&self, name: &str) -> Result<()>;

    /// Restart it now, without asking the guest.
    async fn reset(&self, name: &str) -> Result<()>;

    /// The autostart flag — where "start on boot" actually lives. It is not
    /// part of the domain document, so it is read and written on its own.
    async fn set_autostart(&self, name: &str, on: bool) -> Result<()>;

    /// Attach a device to the machine that is running right now. The stored
    /// configuration is `define`'s job; this only reaches the live machine.
    async fn attach_device_live(&self, name: &str, device_xml: &str) -> Result<()>;

    /// Detach a device from the machine that is running right now.
    async fn detach_device_live(&self, name: &str, device_xml: &str) -> Result<()>;

    /// Change the memory of the running machine. The hypervisor allows this
    /// only within the maximum the machine was started with, and says so when
    /// it does not — which is exactly the "waiting for a restart" answer.
    async fn set_memory_live(&self, name: &str, mib: u64) -> Result<()>;

    /// Change the processor count of the running machine, with the same
    /// constraint as memory.
    async fn set_vcpus_live(&self, name: &str, count: u32) -> Result<()>;

    /// Replace the metadata of the *running* machine only, so it disappears
    /// when the machine stops. Used for the start time an uptime is measured
    /// from; see [`crate::domain_xml::live_metadata`].
    async fn set_live_metadata(&self, name: &str, metadata_xml: &str) -> Result<()>;

    /// Live-migrate the running machine to the hypervisor at `destination` —
    /// a `qemu+tcp://<core-address>/system` URI on the cluster's Core
    /// network. Peer-to-peer, persistent on the destination, undefined here:
    /// when this answers Ok the machine has one home and it is not this
    /// node. The two-primaries window around this call is the service's
    /// business, never the backend's.
    async fn migrate(&self, name: &str, destination: &str) -> Result<()>;

    /// The document of the machine as it is **running**, not as it is stored.
    ///
    /// Every other read in this trait deliberately answers with the stored
    /// document, because that is the one the console edits and the difference
    /// between the two is what "waiting for a restart" means. The console
    /// viewer is the one caller that needs the opposite: a socket is a fact
    /// about the process that is listening, and the stored document describes a
    /// machine that may not have started yet with those devices — or, for a
    /// machine defined before this appliance put a screen on one, may not
    /// describe a screen at all.
    async fn live_xml(&self, name: &str) -> Result<String>;

    /// Ask the guest's own agent to do something, and hand back its reply.
    ///
    /// `command` is one QEMU guest agent request as JSON — `{"execute": …,
    /// "arguments": {…}}` — and the answer is the JSON it sends back. Nothing
    /// here interprets either: the caller composes the request and reads the
    /// reply, because the agent's vocabulary is the agent's and not something
    /// this trait should mirror.
    ///
    /// This is the one call in the trait that depends on software *inside* the
    /// guest. A machine without `qemu-guest-agent` running is not broken and is
    /// not a hypervisor failure — it simply cannot be asked, and the refusal
    /// says so in those words rather than in the hypervisor's.
    async fn guest_agent(&self, name: &str, command: &str) -> Result<String>;
}
