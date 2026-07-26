//! In-memory hypervisor. Deterministic, no daemon, no guests, no clock.
//!
//! Compiled unconditionally and exported (see the note in `backend/mod.rs`) so
//! lumen-controlplane's integration tests can inject it the way
//! `tests/auth_flow.rs` injects its mock realm. Every test in this crate and
//! every control-plane API test runs against it, which is what lets
//! `make test` pass on a machine with no hypervisor installed.
//!
//! It models enough to exercise the whole lifecycle: a domain is defined or it
//! is not, it is running or it is not, and the operations that make no sense
//! in the current state are refused the way the real hypervisor refuses them.
//!
//! Two things it deliberately does **not** model, because a test that depends
//! on either is a test about the mock rather than about Lumen:
//!
//! - A guest. An orderly shutdown request stops the machine immediately here;
//!   on a real node the guest decides when, and may decline.
//! - The gap between the stored configuration and the running one. `define`
//!   replaces the stored document, and that is what every read returns —
//!   exactly as on a real node, where the console edits the stored document.
//!   What a live call would or would not have managed is driven explicitly
//!   with [`MockBackend::refuse_live_changes`], because that is the branch
//!   worth testing.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::backend::VirtBackend;
use crate::domain_caps::{CpuModelInfo, CpuModels};
use crate::domain_xml;
use crate::error::{Result, VirtError};
use crate::state::{DomainRuntime, DomainState, HostInfo, ObservedDomain};

#[derive(Debug, Clone)]
struct Entry {
    xml: String,
    state: DomainState,
    autostart: bool,
    uuid: String,
    started_at: Option<u64>,
    /// Memory and processors as the *running* machine has them. They diverge
    /// from the stored document exactly when a live change was refused.
    live_memory_mib: u64,
    live_vcpus: u32,
    cpu_time_ns: u64,
}

#[derive(Debug)]
struct Inner {
    host: HostInfo,
    domains: BTreeMap<String, Entry>,
    /// Every live call the service made, in order, for assertions.
    live_calls: Vec<String>,
    /// When set, every live call fails with this reason — the running machine
    /// that cannot take the change, so it waits for a restart.
    refuse_live: Option<String>,
    next_uuid: u32,
    /// What `start` records as the start time. Fixed rather than read from a
    /// clock, so an uptime assertion is not a race.
    now: u64,
    /// Whether the guests here have an agent that answers. A guest somebody
    /// installed from their own media usually has none, so "no agent" is an
    /// ordinary state a test asks for rather than a failure to simulate.
    agent_answers: bool,
    /// Files the agent currently has open, by handle: where it is going, and
    /// what has arrived so far.
    agent_open: BTreeMap<i64, (String, Vec<u8>)>,
    /// What actually landed inside the guest, by path — the thing a test about
    /// pushing a file in wants to look at.
    guest_files: BTreeMap<String, Vec<u8>>,
    next_agent_handle: i64,
}

pub struct MockBackend {
    inner: Mutex<Inner>,
}

impl MockBackend {
    /// A node with no machines on it: sixteen threads, 32 GiB, nothing
    /// defined. The shape of an appliance the first machine is about to be
    /// created on.
    pub fn appliance() -> Self {
        Self::with_host(HostInfo {
            node: "lumen".into(),
            cpus: 16,
            memory_mib: 32_768,
            hypervisor_version: Some("11.10.0".into()),
        })
    }

    pub fn with_host(host: HostInfo) -> Self {
        Self {
            inner: Mutex::new(Inner {
                host,
                domains: BTreeMap::new(),
                live_calls: Vec::new(),
                refuse_live: None,
                next_uuid: 1,
                now: 1_785_000_000,
                agent_answers: true,
                agent_open: BTreeMap::new(),
                guest_files: BTreeMap::new(),
                next_agent_handle: 1000,
            }),
        }
    }

    /// A node whose guests have no agent in them — the ordinary case for a
    /// machine somebody installed themselves.
    pub fn without_guest_agent(self) -> Self {
        self.inner.lock().unwrap().agent_answers = false;
        self
    }

