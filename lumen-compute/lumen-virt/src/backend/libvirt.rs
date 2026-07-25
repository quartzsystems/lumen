//! The real backend: the hypervisor's own library, over its local socket.
//!
//! ## Why this is safe under the control plane's hardening
//!
//! The daemon this talks to is privileged and runs in its own process, reached
//! over a socket under `/run`. That is the same arrangement the networking
//! domain has with the network daemon, and it has the same consequence:
//! `lumen-controlplane.service` keeps `ProtectSystem=strict`,
//! `ProtectKernelTunables=yes`, `PrivateTmp=yes`, and `ProtectHome=yes`
//! unchanged, because none of the privileged work happens in this process.
//! `ProtectSystem=strict` leaves `/run` writable, so the socket is reachable.
//! See docs/compute.md for the reproduction.
//!
//! ## Threads
//!
//! Every call into the library blocks, so each one runs on a blocking thread
//! rather than on the async runtime. The connection handle is shared across
//! those threads, which the library explicitly supports — see the safety note
//! on [`Handle`].

use std::sync::Arc;

use async_trait::async_trait;
use virt::connect::Connect;
use virt::domain::Domain;
use virt::error::{Error as LibvirtError, ErrorNumber};

use crate::backend::VirtBackend;
use crate::domain_caps::CpuModels;
use crate::error::{Result, VirtError};
use crate::state::{DomainRuntime, DomainState, HostInfo, ObservedDomain};

/// The local system hypervisor. Not the session one: a session connection
/// would run machines as an unprivileged user with no access to the host's
/// bridges or volumes.
pub const SYSTEM_URI: &str = "qemu:///system";

/// The running machine and nothing else. `define` is the only thing that
/// writes the stored configuration, so every live call carries exactly this —
/// which is what stops a device being added to the configuration twice.
const LIVE_ONLY: u32 = virt_sys::VIR_DOMAIN_AFFECT_LIVE;

/// A connection shared across blocking threads.
///
/// # Safety
///
/// The hypervisor's client library is thread safe, and a connection object may
/// be used concurrently from several threads: each API call takes the
/// connection's own lock internally. The Rust binding wraps a raw pointer and
/// therefore does not derive these automatically, so they are asserted here
/// rather than working around the library's own guarantee by opening a
/// connection per request.
struct Handle(Connect);

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

pub struct LibvirtBackend {
    handle: Arc<Handle>,
    node: String,
}

impl LibvirtBackend {
    /// Connect to the local hypervisor and check it can actually run a
    /// machine, or say why not.
    ///
    /// A failure here is not fatal to the control plane: `main` swaps in
    /// [`super::unavailable::UnavailableBackend`] and the console comes up
    /// reporting the reason, because an operator whose hypervisor is down — or
    /// whose node turns out not to have KVM at all, see `ensure_kvm` below —
    /// needs the console more than usual.
    pub async fn connect() -> Result<Self> {
        Self::connect_to(SYSTEM_URI).await
    }

    pub async fn connect_to(uri: &str) -> Result<Self> {
        let uri = uri.to_string();
        let handle = tokio::task::spawn_blocking(move || Connect::open(Some(&uri)))
            .await
            .map_err(|err| VirtError::Backend(anyhow::anyhow!("{err}")))?
            .map_err(|err| {
                VirtError::Backend(anyhow::anyhow!(
                    "could not reach the hypervisor: {}",
                    err.message()
                ))
            })?;
        let backend = Self {
            handle: Arc::new(Handle(handle)),
            node: crate::state::hostname(),
        };
        backend.ensure_kvm().await?;
        Ok(backend)
    }

    /// Refuse a node that cannot run a machine, at startup rather than at the
    /// first create.
    ///
    /// Reaching the hypervisor is not the same as being able to run a guest. A
    /// node whose processor virtualization is off in firmware — or a nested one
    /// whose host does not pass it through — has a perfectly healthy libvirt
    /// and no `/dev/kvm`, and answers every question the console asks at boot.
    /// Nothing notices until a create, and then the failure arrives in the
    /// worst possible shape: the disks are made first, so the hypervisor's
    /// refusal rolls a volume back and reports itself as an internal error
    /// with the reason only in the journal.
    ///
    /// Worse, the reason it reports is not the one an operator needs. Firmware
    /// selection is resolved against the machine types the domain's accelerator
    /// can provide, so a UEFI machine on a node without KVM fails with
    /// "Unable to find 'efi' firmware that is compatible with the current
    /// configuration" — which reads as a missing firmware package and is not.
    ///
    /// Asking for the KVM domain capabilities is how the hypervisor itself
    /// answers the question, and asking at startup turns all of that into one
    /// sentence on the page.
    async fn ensure_kvm(&self) -> Result<()> {
        self.call(|conn| conn.get_domain_capabilities(None, Some("x86_64"), None, Some("kvm"), 0))
            .await
            .map(|_| ())
            .map_err(|err| kvm_unavailable(&err.to_string()))
    }