    /// What a file pushed into a guest actually contains, on the other side.
    pub fn guest_file(&self, path: &str) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().guest_files.get(path).cloned()
    }

    /// Make every live change fail, so the service has to report the change as
    /// waiting for a restart.
    pub fn refuse_live_changes(&self, reason: impl Into<String>) {
        self.inner.lock().unwrap().refuse_live = Some(reason.into());
    }

    pub fn allow_live_changes(&self) {
        self.inner.lock().unwrap().refuse_live = None;
    }

    /// Every live call made, in order, as `verb name`.
    pub fn live_calls(&self) -> Vec<String> {
        self.inner.lock().unwrap().live_calls.clone()
    }

    pub fn names(&self) -> Vec<String> {
        self.inner.lock().unwrap().domains.keys().cloned().collect()
    }

    pub fn is_defined(&self, name: &str) -> bool {
        self.inner.lock().unwrap().domains.contains_key(name)
    }

    pub fn state_of(&self, name: &str) -> Option<DomainState> {
        self.inner
            .lock()
            .unwrap()
            .domains
            .get(name)
            .map(|entry| entry.state)
    }

    /// The stored document, for assertions about what was actually written.
    pub fn xml_of(&self, name: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .domains
            .get(name)
            .map(|entry| entry.xml.clone())
    }

    /// Move the mock's clock, so an uptime can be asserted exactly.
    pub fn set_now(&self, now: u64) {
        self.inner.lock().unwrap().now = now;
    }

    pub fn now(&self) -> u64 {
        self.inner.lock().unwrap().now
    }

    /// Pretend the guest crashed, which is a state no operation can produce.
    pub fn crash(&self, name: &str) {
        if let Some(entry) = self.inner.lock().unwrap().domains.get_mut(name) {
            entry.state = DomainState::Crashed;
            entry.started_at = None;
        }
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::appliance()
    }
}

impl Inner {
    fn entry(&mut self, name: &str) -> Result<&mut Entry> {
        self.domains.get_mut(name).ok_or_else(|| {
            VirtError::NotFound(format!("No machine named \"{name}\" on this node."))
        })
    }

    fn observe(&self, name: &str, entry: &Entry) -> ObservedDomain {
        let running = entry.state.is_running();
        ObservedDomain {
            name: name.to_string(),
            uuid: Some(entry.uuid.clone()),
            state: entry.state,
            persistent: true,
            autostart: entry.autostart,
            runtime: running.then_some(DomainRuntime {
                vcpus: entry.live_vcpus,
                memory_kib: entry.live_memory_mib * 1024,
                max_memory_kib: entry.live_memory_mib * 1024,
                cpu_time_ns: entry.cpu_time_ns,
            }),
            xml: entry.xml.clone(),
            started_at: running.then_some(entry.started_at).flatten(),
        }
    }