    /// Run one blocking library call off the async runtime.
    async fn call<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connect) -> std::result::Result<T, LibvirtError> + Send + 'static,
        T: Send + 'static,
    {
        let handle = self.handle.clone();
        tokio::task::spawn_blocking(move || f(&handle.0))
            .await
            .map_err(|err| VirtError::Backend(anyhow::anyhow!("hypervisor call failed: {err}")))?
            .map_err(map_error)
    }

    /// Look one machine up and do something with it.
    async fn with_domain<T, F>(&self, name: &str, f: F) -> Result<T>
    where
        F: FnOnce(&Domain) -> std::result::Result<T, LibvirtError> + Send + 'static,
        T: Send + 'static,
    {
        let name = name.to_string();
        self.call(move |conn| {
            let domain = Domain::lookup_by_name(conn, &name)?;
            f(&domain)
        })
        .await
    }
}

/// Why this node cannot run machines, in the words an operator can act on.
///
/// The hypervisor's own sentence is kept — it names the emulator, which is
/// worth having — but on its own it says nothing about what to do next, and
/// the two things that cause it are firmware settings on the node and a host
/// that does not pass virtualization through to it.
fn kvm_unavailable(reason: &str) -> VirtError {
    VirtError::Backend(anyhow::anyhow!(
        "the hypervisor is running but KVM is not available ({reason}). \
         On hardware, virtualization has to be enabled in the node's firmware; \
         if this node is itself a virtual machine, its host has to pass \
         hardware virtualization through to it."
    ))
}

/// The hypervisor's own error, mapped onto the API's. A machine that is not
/// there is a 404, not a 500 — every other failure is the hypervisor's to
/// explain and its message is carried through verbatim.
fn map_error(err: LibvirtError) -> VirtError {
    match err.code() {
        ErrorNumber::NoDomain => VirtError::NotFound(format!(
            "No machine matching that name on this node ({}).",
            err.message()
        )),
        ErrorNumber::OperationInvalid => VirtError::Conflict(err.message().to_string()),
        _ => VirtError::Backend(anyhow::anyhow!("{}", err.message())),
    }
}

fn state_of(raw: virt_sys::virDomainState) -> DomainState {
    match raw {
        virt_sys::VIR_DOMAIN_RUNNING | virt_sys::VIR_DOMAIN_BLOCKED => DomainState::Running,
        virt_sys::VIR_DOMAIN_PAUSED => DomainState::Paused,
        // A machine mid-shutdown is still up as far as anything an operator
        // can do to it is concerned.
        virt_sys::VIR_DOMAIN_SHUTDOWN => DomainState::Running,
        virt_sys::VIR_DOMAIN_SHUTOFF => DomainState::ShutOff,
        virt_sys::VIR_DOMAIN_CRASHED => DomainState::Crashed,
        virt_sys::VIR_DOMAIN_PMSUSPENDED => DomainState::Suspended,
        _ => DomainState::Unknown,
    }
}

/// Everything one domain has to say, gathered in a single blocking call so a
/// listing is one trip rather than five per machine.
fn observe(domain: &Domain) -> std::result::Result<ObservedDomain, LibvirtError> {
    let name = domain.get_name()?;
    let info = domain.get_info()?;
    let state = state_of(info.state);
    let running = !matches!(state, DomainState::ShutOff | DomainState::Crashed);

    // Always the *stored* document, even while the machine is running: that is
    // the one the console edits, and the difference between it and what is
    // running is reported as changes waiting for a restart.
    let xml = domain.get_xml_desc(virt_sys::VIR_DOMAIN_XML_INACTIVE)?;

    // The start time lives in metadata attached to the running machine only,
    // so it is asked for separately and its absence is ordinary — a machine
    // somebody started with `virsh` has none.
    let started_at = running
        .then(|| {
            domain
                .get_metadata(
                    virt_sys::VIR_DOMAIN_METADATA_ELEMENT as i32,
                    Some(crate::domain_xml::LUMEN_NS),
                    virt_sys::VIR_DOMAIN_AFFECT_LIVE,
                )
                .ok()
                .and_then(|metadata| crate::domain_xml::started_from_metadata(&metadata))
        })
        .flatten();

    Ok(ObservedDomain {
        name,
        uuid: domain.get_uuid_string().ok(),
        state,
        persistent: domain.is_persistent().unwrap_or(true),
        autostart: domain.get_autostart().unwrap_or(false),
        runtime: running.then_some(DomainRuntime {
            vcpus: info.nr_virt_cpu,
            memory_kib: info.memory,
            max_memory_kib: info.max_mem,
            cpu_time_ns: info.cpu_time,
        }),
        xml,
        started_at,
    })
}