    /// The one gate every live call passes through.
    fn live(&mut self, verb: &str, name: &str) -> Result<()> {
        let refused = self.refuse_live.clone();
        let entry = self.entry(name)?;
        if !entry.state.is_running() {
            return Err(VirtError::Conflict(format!(
                "\"{name}\" is not running, so there is nothing live to change."
            )));
        }
        self.live_calls.push(format!("{verb} {name}"));
        match refused {
            Some(reason) => Err(VirtError::Backend(anyhow::anyhow!("{reason}"))),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl VirtBackend for MockBackend {
    async fn host(&self) -> Result<HostInfo> {
        Ok(self.inner.lock().unwrap().host.clone())
    }

    /// A small, fixed processor list with one of each interesting case: a
    /// usable model, one this "host" cannot run, and a deprecated one. Enough
    /// to exercise every branch above it without pretending to be a CPU.
    async fn cpu_models(&self) -> Result<CpuModels> {
        Ok(CpuModels {
            host_model: Some("EPYC-Rome".into()),
            host_passthrough: true,
            models: vec![
                CpuModelInfo {
                    name: "EPYC".into(),
                    usable: true,
                    vendor: Some("AMD".into()),
                    deprecated: false,
                },
                CpuModelInfo {
                    name: "EPYC-Rome".into(),
                    usable: true,
                    vendor: Some("AMD".into()),
                    deprecated: false,
                },
                CpuModelInfo {
                    name: "Skylake-Server".into(),
                    usable: false,
                    vendor: Some("Intel".into()),
                    deprecated: false,
                },
                CpuModelInfo {
                    name: "qemu64".into(),
                    usable: true,
                    vendor: None,
                    deprecated: true,
                },
            ],
            reason: None,
        })
    }

    async fn domains(&self) -> Result<Vec<ObservedDomain>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .domains
            .iter()
            .map(|(name, entry)| inner.observe(name, entry))
            .collect())
    }

    async fn domain(&self, name: &str) -> Result<ObservedDomain> {
        let inner = self.inner.lock().unwrap();
        inner
            .domains
            .get(name)
            .map(|entry| inner.observe(name, entry))
            .ok_or_else(|| {
                VirtError::NotFound(format!("No machine named \"{name}\" on this node."))
            })
    }

    async fn define(&self, xml: &str) -> Result<()> {
        // The real hypervisor reads the document too, and refuses one it
        // cannot make sense of — but it does not care whose machine it is, so
        // this reads only what every domain has. A node may perfectly well
        // hold machines somebody defined by hand.
        let summary = domain_xml::summarize(xml)?;
        if summary.name.is_empty() {
            return Err(VirtError::Backend(anyhow::anyhow!(
                "a domain document must name the machine"
            )));
        }
        let name = summary.name.clone();
        let mut inner = self.inner.lock().unwrap();

        match inner.domains.get_mut(&name) {
            Some(entry) => {
                // Redefining a running machine changes what it will boot with
                // next, and nothing about what it is doing now.
                entry.xml = xml.to_string();
                if !entry.state.is_running() {
                    entry.live_memory_mib = summary.memory_mib;
                    entry.live_vcpus = summary.vcpus;
                }
            }
            None => {
                inner.next_uuid += 1;
                let uuid = format!("00000000-0000-4000-8000-{:012x}", inner.next_uuid);
                inner.domains.insert(
                    name,
                    Entry {
                        xml: xml.to_string(),
                        state: DomainState::ShutOff,
                        autostart: false,
                        uuid,
                        started_at: None,
                        live_memory_mib: summary.memory_mib,
                        live_vcpus: summary.vcpus,
                        cpu_time_ns: 0,
                    },
                );
            }
        }
        Ok(())
    }

    async fn undefine(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.entry(name)?;
        if entry.state.is_running() {
            return Err(VirtError::Conflict(format!(
                "\"{name}\" is running and cannot be removed."
            )));
        }
        inner.domains.remove(name);
        Ok(())
    }

    async fn rename(&self, name: &str, new_name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.entry(name)?.clone();
        if entry.state.is_running() {
            return Err(VirtError::Conflict(format!(
                "\"{name}\" is running and cannot be renamed."
            )));
        }
        if inner.domains.contains_key(new_name) {
            return Err(VirtError::Conflict(format!(
                "\"{new_name}\" already exists."
            )));
        }
        inner.domains.remove(name);
        inner.domains.insert(new_name.to_string(), entry);
        Ok(())
    }

    async fn start(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let now = inner.now;
        let entry = inner.entry(name)?;
        if entry.state.is_running() {
            return Err(VirtError::Conflict(format!(
                "\"{name}\" is already running."
            )));
        }
        // Starting is where the running machine picks up the stored
        // configuration — which is exactly why a change that could not be made
        // live takes effect at the next restart.
        let summary = domain_xml::summarize(&entry.xml)?;
        entry.live_memory_mib = summary.memory_mib;
        entry.live_vcpus = summary.vcpus;
        entry.state = DomainState::Running;
        entry.started_at = Some(now);
        entry.cpu_time_ns = 0;
        Ok(())
    }

    async fn shutdown(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.entry(name)?;
        if !entry.state.is_running() {
            return Err(VirtError::Conflict(format!("\"{name}\" is not running.")));
        }
        // No guest here, so the request is granted immediately. On a real node
        // the guest decides when, and may decline entirely.
        entry.state = DomainState::ShutOff;
        entry.started_at = None;
        Ok(())
    }

    async fn destroy(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.entry(name)?;
        if !entry.state.is_running() {
            return Err(VirtError::Conflict(format!("\"{name}\" is not running.")));
        }
        entry.state = DomainState::ShutOff;
        entry.started_at = None;
        Ok(())
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let now = inner.now;
        let entry = inner.entry(name)?;
        if !entry.state.is_running() {
            return Err(VirtError::Conflict(format!("\"{name}\" is not running.")));
        }
        let summary = domain_xml::summarize(&entry.xml)?;
        entry.live_memory_mib = summary.memory_mib;
        entry.live_vcpus = summary.vcpus;
        entry.started_at = Some(now);
        Ok(())
    }

    async fn reset(&self, name: &str) -> Result<()> {
        self.reboot(name).await
    }

    async fn set_autostart(&self, name: &str, on: bool) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.entry(name)?.autostart = on;
        Ok(())
    }

    async fn attach_device_live(&self, name: &str, _device_xml: &str) -> Result<()> {
        self.inner.lock().unwrap().live("attach", name)
    }

    async fn detach_device_live(&self, name: &str, _device_xml: &str) -> Result<()> {
        self.inner.lock().unwrap().live("detach", name)
    }

    async fn set_memory_live(&self, name: &str, mib: u64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.live("memory", name)?;
        inner.entry(name)?.live_memory_mib = mib;
        Ok(())
    }

    async fn set_vcpus_live(&self, name: &str, count: u32) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.live("vcpus", name)?;
        inner.entry(name)?.live_vcpus = count;
        Ok(())
    }

    async fn set_live_metadata(&self, name: &str, metadata_xml: &str) -> Result<()> {
        let started = domain_xml::started_from_metadata(metadata_xml);
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.entry(name)?;
        if !entry.state.is_running() {
            return Err(VirtError::Conflict(format!("\"{name}\" is not running.")));
        }
        entry.started_at = started;
        Ok(())
    }

    /// The mock has one document per machine and does not model the gap
    /// between stored and running — with one deliberate exception. The stored
    /// document defers the console socket's path to the hypervisor, which
    /// chooses one under its per-domain directory at start and publishes it in
    /// the live document (`qemuProcessGraphicsSetupListen`). The console flow
    /// *is* that round trip, so the mock does here what the real hypervisor
    /// does: a running machine's pathless listener answers with a path.
    async fn live_xml(&self, name: &str) -> Result<String> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.entry(name)?;
        if entry.state.is_running() {
            return Ok(entry.xml.replace(
                "<listen type='socket'/>",
                &format!(
                    "<listen type='socket' \
                     socket='/var/lib/libvirt/qemu/domain-1-{name}/vnc.sock'/>"
                ),
            ));
        }
        Ok(entry.xml.clone())
    }

    /// A fake rather than a mock: it applies the three file commands to an
    /// in-memory guest, so the round trip the service actually performs —
    /// open, write in chunks, close — is the thing under test rather than a
    /// record of three calls having been made.
    async fn guest_agent(&self, name: &str, command: &str) -> Result<String> {
        let mut inner = self.inner.lock().unwrap();
        {
            // The machine has to be there and be up, exactly as on a real node:
            // there is no agent to talk to in a machine that is switched off.
            let entry = inner.entry(name)?;
            if !entry.state.is_running() {
                return Err(VirtError::Conflict(format!("\"{name}\" is not running.")));
            }
        }
        if !inner.agent_answers {
            return Err(VirtError::Conflict(format!(
                "This machine's guest agent did not answer. Copying a file in needs \
                 qemu-guest-agent installed and running inside \"{name}\"."
            )));
        }

        let request: serde_json::Value = serde_json::from_str(command).map_err(|err| {
            VirtError::Backend(anyhow::anyhow!("not a guest agent command: {err}"))
        })?;
        let execute = request
            .get("execute")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let arguments = request.get("arguments").cloned().unwrap_or_default();
        let argument = |key: &str| arguments.get(key).cloned().unwrap_or_default();

        match execute.as_str() {
            "guest-file-open" => {
                let path = argument("path").as_str().unwrap_or_default().to_string();
                let handle = inner.next_agent_handle;
                inner.next_agent_handle += 1;
                inner.agent_open.insert(handle, (path, Vec::new()));
                Ok(format!("{{\"return\": {handle}}}"))
            }
            "guest-file-write" => {
                let handle = argument("handle").as_i64().unwrap_or(-1);
                let encoded = argument("buf-b64").as_str().unwrap_or_default().to_string();
                let bytes = base64_decode(&encoded)?;
                let written = bytes.len();
                let open = inner.agent_open.get_mut(&handle).ok_or_else(|| {
                    VirtError::Conflict(format!("no file is open on handle {handle}."))
                })?;
                open.1.extend_from_slice(&bytes);
                Ok(format!(
                    "{{\"return\": {{\"count\": {written}, \"eof\": false}}}}"
                ))
            }
            "guest-file-close" => {
                let handle = argument("handle").as_i64().unwrap_or(-1);
                let (path, contents) = inner.agent_open.remove(&handle).ok_or_else(|| {
                    VirtError::Conflict(format!("no file is open on handle {handle}."))
                })?;
                inner.guest_files.insert(path, contents);
                Ok("{\"return\": {}}".to_string())
            }
            other => Err(VirtError::Conflict(format!(
                "the mock guest agent does not implement \"{other}\"."
            ))),
        }
    }
}