#[async_trait]
impl VirtBackend for LibvirtBackend {
    async fn cpu_models(&self) -> Result<CpuModels> {
        self.call(move |conn| {
            // Every argument left to the hypervisor: it picks the emulator and
            // the machine type it would actually use for a KVM x86_64 guest,
            // which is the same one `render` asks for. Naming a binary path
            // here would be this crate guessing at a layout that differs
            // between distributions.
            let xml = conn.get_domain_capabilities(None, Some("x86_64"), None, Some("kvm"), 0)?;
            Ok(crate::domain_caps::parse_cpu_models(&xml))
        })
        .await
    }

    async fn host(&self) -> Result<HostInfo> {
        let node = self.node.clone();
        self.call(move |conn| {
            let info = conn.get_node_info()?;
            let version = conn.get_lib_version().unwrap_or(0);
            Ok(HostInfo {
                node,
                // `cpus` is the number of logical processors the node has
                // online, which is what a machine's processor count is
                // measured against.
                cpus: info.cpus,
                memory_mib: info.memory / 1024,
                hypervisor_version: (version > 0).then(|| {
                    format!(
                        "{}.{}.{}",
                        version / 1_000_000,
                        (version % 1_000_000) / 1_000,
                        version % 1_000
                    )
                }),
            })
        })
        .await
    }

    async fn domains(&self) -> Result<Vec<ObservedDomain>> {
        self.call(move |conn| {
            // No flags: running and stopped alike. A machine that is off is
            // still a machine.
            let domains = conn.list_all_domains(0)?;
            domains.iter().map(observe).collect()
        })
        .await
    }

    async fn domain(&self, name: &str) -> Result<ObservedDomain> {
        self.with_domain(name, observe).await
    }

    async fn define(&self, xml: &str) -> Result<()> {
        let xml = xml.to_string();
        self.call(move |conn| Domain::define_xml(conn, &xml).map(|_| ()))
            .await
    }

    async fn undefine(&self, name: &str) -> Result<()> {
        self.with_domain(name, |domain| {
            // The firmware variable store and any saved state belong to the
            // machine: leaving them behind means the next machine to take the
            // name inherits somebody else's boot entries.
            domain.undefine_flags(
                virt_sys::VIR_DOMAIN_UNDEFINE_NVRAM
                    | virt_sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE
                    | virt_sys::VIR_DOMAIN_UNDEFINE_SNAPSHOTS_METADATA
                    | virt_sys::VIR_DOMAIN_UNDEFINE_CHECKPOINTS_METADATA,
            )
        })
        .await
    }

    async fn rename(&self, name: &str, new_name: &str) -> Result<()> {
        let new_name = new_name.to_string();
        self.with_domain(name, move |domain| domain.rename(&new_name, 0).map(|_| ()))
            .await
    }

    async fn start(&self, name: &str) -> Result<()> {
        self.with_domain(name, |domain| domain.create().map(|_| ()))
            .await
    }

    async fn shutdown(&self, name: &str) -> Result<()> {
        self.with_domain(name, |domain| domain.shutdown().map(|_| ()))
            .await
    }

    async fn destroy(&self, name: &str) -> Result<()> {
        self.with_domain(name, |domain| domain.destroy()).await
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        self.with_domain(name, |domain| domain.reboot(0)).await
    }

    async fn reset(&self, name: &str) -> Result<()> {
        self.with_domain(name, |domain| domain.reset().map(|_| ()))
            .await
    }

    async fn set_autostart(&self, name: &str, on: bool) -> Result<()> {
        self.with_domain(name, move |domain| domain.set_autostart(on).map(|_| ()))
            .await
    }

    async fn attach_device_live(&self, name: &str, device_xml: &str) -> Result<()> {
        let xml = device_xml.to_string();
        self.with_domain(name, move |domain| {
            domain.attach_device_flags(&xml, LIVE_ONLY).map(|_| ())
        })
        .await
    }

    async fn detach_device_live(&self, name: &str, device_xml: &str) -> Result<()> {
        let xml = device_xml.to_string();
        self.with_domain(name, move |domain| {
            domain.detach_device_flags(&xml, LIVE_ONLY).map(|_| ())
        })
        .await
    }

    async fn set_memory_live(&self, name: &str, mib: u64) -> Result<()> {
        let kib = mib.saturating_mul(1024);
        self.with_domain(name, move |domain| {
            // Only the running machine: the stored configuration already
            // carries the new number, because `define` put it there. The
            // hypervisor refuses to go above the maximum the machine booted
            // with, and that refusal is the answer we want.
            domain.set_memory_flags(kib, LIVE_ONLY).map(|_| ())
        })
        .await
    }

    async fn set_vcpus_live(&self, name: &str, count: u32) -> Result<()> {
        self.with_domain(name, move |domain| {
            domain.set_vcpus_flags(count, LIVE_ONLY).map(|_| ())
        })
        .await
    }