fn base64_decode(encoded: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|err| VirtError::Backend(anyhow::anyhow!("the agent was sent bad base64: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::VmConfig;

    fn config(name: &str, vmid: u32) -> VmConfig {
        VmConfig {
            vmid,
            name: name.into(),
            ..VmConfig::default()
        }
    }

    async fn defined(backend: &MockBackend, name: &str, vmid: u32) {
        backend
            .define(&domain_xml::render(&config(name, vmid)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_machine_can_be_defined_started_stopped_and_removed() {
        let backend = MockBackend::appliance();
        assert!(backend.names().is_empty());

        defined(&backend, "web01", 101).await;
        assert_eq!(backend.state_of("web01"), Some(DomainState::ShutOff));

        backend.start("web01").await.unwrap();
        assert_eq!(backend.state_of("web01"), Some(DomainState::Running));
        let observed = backend.domain("web01").await.unwrap();
        assert!(observed.runtime.is_some());
        assert_eq!(observed.started_at, Some(backend.now()));

        // Removing a running machine is refused, the way the hypervisor
        // refuses it.
        assert!(matches!(
            backend.undefine("web01").await.unwrap_err(),
            VirtError::Conflict(_)
        ));

        backend.shutdown("web01").await.unwrap();
        assert_eq!(backend.state_of("web01"), Some(DomainState::ShutOff));
        assert!(backend.domain("web01").await.unwrap().runtime.is_none());

        backend.undefine("web01").await.unwrap();
        assert!(!backend.is_defined("web01"));
    }

    #[tokio::test]
    async fn the_operations_that_make_no_sense_right_now_are_refused() {
        let backend = MockBackend::appliance();
        defined(&backend, "web01", 101).await;

        assert!(matches!(
            backend.shutdown("web01").await.unwrap_err(),
            VirtError::Conflict(_)
        ));
        backend.start("web01").await.unwrap();
        assert!(matches!(
            backend.start("web01").await.unwrap_err(),
            VirtError::Conflict(_)
        ));
        assert!(matches!(
            backend.domain("nothing").await.unwrap_err(),
            VirtError::NotFound(_)
        ));
    }

    /// The change that could not be made live is the one that takes effect at
    /// the next restart — which is what makes "waiting for a restart" true
    /// rather than a label.
    #[tokio::test]
    async fn a_refused_live_change_takes_effect_when_the_machine_next_starts() {
        let backend = MockBackend::appliance();
        defined(&backend, "web01", 101).await;
        backend.start("web01").await.unwrap();
        backend.refuse_live_changes("cannot grow beyond the boot maximum");

        let mut bigger = config("web01", 101);
        bigger.memory_mib = 8192;
        backend.define(&domain_xml::render(&bigger)).await.unwrap();
        assert!(backend.set_memory_live("web01", 8192).await.is_err());

        // Still running on what it booted with…
        let observed = backend.domain("web01").await.unwrap();
        assert_eq!(observed.runtime.unwrap().memory_kib, 2048 * 1024);
        // …but the stored document already says otherwise.
        assert_eq!(
            domain_xml::parse(&observed.xml).unwrap().config.memory_mib,
            8192
        );

        backend.shutdown("web01").await.unwrap();
        backend.start("web01").await.unwrap();
        assert_eq!(
            backend
                .domain("web01")
                .await
                .unwrap()
                .runtime
                .unwrap()
                .memory_kib,
            8192 * 1024
        );
        assert_eq!(backend.live_calls(), vec!["memory web01".to_string()]);
    }

    #[tokio::test]
    async fn a_document_the_hypervisor_could_not_read_is_refused() {
        let backend = MockBackend::appliance();
        // Not a document at all.
        assert!(backend
            .define("<domain><name>x</name></nope></domain>")
            .await
            .is_err());
        // A document, but one that names no machine.
        assert!(backend
            .define("<domain type='kvm'><devices/></domain>")
            .await
            .is_err());
        assert!(backend.names().is_empty());
    }

    /// A machine somebody defined with the hypervisor's own tools is not
    /// Lumen's, but it is still a machine the node holds — and the hypervisor
    /// has no opinion about whose it is.
    #[tokio::test]
    async fn a_domain_that_is_not_lumens_is_still_accepted() {
        let backend = MockBackend::appliance();
        backend
            .define(
                "<domain type='kvm'><name>legacy</name><memory unit='KiB'>1048576</memory>\
                 <vcpu>1</vcpu><devices/></domain>",
            )
            .await
            .unwrap();
        assert_eq!(backend.names(), vec!["legacy".to_string()]);
        let observed = backend.domain("legacy").await.unwrap();
        assert_eq!(observed.state, DomainState::ShutOff);
        // …and it is the *service* that decides it is not one of ours.
        assert!(domain_xml::parse(&observed.xml).is_err());
    }

    #[tokio::test]
    async fn the_autostart_flag_lives_outside_the_document() {
        let backend = MockBackend::appliance();
        defined(&backend, "web01", 101).await;
        assert!(!backend.domain("web01").await.unwrap().autostart);
        backend.set_autostart("web01", true).await.unwrap();
        assert!(backend.domain("web01").await.unwrap().autostart);
        // …and redefining the machine does not disturb it.
        defined(&backend, "web01", 101).await;
        assert!(backend.domain("web01").await.unwrap().autostart);
    }
}