    async fn set_live_metadata(&self, name: &str, metadata_xml: &str) -> Result<()> {
        let metadata = metadata_xml.to_string();
        self.with_domain(name, move |domain| {
            domain
                .set_metadata(
                    virt_sys::VIR_DOMAIN_METADATA_ELEMENT as i32,
                    Some(&metadata),
                    Some("lumen"),
                    Some(crate::domain_xml::LUMEN_NS),
                    virt_sys::VIR_DOMAIN_AFFECT_LIVE,
                )
                .map(|_| ())
        })
        .await
    }

    async fn guest_agent(&self, name: &str, command: &str) -> Result<String> {
        let command = command.to_string();
        self.with_domain(name, move |domain| {
            // The timeout is in seconds and is deliberately short. A guest
            // whose agent is installed but wedged must not hold a blocking
            // thread indefinitely, and every command this appliance sends is
            // one the agent answers immediately or not at all.
            domain.qemu_agent_command(&command, GUEST_AGENT_TIMEOUT_SECS, 0)
        })
        .await
        .map_err(explain_agent_failure)
    }
}

/// How long to wait for a guest's agent, in seconds.
const GUEST_AGENT_TIMEOUT_SECS: i32 = 10;

/// The hypervisor's words about an agent, in the operator's.
///
/// This is the one call whose failure is usually not about the node at all:
/// almost every time, the guest simply has no `qemu-guest-agent` running, and
/// the hypervisor reports that as "Guest agent is not responding" — true, and
/// unactionable unless you already know what a guest agent is. The distinction
/// matters enough to name: nothing is wrong with the machine, something is
/// missing inside it.
fn explain_agent_failure(err: VirtError) -> VirtError {
    let said = err.to_string();
    let no_agent = said.contains("not responding")
        || said.contains("not connected")
        || said.contains("QEMU guest agent is not available");
    if !no_agent {
        return err;
    }
    VirtError::Conflict(format!(
        "This machine's guest agent did not answer ({said}). Copying a file in needs \
         qemu-guest-agent installed and running inside the guest — on this appliance's own \
         images it is there, and on a guest installed from other media it usually is not."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The projection an operator reads in the console. There is no hypervisor
    /// in this test — it is the mapping that is being pinned, not a call.
    #[test]
    fn hypervisor_states_project_onto_the_ones_the_console_shows() {
        assert_eq!(state_of(virt_sys::VIR_DOMAIN_RUNNING), DomainState::Running);
        assert_eq!(state_of(virt_sys::VIR_DOMAIN_BLOCKED), DomainState::Running);
        // Mid-shutdown is still running as far as anything an operator can do
        // to it is concerned.
        assert_eq!(
            state_of(virt_sys::VIR_DOMAIN_SHUTDOWN),
            DomainState::Running
        );
        assert_eq!(state_of(virt_sys::VIR_DOMAIN_SHUTOFF), DomainState::ShutOff);
        assert_eq!(state_of(virt_sys::VIR_DOMAIN_PAUSED), DomainState::Paused);
        assert_eq!(state_of(virt_sys::VIR_DOMAIN_CRASHED), DomainState::Crashed);
        assert_eq!(
            state_of(virt_sys::VIR_DOMAIN_PMSUSPENDED),
            DomainState::Suspended
        );
        assert_eq!(state_of(virt_sys::VIR_DOMAIN_NOSTATE), DomainState::Unknown);
    }

    /// The live calls must never touch the stored configuration — `define`
    /// owns it, and a device added by both would be added twice.
    #[test]
    fn live_calls_carry_only_the_live_flag() {
        assert_eq!(LIVE_ONLY, virt_sys::VIR_DOMAIN_AFFECT_LIVE);
        assert_eq!(LIVE_ONLY & virt_sys::VIR_DOMAIN_AFFECT_CONFIG, 0);
    }

    /// A node without KVM is a node that cannot run anything, and the answer
    /// has to say so in terms an operator can act on — the hypervisor's own
    /// sentence names the emulator and stops there. Backend, so `main` swaps
    /// in the unavailable backend and the console still comes up.
    #[test]
    fn a_node_without_kvm_says_what_to_check() {
        let err = kvm_unavailable("the accel 'kvm' is not supported by '/usr/libexec/qemu-kvm'");
        assert!(matches!(err, VirtError::Backend(_)), "{err:?}");
        let text = err.to_string();
        // The hypervisor's own words, kept.
        assert!(text.contains("/usr/libexec/qemu-kvm"), "{text}");
        // And both things that cause it, because the operator cannot tell
        // which one they are looking at from the hypervisor's sentence.
        assert!(text.contains("firmware"), "{text}");
        assert!(
            text.contains("pass hardware virtualization through"),
            "{text}"
        );
    }
}
