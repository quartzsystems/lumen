//! The virtualization domain's one entry point.
//!
//! Everything the control plane's HTTP handlers do goes through here:
//! defining a machine, changing it, starting and stopping it, and giving it
//! disks and adapters. The handlers deserialize, call one method, serialize —
//! no documents, no hypervisor calls, and no validation above this line.
//!
//! ## No staged apply
//!
//! Networking stages every change and applies it inside a checkpoint that
//! reverts itself, because a bad network commit costs a drive to the rack.
//! Machines are not like that: starting one is immediate, observable, and
//! reversible by stopping it again, and a machine that fails to start leaves
//! the node exactly as it was. Copying the checkpoint engine here would add a
//! second staging system, a second set of confirm endpoints, and a second way
//! for the console to be out of step with the node — in exchange for undoing
//! something a single click already undoes.
//!
//! What machines do have, and networking does not, is the split between a
//! change that reaches the running guest and one that waits for a restart.
//! That distinction is not invented here: the stored configuration is written
//! once with `define`, the live change is attempted on top, and **the
//! hypervisor's own refusal** is what turns into "waiting for a restart".

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use lumen_net::NetworkService;
use lumen_zfs::StorageService;

use crate::backend::VirtBackend;
use crate::domain_caps::CpuModels;
use crate::domain_xml;
use crate::error::{Result, VirtError};
use crate::model::{
    generate_mac, valid_vm_name, BootDevice, CacheMode, CpuModel, CpuTopology, DiskBus, Firmware,
    NicModel, VmCdrom, VmConfig, VmDisk, VmNic, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, FIRST_VMID,
    LAST_VMID,
};
use crate::osinfo::{self, OsCatalog};
use crate::state::{DomainState, HostInfo, ObservedDomain};
use crate::validate::{
    check_destructive, validate, Acknowledgements, HostFacts, PlannedDisk, ValidationCode,
    ValidationError,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// Whether a lifecycle control is available, and — when it is not — why, so
/// the console renders a control that explains itself rather than one that is
/// silently grey. The same shape as `lumen_net`'s `delete_blocked_reason`.
#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The console must collect `i_understand_this_may_lose_data` before
    /// sending this one.
    pub requires_acknowledgement: bool,
}

impl Action {
    fn yes() -> Self {
        Self {
            allowed: true,
            reason: None,
            requires_acknowledgement: false,
        }
    }

    fn risky() -> Self {
        Self {
            allowed: true,
            reason: None,
            requires_acknowledgement: true,
        }
    }

    fn no(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            reason: Some(reason.into()),
            requires_acknowledgement: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VmActions {
    pub start: Action,
    pub shutdown: Action,
    pub stop: Action,
    pub reboot: Action,
    pub reset: Action,
    pub delete: Action,
}

fn actions_for(state: DomainState, name: &str) -> VmActions {
    let running = state.is_running();
    let off_reason = format!("\"{name}\" is not running.");
    let on_reason = format!("\"{name}\" is already running.");
    VmActions {
        start: if running {
            Action::no(on_reason)
        } else {
            Action::yes()
        },
        shutdown: if running {
            Action::yes()
        } else {
            Action::no(off_reason.clone())
        },
        stop: if running {
            Action::risky()
        } else {
            Action::no(off_reason.clone())
        },
        reboot: if running {
            Action::yes()
        } else {
            Action::no(off_reason.clone())
        },
        reset: if running {
            Action::risky()
        } else {
            Action::no(off_reason)
        },
        // Removing a machine is always offered; removing a *running* one, or
        // removing its disks, is what needs saying out loud.
        delete: if running {
            Action::risky()
        } else {
            Action::yes()
        },
    }
}

/// One row of the console's machine table, and the whole of its detail page.
/// Everything either needs is here, so rendering never needs a second round
/// trip — the shape `lumen_net::service::LinkView` established.
#[derive(Debug, Clone, Serialize)]
pub struct VmView {
    pub vmid: u32,
    pub name: String,
    pub node: String,
    pub description: Option<String>,
    pub state: DomainState,
    pub tags: Vec<String>,

    // Configuration — the stored one, which is what the console edits.
    pub vcpus: u32,
    pub memory_mib: u64,
    pub cpu_model: CpuModel,
    pub topology: Option<CpuTopology>,
    pub machine: String,
    pub firmware: Firmware,
    pub boot_order: Vec<BootDevice>,
    pub start_on_boot: bool,
    pub guest_agent: bool,
    /// What the machine was built to run, in libosinfo's words. Metadata; the
    /// console shows it and nothing reads it to decide anything.
    pub os_id: Option<String>,
    pub disks: Vec<VmDisk>,
    pub cdroms: Vec<VmCdrom>,
    pub nics: Vec<VmNic>,

    // What the running machine is actually doing. All absent when it is not.
    pub current_vcpus: Option<u32>,
    pub current_memory_mib: Option<u64>,
    pub cpu_time_ns: Option<u64>,
    pub uptime_secs: Option<u64>,

    /// Convenience the table would otherwise have to compute per row.
    pub boot_disk: Option<String>,
    pub total_disk_bytes: u64,
    /// Where the machine's console socket is, for the viewer part 2 adds.
    pub vnc_socket: String,
    /// Changes that are stored but will not reach the guest until it restarts.
    pub pending_reboot: Vec<String>,
    pub actions: VmActions,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeVms {
    pub node: String,
    pub vms: Vec<VmView>,
}

/// GET /api/vms. Grouped by node even with one node, for the same reason
/// `/api/network/interfaces` is: the shape must not change when clustering
/// lands, and the console renders its per-node layout now.
#[derive(Debug, Clone, Serialize)]
pub struct VmsResponse {
    pub nodes: Vec<NodeVms>,
}

/// The answer to anything that changes a machine.
#[derive(Debug, Clone, Serialize)]
pub struct VmUpdateResponse {
    pub vm: VmView,
    /// Changes the running machine took immediately.
    pub applied_live: Vec<String>,
    /// Changes that are stored but wait for a restart, each carrying the
    /// hypervisor's own reason where it gave one.
    pub pending_reboot: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VmDeleteResponse {
    pub vmid: u32,
    pub name: String,
    /// Volumes actually removed. Empty unless `purge_disks` was asked for.
    pub removed_volumes: Vec<String>,
    /// Volumes left behind, which is the default — an operator who did not ask
    /// for the data to go needs to be told where it still is.
    pub kept_volumes: Vec<String>,
}

// --- request bodies ----------------------------------------------------------

fn default_vcpus() -> u32 {
    DEFAULT_VCPUS
}

fn default_memory() -> u64 {
    DEFAULT_MEMORY_MIB
}

fn default_true() -> bool {
    true
}

/// POST /api/vms.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmCreate {
    pub name: String,
    /// Allocated as the lowest free identifier when absent.
    #[serde(default)]
    pub vmid: Option<u32>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_vcpus")]
    pub vcpus: u32,
    #[serde(default = "default_memory")]
    pub memory_mib: u64,
    #[serde(default)]
    pub cpu_model: CpuModel,
    #[serde(default)]
    pub topology: Option<CpuTopology>,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub firmware: Firmware,
    #[serde(default)]
    pub boot_order: Option<Vec<BootDevice>>,
    #[serde(default)]
    pub start_on_boot: bool,
    #[serde(default = "default_true")]
    pub guest_agent: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The guest this machine is for, as a libosinfo identifier. Checked
    /// against the node's own database when it has one.
    #[serde(default)]
    pub os_id: Option<String>,
    #[serde(default)]
    pub disks: Vec<DiskCreate>,
    /// Optical drives, in order. The first is the installation media; a second
    /// is where the driver disc a Windows installer needs goes.
    #[serde(default)]
    pub cdroms: Vec<CdromCreate>,
    #[serde(default)]
    pub nics: Vec<NicCreate>,
    /// Start it as soon as it is defined.
    #[serde(default)]
    pub start: bool,
}

/// A disk to create and attach. The size is in GiB because that is the unit an
/// operator thinks in; everything below this line is bytes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskCreate {
    pub pool: String,
    pub size_gib: u64,
    #[serde(default)]
    pub bus: DiskBus,
    #[serde(default)]
    pub cache: CacheMode,
    #[serde(default = "default_true")]
    pub discard: bool,
    /// Volume block size in bytes. `None` leaves the pool default.
    #[serde(default)]
    pub blocksize: Option<u64>,
}

/// An optical drive to define. The image is named by the pool it is in and
/// its file name, never by a path: a path from the console would be a path the
/// console chose, and the one rule about where media may live belongs in the
/// storage domain.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CdromCreate {
    /// The pool whose media library the image is in. Absent leaves the drive
    /// empty, which is a real thing to ask for.
    #[serde(default)]
    pub storage: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    /// An absolute path, resolved by the caller. Set by the control plane from
    /// `storage`/`image`; a request that sets it directly is refused.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NicCreate {
    pub bridge: String,
    #[serde(default)]
    pub model: NicModel,
    #[serde(default)]
    pub vlan_tag: Option<u16>,
}

/// PATCH /api/vms/{vmid}. Absent fields are left alone.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub vcpus: Option<u32>,
    pub memory_mib: Option<u64>,
    pub cpu_model: Option<CpuModel>,
    pub topology: Option<CpuTopology>,
    pub machine: Option<String>,
    pub firmware: Option<Firmware>,
    pub boot_order: Option<Vec<BootDevice>>,
    pub start_on_boot: Option<bool>,
    pub guest_agent: Option<bool>,
    pub tags: Option<Vec<String>>,
}

// --- the service -------------------------------------------------------------

pub struct VirtService {
    backend: Arc<dyn VirtBackend>,
    storage: Arc<StorageService>,
    network: Arc<NetworkService>,
    node: String,
    /// Serializes every mutation. Two machines being created at once must not
    /// race for the same identifier.
    gate: Mutex<()>,
    /// Where the guest operating system database lives, and the copy read out
    /// of it. See [`VirtService::os_catalog`] for why it is cached.
    osinfo_root: std::path::PathBuf,
    os_catalog: tokio::sync::RwLock<Option<OsCatalog>>,
}

/// One machine, as the hypervisor holds it and as Lumen reads it.
struct Machine {
    observed: ObservedDomain,
    config: VmConfig,
}

impl VirtService {
    pub fn new(
        backend: Arc<dyn VirtBackend>,
        storage: Arc<StorageService>,
        network: Arc<NetworkService>,
    ) -> Self {
        Self {
            backend,
            storage,
            network,
            node: crate::state::hostname(),
            gate: Mutex::new(()),
            osinfo_root: osinfo::OSINFO_DB_ROOT.into(),
            os_catalog: tokio::sync::RwLock::new(None),
        }
    }

    /// The same service reading its guest database from somewhere else — the
    /// seam the tests use, so none of them depends on what is installed on the
    /// machine running them.
    pub fn with_osinfo_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.osinfo_root = root.into();
        self.os_catalog = tokio::sync::RwLock::new(None);
        self
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    // --- reads -----------------------------------------------------------

    /// Every machine on this node that Lumen manages, in identifier order.
    ///
    /// A domain somebody defined by hand carries no Lumen identifier and is
    /// skipped rather than guessed at — but its *name* is still remembered, so
    /// a new machine cannot be given a name the node already has.
    async fn machines(&self) -> Result<(Vec<Machine>, Vec<String>)> {
        let domains = self.backend.domains().await?;
        let mut machines = Vec::new();
        let mut foreign = Vec::new();
        for observed in domains {
            match domain_xml::parse(&observed.xml) {
                Ok(parsed) => {
                    let mut config = parsed.config;
                    // "Start on boot" is the hypervisor's autostart flag, not
                    // part of the document, so it is folded in here.
                    config.start_on_boot = observed.autostart;
                    machines.push(Machine { observed, config });
                }
                Err(err) => {
                    tracing::debug!(domain = %observed.name, "not a Lumen machine: {err}");
                    foreign.push(observed.name.clone());
                }
            }
        }
        machines.sort_by_key(|m| m.config.vmid);
        Ok((machines, foreign))
    }

    async fn machine(&self, vmid: u32) -> Result<Machine> {
        let (machines, _) = self.machines().await?;
        machines
            .into_iter()
            .find(|m| m.config.vmid == vmid)
            .ok_or_else(|| VirtError::NotFound(format!("No machine with identifier {vmid}.")))
    }

    pub async fn list(&self) -> Result<VmsResponse> {
        let (machines, _) = self.machines().await?;
        let vms = machines.iter().map(|m| self.view_of(m)).collect();
        Ok(VmsResponse {
            nodes: vec![NodeVms {
                node: self.node.clone(),
                vms,
            }],
        })
    }

    pub async fn get(&self, vmid: u32) -> Result<VmView> {
        Ok(self.view_of(&self.machine(vmid).await?))
    }

    pub async fn host(&self) -> Result<HostInfo> {
        self.backend.host().await
    }

    /// The lowest free identifier at or above 100.
    ///
    /// Scanning rather than counting means a machine removed from the middle
    /// frees its number, which is what an operator expects and what keeps the
    /// numbers small enough to say out loud.
    pub async fn next_vmid(&self) -> Result<u32> {
        let (machines, _) = self.machines().await?;
        let taken: Vec<u32> = machines.iter().map(|m| m.config.vmid).collect();
        (FIRST_VMID..=LAST_VMID)
            .find(|id| !taken.contains(id))
            .ok_or_else(|| {
                VirtError::Conflict(format!(
                    "Every machine identifier from {FIRST_VMID} to {LAST_VMID} is in use."
                ))
            })
    }

    /// Everything the pure checks need about this node, gathered once.
    ///
    /// A subsystem that cannot be read is recorded as unreadable rather than
    /// as empty: refusing to define a machine because storage is down would
    /// turn one broken thing into two.
    async fn facts(&self, excluding: Option<u32>) -> Result<HostFacts> {
        let host = self.backend.host().await.unwrap_or_else(|err| {
            tracing::warn!("could not read this node's size: {err}");
            HostInfo {
                node: self.node.clone(),
                ..HostInfo::default()
            }
        });

        let (bridges, bridges_known) = match self.network.interfaces().await {
            Ok(response) => (
                response
                    .nodes
                    .into_iter()
                    .flat_map(|node| node.interfaces)
                    .filter(|link| link.kind == lumen_net::LinkKind::Bridge)
                    .map(|link| link.name)
                    .collect(),
                true,
            ),
            Err(err) => {
                tracing::warn!("could not read this node's bridges, skipping that check: {err}");
                (Vec::new(), false)
            }
        };

        let (pools, pools_known) = match self.storage.observe().await {
            Ok(state) => (
                state
                    .pools
                    .into_iter()
                    .map(|pool| (pool.name, pool.free))
                    .collect(),
                true,
            ),
            Err(err) => {
                tracing::warn!("could not read this node's pools, skipping that check: {err}");
                (Vec::new(), false)
            }
        };

        let (machines, foreign) = self.machines().await?;
        let mut existing: Vec<(u32, String)> = machines
            .iter()
            .filter(|m| Some(m.config.vmid) != excluding)
            .map(|m| (m.config.vmid, m.config.name.clone()))
            .collect();
        // A domain Lumen did not define still owns its name on this node.
        // Identifier 0 is outside the range Lumen allocates, so it can only
        // ever collide on the name.
        existing.extend(foreign.into_iter().map(|name| (0, name)));

        Ok(HostFacts {
            host,
            bridges,
            bridges_known,
            pools,
            pools_known,
            existing,
        })
    }

    fn view_of(&self, machine: &Machine) -> VmView {
        let config = &machine.config;
        let observed = &machine.observed;
        let runtime = observed.runtime;

        // What is stored but has not reached the running machine. Compared
        // against what it is actually doing, not against a guess.
        let mut pending = Vec::new();
        if let Some(runtime) = runtime {
            let live_memory = runtime.memory_kib / 1024;
            if live_memory != config.memory_mib {
                pending.push(format!(
                    "memory ({live_memory} MiB now, {} MiB after a restart)",
                    config.memory_mib
                ));
            }
            if runtime.vcpus != config.vcpus {
                pending.push(format!(
                    "processors ({} now, {} after a restart)",
                    runtime.vcpus, config.vcpus
                ));
            }
        }

        VmView {
            vmid: config.vmid,
            name: config.name.clone(),
            node: self.node.clone(),
            description: config.description.clone(),
            state: observed.state,
            tags: config.tags.clone(),
            vcpus: config.vcpus,
            memory_mib: config.memory_mib,
            cpu_model: config.cpu_model.clone(),
            topology: config.topology,
            machine: config.machine.clone(),
            firmware: config.firmware,
            boot_order: config.boot_order.clone(),
            start_on_boot: observed.autostart,
            guest_agent: config.guest_agent,
            os_id: config.os_id.clone(),
            disks: config.disks.clone(),
            cdroms: config.cdroms.clone(),
            nics: config.nics.clone(),
            current_vcpus: runtime.map(|r| r.vcpus),
            current_memory_mib: runtime.map(|r| r.memory_kib / 1024),
            cpu_time_ns: runtime.map(|r| r.cpu_time_ns),
            uptime_secs: observed
                .started_at
                .map(|started| now_unix().saturating_sub(started)),
            boot_disk: config.boot_disk().map(|d| d.id.clone()),
            total_disk_bytes: config.total_disk_bytes(),
            vnc_socket: domain_xml::vnc_socket_path(config.vmid),
            pending_reboot: pending,
            actions: actions_for(observed.state, &config.name),
        }
    }

    // --- what this node offers --------------------------------------------

    /// The processor models this node can run.
    pub async fn cpu_models(&self) -> Result<CpuModels> {
        self.backend.cpu_models().await
    }

    /// The guest operating systems this node knows about.
    ///
    /// Read once and kept: the database is a package on disk that changes only
    /// when the package does, and it is a thousand small files. The read
    /// happens on a blocking thread because it is file work, not async work.
    pub async fn os_catalog(&self) -> Result<OsCatalog> {
        if let Some(cached) = self.os_catalog.read().await.clone() {
            return Ok(cached);
        }
        let root = self.osinfo_root.clone();
        let catalog = tokio::task::spawn_blocking(move || osinfo::read(root))
            .await
            .map_err(|err| VirtError::Backend(anyhow::anyhow!("{err}")))?;
        *self.os_catalog.write().await = Some(catalog.clone());
        Ok(catalog)
    }

    /// Turn a requested image into the absolute path a domain document points
    /// at, or refuse it.
    ///
    /// The storage domain owns what a media path may look like, so this asks
    /// it rather than joining strings — and it checks the file is actually
    /// there, because a machine defined against media that is not present
    /// boots to a firmware prompt with nothing explaining why.
    async fn resolve_media(&self, cdrom: &CdromCreate) -> Result<Option<String>> {
        if cdrom.source.is_some() {
            return Err(VirtError::Conflict(
                "An optical drive names its image by storage and file name, not by path.".into(),
            ));
        }
        let (Some(storage), Some(image)) = (cdrom.storage.as_deref(), cdrom.image.as_deref())
        else {
            // Both absent is an empty drive, which is a real request. One
            // without the other is a mistake worth saying out loud.
            if cdrom.storage.is_some() || cdrom.image.is_some() {
                return Err(VirtError::Conflict(
                    "An optical drive needs both a storage and an image, or neither.".into(),
                ));
            }
            return Ok(None);
        };
        let path = self.storage.iso_path(storage, image)?;
        if !self.storage.iso_exists(&path).await? {
            return Err(VirtError::NotFound(format!(
                "No image named \"{image}\" in the \"{storage}\" media library."
            )));
        }
        Ok(Some(path))
    }

    // --- create / update / delete ----------------------------------------

    pub async fn create(&self, request: VmCreate) -> Result<VmView> {
        let _guard = self.gate.lock().await;

        let vmid = match request.vmid {
            Some(vmid) => vmid,
            None => self.next_vmid().await?,
        };

        let mut config = VmConfig {
            vmid,
            name: request.name.trim().to_string(),
            description: tidy(request.description),
            vcpus: request.vcpus,
            memory_mib: request.memory_mib,
            cpu_model: request.cpu_model,
            topology: request.topology,
            machine: request
                .machine
                .unwrap_or_else(|| VmConfig::default().machine),
            firmware: request.firmware,
            boot_order: request
                .boot_order
                .unwrap_or_else(|| VmConfig::default().boot_order),
            start_on_boot: request.start_on_boot,
            guest_agent: request.guest_agent,
            tags: tidy_tags(request.tags),
            os_id: tidy(request.os_id),
            disks: Vec::new(),
            cdroms: Vec::new(),
            nics: Vec::new(),
        };

        // Optical drives are files that already exist, so like adapters they
        // cost nothing to build and are checked with everything else rather
        // than after something has been created on the node.
        for cdrom in &request.cdroms {
            let source = self.resolve_media(cdrom).await?;
            let id = config.next_cdrom_target();
            config.cdroms.push(VmCdrom {
                id,
                source,
                boot_index: None,
            });
        }

        // Adapters cost nothing to build, so they exist before validation and
        // are checked with everything else.
        for (index, nic) in request.nics.iter().enumerate() {
            config.nics.push(VmNic {
                id: generate_mac(vmid, index as u32),
                model: nic.model,
                bridge: nic.bridge.clone(),
                vlan_tag: nic.vlan_tag,
                boot_index: None,
            });
        }

        let planned: Vec<PlannedDisk> = request
            .disks
            .iter()
            .map(|disk| PlannedDisk {
                pool: disk.pool.clone(),
                size: disk.size_gib.saturating_mul(GIB),
            })
            .collect();

        let facts = self.facts(None).await?;
        let errors = validate(&config, &planned, &facts);
        if !errors.is_empty() {
            return Err(VirtError::Invalid(errors));
        }

        // Everything below this point creates something on the node, so
        // everything below this point has to be able to undo itself.
        let mut created: Vec<String> = Vec::new();
        for (index, disk) in request.disks.iter().enumerate() {
            let name = format!("vm-{vmid}-disk-{index}");
            let size = disk.size_gib.saturating_mul(GIB);
            let volume = match self
                .storage
                .create_volume(&disk.pool, &name, size, disk.blocksize)
                .await
            {
                Ok(volume) => volume,
                Err(err) => {
                    self.remove_volumes(&created).await;
                    return Err(err.into());
                }
            };
            config.disks.push(VmDisk {
                id: config.next_disk_target(disk.bus),
                bus: disk.bus,
                source: self.storage.device_path(&volume.name),
                size: volume.volsize.unwrap_or(size),
                cache: disk.cache,
                discard: disk.discard,
                boot_index: None,
            });
            created.push(volume.name);
        }

        if let Err(err) = self.backend.define(&domain_xml::render(&config)).await {
            // A machine that did not get defined must not leave its disks
            // behind: the next attempt with the same identifier would find the
            // volumes already there and fail for a reason nobody could guess.
            self.remove_volumes(&created).await;
            return Err(err);
        }

        if config.start_on_boot {
            if let Err(err) = self.backend.set_autostart(&config.name, true).await {
                tracing::warn!(vm = %config.name, "could not set start on boot: {err}");
            }
        }
        if request.start {
            self.start_domain(&config).await?;
        }

        tracing::info!(vmid, name = %config.name, "machine defined");
        self.get(vmid).await
    }

    pub async fn update(&self, vmid: u32, patch: VmPatch) -> Result<VmUpdateResponse> {
        let _guard = self.gate.lock().await;
        let machine = self.machine(vmid).await?;
        let before = machine.config.clone();
        let mut config = before.clone();

        if let Some(name) = patch.name {
            config.name = name.trim().to_string();
        }
        if let Some(description) = patch.description {
            config.description = tidy(Some(description));
        }
        if let Some(vcpus) = patch.vcpus {
            config.vcpus = vcpus;
        }
        if let Some(memory) = patch.memory_mib {
            config.memory_mib = memory;
        }
        if let Some(model) = patch.cpu_model {
            config.cpu_model = model;
        }
        if let Some(topology) = patch.topology {
            config.topology = Some(topology);
        }
        if let Some(machine_type) = patch.machine {
            config.machine = machine_type;
        }
        if let Some(firmware) = patch.firmware {
            config.firmware = firmware;
        }
        if let Some(boot_order) = patch.boot_order {
            config.boot_order = boot_order;
        }
        if let Some(on_boot) = patch.start_on_boot {
            config.start_on_boot = on_boot;
        }
        if let Some(agent) = patch.guest_agent {
            config.guest_agent = agent;
        }
        if let Some(tags) = patch.tags {
            config.tags = tidy_tags(tags);
        }

        let facts = self.facts(Some(vmid)).await?;
        let errors = validate(&config, &[], &facts);
        if !errors.is_empty() {
            return Err(VirtError::Invalid(errors));
        }

        let renamed = config.name != before.name;
        if renamed {
            if machine.observed.state.is_running() {
                return Err(VirtError::Conflict(format!(
                    "\"{}\" is running, and a machine can only be renamed while it is stopped.",
                    before.name
                )));
            }
            if !valid_vm_name(&config.name) {
                return Err(VirtError::invalid(ValidationError::new(
                    ValidationCode::InvalidName,
                    format!("\"{}\" is not a usable machine name.", config.name),
                )));
            }
            self.backend.rename(&before.name, &config.name).await?;
        }

        self.backend.define(&domain_xml::render(&config)).await?;
        if config.start_on_boot != before.start_on_boot {
            self.backend
                .set_autostart(&config.name, config.start_on_boot)
                .await?;
        }

        let (applied_live, pending_reboot) = self
            .reach_the_running_machine(&machine, &before, &config)
            .await;

        Ok(VmUpdateResponse {
            vm: self.get(vmid).await?,
            applied_live,
            pending_reboot,
        })
    }

    /// Try to make the change reach the guest, and report honestly what did.
    ///
    /// Two of these have a live path in the hypervisor and are attempted;
    /// the rest have none at all, because the only thing that writes them is
    /// a whole-document define and a define never touches the running machine.
    /// That is the interface, not a guess about it.
    async fn reach_the_running_machine(
        &self,
        machine: &Machine,
        before: &VmConfig,
        after: &VmConfig,
    ) -> (Vec<String>, Vec<String>) {
        let mut applied = Vec::new();
        let mut pending = Vec::new();

        if !machine.observed.state.is_running() {
            // Nothing is running, so everything is in force the moment the
            // machine next starts — which is not "pending", it is just done.
            return (Vec::new(), Vec::new());
        }
        let name = &after.name;

        if after.memory_mib != before.memory_mib {
            match self.backend.set_memory_live(name, after.memory_mib).await {
                Ok(()) => applied.push(format!("memory set to {} MiB", after.memory_mib)),
                Err(err) => pending.push(format!("memory ({err})")),
            }
        }
        if after.vcpus != before.vcpus {
            match self.backend.set_vcpus_live(name, after.vcpus).await {
                Ok(()) => applied.push(format!("processors set to {}", after.vcpus)),
                Err(err) => pending.push(format!("processors ({err})")),
            }
        }

        for (changed, what) in [
            (after.cpu_model != before.cpu_model, "processor model"),
            (after.topology != before.topology, "processor layout"),
            (after.machine != before.machine, "machine type"),
            (after.firmware != before.firmware, "firmware"),
            (after.boot_order != before.boot_order, "boot order"),
            (after.guest_agent != before.guest_agent, "guest agent"),
        ] {
            if changed {
                pending.push(format!("{what} (takes effect when the machine restarts)"));
            }
        }
        (applied, pending)
    }

    pub async fn delete(
        &self,
        vmid: u32,
        purge_disks: bool,
        ack: Acknowledgements,
    ) -> Result<VmDeleteResponse> {
        let _guard = self.gate.lock().await;
        let machine = self.machine(vmid).await?;
        let name = machine.config.name.clone();

        if let Some(error) = check_destructive(
            "Removing it",
            &name,
            machine.observed.state,
            purge_disks,
            ack,
        ) {
            return Err(VirtError::invalid(error));
        }

        if machine.observed.state.is_running() {
            // Acknowledged above; the guest gets no say, which is what the
            // acknowledgement was about.
            self.backend.destroy(&name).await?;
        }
        self.backend.undefine(&name).await?;

        let volumes: Vec<String> = machine
            .config
            .disks
            .iter()
            .filter_map(|disk| volume_of(&disk.source))
            .collect();

        let mut removed = Vec::new();
        let mut kept = Vec::new();
        for volume in volumes {
            if !purge_disks {
                kept.push(volume);
                continue;
            }
            match self.storage.destroy_volume(&volume).await {
                Ok(()) => removed.push(volume),
                Err(err) => {
                    // The machine is already gone; a volume that will not go
                    // with it is reported rather than making the whole
                    // operation look like a failure.
                    tracing::error!(%volume, "could not remove the volume: {err}");
                    kept.push(volume);
                }
            }
        }

        tracing::info!(vmid, %name, purge_disks, "machine removed");
        Ok(VmDeleteResponse {
            vmid,
            name,
            removed_volumes: removed,
            kept_volumes: kept,
        })
    }

    // --- lifecycle --------------------------------------------------------

    async fn start_domain(&self, config: &VmConfig) -> Result<()> {
        self.backend.start(&config.name).await?;
        // Record when, on the running machine only, so an uptime survives a
        // control-plane restart and disappears by itself when the machine
        // stops. A failure here costs an uptime, not a machine.
        let metadata = domain_xml::live_metadata(config, now_unix());
        if let Err(err) = self
            .backend
            .set_live_metadata(&config.name, &metadata)
            .await
        {
            tracing::warn!(vm = %config.name, "could not record the start time: {err}");
        }
        Ok(())
    }

    pub async fn start(&self, vmid: u32) -> Result<VmView> {
        let _guard = self.gate.lock().await;
        let machine = self.machine(vmid).await?;
        if machine.observed.state.is_running() {
            return Err(VirtError::Conflict(format!(
                "\"{}\" is already running.",
                machine.config.name
            )));
        }
        self.start_domain(&machine.config).await?;
        tracing::info!(vmid, name = %machine.config.name, "machine started");
        self.get(vmid).await
    }

    /// Ask the guest to shut down and let it decide when.
    pub async fn shutdown(&self, vmid: u32) -> Result<VmView> {
        let _guard = self.gate.lock().await;
        let machine = self.running(vmid).await?;
        self.backend.shutdown(&machine.config.name).await?;
        tracing::info!(vmid, name = %machine.config.name, "orderly shutdown requested");
        self.get(vmid).await
    }

    /// Stop it now. The guest gets no warning, so this needs the
    /// acknowledgement.
    pub async fn stop(&self, vmid: u32, ack: Acknowledgements) -> Result<VmView> {
        let _guard = self.gate.lock().await;
        let machine = self.running(vmid).await?;
        if let Some(error) = check_destructive(
            "Stopping it immediately",
            &machine.config.name,
            machine.observed.state,
            false,
            ack,
        ) {
            return Err(VirtError::invalid(error));
        }
        self.backend.destroy(&machine.config.name).await?;
        tracing::warn!(vmid, name = %machine.config.name, "machine stopped immediately");
        self.get(vmid).await
    }

    pub async fn reboot(&self, vmid: u32) -> Result<VmView> {
        let _guard = self.gate.lock().await;
        let machine = self.running(vmid).await?;
        self.backend.reboot(&machine.config.name).await?;
        self.get(vmid).await
    }

    /// Restart it now, without asking the guest — the reset button.
    pub async fn reset(&self, vmid: u32, ack: Acknowledgements) -> Result<VmView> {
        let _guard = self.gate.lock().await;
        let machine = self.running(vmid).await?;
        if let Some(error) = check_destructive(
            "Resetting it",
            &machine.config.name,
            machine.observed.state,
            false,
            ack,
        ) {
            return Err(VirtError::invalid(error));
        }
        self.backend.reset(&machine.config.name).await?;
        tracing::warn!(vmid, name = %machine.config.name, "machine reset");
        self.get(vmid).await
    }

    async fn running(&self, vmid: u32) -> Result<Machine> {
        let machine = self.machine(vmid).await?;
        if !machine.observed.state.is_running() {
            return Err(VirtError::Conflict(format!(
                "\"{}\" is not running.",
                machine.config.name
            )));
        }
        Ok(machine)
    }

    // --- hardware ---------------------------------------------------------

    pub async fn attach_disk(&self, vmid: u32, request: DiskCreate) -> Result<VmUpdateResponse> {
        let _guard = self.gate.lock().await;
        let machine = self.machine(vmid).await?;
        let mut config = machine.config.clone();

        let size = request.size_gib.saturating_mul(GIB);
        let facts = self.facts(Some(vmid)).await?;
        let errors = validate(
            &config,
            &[PlannedDisk {
                pool: request.pool.clone(),
                size,
            }],
            &facts,
        );
        if !errors.is_empty() {
            return Err(VirtError::Invalid(errors));
        }

        let name = format!("vm-{vmid}-disk-{}", config.next_disk_index());
        let volume = self
            .storage
            .create_volume(&request.pool, &name, size, request.blocksize)
            .await?;

        let disk = VmDisk {
            id: config.next_disk_target(request.bus),
            bus: request.bus,
            source: self.storage.device_path(&volume.name),
            size: volume.volsize.unwrap_or(size),
            cache: request.cache,
            discard: request.discard,
            boot_index: None,
        };
        config.disks.push(disk.clone());

        if let Err(err) = self.backend.define(&domain_xml::render(&config)).await {
            self.remove_volumes(&[volume.name]).await;
            return Err(err);
        }

        let (applied_live, pending_reboot) = self
            .live_device(&machine, true, &domain_xml::disk_fragment(&disk), &disk.id)
            .await;

        tracing::info!(vmid, disk = %disk.id, volume = %volume.name, "disk attached");
        Ok(VmUpdateResponse {
            vm: self.get(vmid).await?,
            applied_live,
            pending_reboot,
        })
    }

    pub async fn detach_disk(
        &self,
        vmid: u32,
        id: &str,
        purge: bool,
        ack: Acknowledgements,
    ) -> Result<VmUpdateResponse> {
        let _guard = self.gate.lock().await;
        let machine = self.machine(vmid).await?;
        let disk = machine.config.disk(id).cloned().ok_or_else(|| {
            VirtError::NotFound(format!("\"{id}\" is not a disk on this machine."))
        })?;

        if let Some(error) = check_destructive(
            &format!("Removing \"{id}\""),
            &machine.config.name,
            machine.observed.state,
            purge,
            ack,
        ) {
            return Err(VirtError::invalid(error));
        }

        // Take it away from the running machine first: a volume must never be
        // removed while a guest still has it open.
        let (applied_live, pending_reboot) = self
            .live_device(&machine, false, &domain_xml::disk_fragment(&disk), id)
            .await;

        let mut config = machine.config.clone();
        config.disks.retain(|d| d.id != id);
        self.backend.define(&domain_xml::render(&config)).await?;

        if purge {
            let still_attached = machine.observed.state.is_running() && !pending_reboot.is_empty();
            match volume_of(&disk.source) {
                Some(volume) if still_attached => {
                    return Err(VirtError::Conflict(format!(
                        "\"{id}\" was removed from the configuration, but the running machine \
                         still has it, so \"{volume}\" was left in place. Restart the machine and \
                         remove the volume again."
                    )));
                }
                Some(volume) => self.storage.destroy_volume(&volume).await?,
                None => {
                    return Err(VirtError::Conflict(format!(
                        "\"{}\" is not a volume this appliance created, so it was left in place.",
                        disk.source
                    )))
                }
            }
        }

        tracing::info!(vmid, disk = %id, purge, "disk detached");
        Ok(VmUpdateResponse {
            vm: self.get(vmid).await?,
            applied_live,
            pending_reboot,
        })
    }

    pub async fn attach_nic(&self, vmid: u32, request: NicCreate) -> Result<VmUpdateResponse> {
        let _guard = self.gate.lock().await;
        let machine = self.machine(vmid).await?;
        let mut config = machine.config.clone();

        // The first index whose address this machine does not already use, so
        // removing an adapter frees its address for the next one.
        let index = (0u32..)
            .find(|index| {
                let mac = generate_mac(vmid, *index);
                !config.nics.iter().any(|n| n.id.eq_ignore_ascii_case(&mac))
            })
            .expect("an unbounded search always finds a free address");

        let nic = VmNic {
            id: generate_mac(vmid, index),
            model: request.model,
            bridge: request.bridge.clone(),
            vlan_tag: request.vlan_tag,
            boot_index: None,
        };
        config.nics.push(nic.clone());

        let facts = self.facts(Some(vmid)).await?;
        let errors = validate(&config, &[], &facts);
        if !errors.is_empty() {
            return Err(VirtError::Invalid(errors));
        }

        self.backend.define(&domain_xml::render(&config)).await?;
        let (applied_live, pending_reboot) = self
            .live_device(&machine, true, &domain_xml::nic_fragment(&nic), &nic.id)
            .await;

        tracing::info!(vmid, nic = %nic.id, bridge = %nic.bridge, "adapter attached");
        Ok(VmUpdateResponse {
            vm: self.get(vmid).await?,
            applied_live,
            pending_reboot,
        })
    }

    pub async fn detach_nic(&self, vmid: u32, id: &str) -> Result<VmUpdateResponse> {
        let _guard = self.gate.lock().await;
        let machine = self.machine(vmid).await?;
        let nic = machine.config.nic(id).cloned().ok_or_else(|| {
            VirtError::NotFound(format!("\"{id}\" is not an adapter on this machine."))
        })?;

        let (applied_live, pending_reboot) = self
            .live_device(&machine, false, &domain_xml::nic_fragment(&nic), id)
            .await;

        let mut config = machine.config.clone();
        config.nics.retain(|n| !n.id.eq_ignore_ascii_case(id));
        self.backend.define(&domain_xml::render(&config)).await?;

        tracing::info!(vmid, nic = %id, "adapter detached");
        Ok(VmUpdateResponse {
            vm: self.get(vmid).await?,
            applied_live,
            pending_reboot,
        })
    }

    /// Try to add or remove a device on the running machine, and report what
    /// the hypervisor said. A machine that is not running has nothing pending:
    /// the stored configuration is what it will start with.
    async fn live_device(
        &self,
        machine: &Machine,
        attaching: bool,
        fragment: &str,
        id: &str,
    ) -> (Vec<String>, Vec<String>) {
        if !machine.observed.state.is_running() {
            return (Vec::new(), Vec::new());
        }
        let name = &machine.config.name;
        let result = if attaching {
            self.backend.attach_device_live(name, fragment).await
        } else {
            self.backend.detach_device_live(name, fragment).await
        };
        let verb = if attaching { "added" } else { "removed" };
        match result {
            Ok(()) => (vec![format!("\"{id}\" {verb}")], Vec::new()),
            Err(err) => (
                Vec::new(),
                vec![format!("\"{id}\" {verb} when the machine restarts ({err})")],
            ),
        }
    }

    /// Undo volumes created for a machine that did not survive being made.
    async fn remove_volumes(&self, volumes: &[String]) {
        for volume in volumes {
            if let Err(err) = self.storage.destroy_volume(volume).await {
                tracing::error!(%volume, "could not clean up after a failed create: {err}");
            }
        }
    }
}

/// The dataset behind a disk's device path, if the appliance created it.
fn volume_of(source: &str) -> Option<String> {
    let dataset = source.strip_prefix("/dev/zvol/")?;
    lumen_zfs::is_lumen_volume(dataset).then(|| dataset.to_string())
}

/// An empty string means "nothing", not "a value that is empty" — the console
/// clears a field by sending `""`.
fn tidy(value: Option<String>) -> Option<String> {
    let trimmed = value?.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn tidy_tags(tags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = tags
        .into_iter()
        .map(|tag| tag.trim().to_ascii_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Seconds since the epoch.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;
    use lumen_net::backend::mock::MockBackend as MockNetBackend;
    use lumen_zfs::backend::mock::MockBackend as MockZfsBackend;

    struct Harness {
        service: VirtService,
        virt: Arc<MockBackend>,
        zfs: Arc<MockZfsBackend>,
        state_dir: std::path::PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.state_dir);
        }
    }

    /// A node with one bridge, one pool, and no machines.
    ///
    /// The three domains are wired together exactly as `main` wires them, so
    /// these tests exercise the real dependency direction: compute asks
    /// storage for a volume and networking for a bridge, and neither of those
    /// knows a machine exists.
    async fn harness(tag: &str) -> Harness {
        let state_dir = std::env::temp_dir().join(format!(
            "lumen-virt-service-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&state_dir);

        let virt = Arc::new(MockBackend::appliance());
        let zfs = Arc::new(MockZfsBackend::appliance());
        let net_backend = Arc::new(MockNetBackend::appliance());
        let network = Arc::new(NetworkService::new(net_backend, &state_dir, 60));

        // The networking mock's appliance comes up with the address on a bare
        // adapter, so br0 is created here the way a real first boot does — an
        // adapter has to have something to attach to.
        network.management_bridge().await.unwrap();
        network.confirm().await.unwrap();

        // The media library and the guest database both live under the same
        // temporary root, so no test reads or writes anything the machine
        // running it actually has.
        let storage =
            Arc::new(StorageService::new(zfs.clone()).with_iso_root(state_dir.join("iso")));
        let service = VirtService::new(virt.clone(), storage, network)
            .with_osinfo_root(state_dir.join("osinfo"));

        Harness {
            service,
            virt,
            zfs,
            state_dir,
        }
    }

    /// Put an image in a pool's library, as an upload would have.
    async fn seed_image(harness: &Harness, pool: &str, name: &str) {
        std::fs::create_dir_all(harness.state_dir.join("iso").join(pool)).unwrap();
        std::fs::write(
            harness.state_dir.join("iso").join(pool).join(name),
            b"CD001 pretend installation media",
        )
        .unwrap();
    }

    fn create(name: &str) -> VmCreate {
        VmCreate {
            name: name.into(),
            vmid: None,
            description: None,
            vcpus: 2,
            memory_mib: 4096,
            cpu_model: CpuModel::HostModel,
            topology: None,
            machine: None,
            firmware: Firmware::Uefi,
            boot_order: None,
            start_on_boot: false,
            guest_agent: true,
            tags: Vec::new(),
            os_id: None,
            cdroms: Vec::new(),
            disks: vec![DiskCreate {
                pool: "rpool".into(),
                size_gib: 32,
                bus: DiskBus::VirtioBlk,
                cache: CacheMode::None,
                discard: true,
                blocksize: None,
            }],
            nics: vec![NicCreate {
                bridge: "br0".into(),
                model: NicModel::Virtio,
                vlan_tag: None,
            }],
            start: false,
        }
    }

    #[tokio::test]
    async fn the_first_machine_gets_a_hundred_and_a_disk_and_an_adapter() {
        let h = harness("first").await;
        let vm = h.service.create(create("web01")).await.unwrap();

        assert_eq!(vm.vmid, 100);
        assert_eq!(vm.name, "web01");
        assert_eq!(vm.state, DomainState::ShutOff);
        assert_eq!(vm.disks.len(), 1);
        assert_eq!(vm.disks[0].id, "vda");
        assert_eq!(vm.disks[0].source, "/dev/zvol/rpool/lumen/vm-100-disk-0");
        assert_eq!(vm.nics.len(), 1);
        assert_eq!(vm.nics[0].id, generate_mac(100, 0));
        assert_eq!(vm.nics[0].bridge, "br0");
        assert_eq!(vm.boot_disk.as_deref(), Some("vda"));
        assert_eq!(vm.vnc_socket, "/var/lib/libvirt/qemu/lumen-100-vnc.sock");

        // The volume really exists, and so does the domain.
        assert!(h.zfs.has_dataset("rpool/lumen/vm-100-disk-0"));
        assert!(h.virt.is_defined("web01"));

        // The controls say what can be done right now, and why not.
        assert!(vm.actions.start.allowed);
        assert!(!vm.actions.shutdown.allowed);
        assert!(vm.actions.shutdown.reason.is_some());
    }

    #[tokio::test]
    async fn identifiers_fill_the_lowest_free_slot() {
        let h = harness("ids").await;
        assert_eq!(h.service.next_vmid().await.unwrap(), 100);
        h.service.create(create("a")).await.unwrap();
        h.service.create(create("b")).await.unwrap();
        assert_eq!(h.service.next_vmid().await.unwrap(), 102);

        // Removing the one in the middle frees its number again.
        h.service
            .delete(
                100,
                false,
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(h.service.next_vmid().await.unwrap(), 100);
    }

    /// The path the whole stage exists for.
    #[tokio::test]
    async fn create_start_shutdown_delete() {
        let h = harness("lifecycle").await;
        let vm = h.service.create(create("web01")).await.unwrap();

        let started = h.service.start(vm.vmid).await.unwrap();
        assert_eq!(started.state, DomainState::Running);
        assert_eq!(started.current_vcpus, Some(2));
        assert_eq!(started.current_memory_mib, Some(4096));
        assert!(started.uptime_secs.is_some());
        assert!(!started.actions.start.allowed);
        assert!(started.actions.stop.requires_acknowledgement);

        // Starting it again is a conflict, not a second start.
        assert!(matches!(
            h.service.start(vm.vmid).await.unwrap_err(),
            VirtError::Conflict(_)
        ));

        let stopped = h.service.shutdown(vm.vmid).await.unwrap();
        assert_eq!(stopped.state, DomainState::ShutOff);
        assert_eq!(stopped.uptime_secs, None);

        let removed = h
            .service
            .delete(vm.vmid, false, Acknowledgements::default())
            .await
            .unwrap();
        assert!(!h.virt.is_defined("web01"));
        // The default keeps the data, and says where it still is.
        assert_eq!(removed.removed_volumes, Vec::<String>::new());
        assert_eq!(removed.kept_volumes, vec!["rpool/lumen/vm-100-disk-0"]);
        assert!(h.zfs.has_dataset("rpool/lumen/vm-100-disk-0"));
    }

    #[tokio::test]
    async fn the_disks_only_go_when_the_caller_asks_for_them_to_go() {
        let h = harness("purge").await;
        let vm = h.service.create(create("web01")).await.unwrap();
        let removed = h
            .service
            .delete(
                vm.vmid,
                true,
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(removed.removed_volumes, vec!["rpool/lumen/vm-100-disk-0"]);
        assert!(!h.zfs.has_dataset("rpool/lumen/vm-100-disk-0"));
    }

    #[tokio::test]
    async fn destroying_the_disks_needs_the_acknowledgement() {
        let h = harness("purge-ack").await;
        let vm = h.service.create(create("web01")).await.unwrap();
        let err = h
            .service
            .delete(vm.vmid, true, Acknowledgements::default())
            .await
            .unwrap_err();
        match err {
            VirtError::Invalid(errors) => assert_eq!(
                errors[0].code,
                ValidationCode::UnacknowledgedDestructiveOperation
            ),
            other => panic!("expected the guard, got {other:?}"),
        }
        // Nothing happened on the way to being refused.
        assert!(h.virt.is_defined("web01"));
        assert!(h.zfs.has_dataset("rpool/lumen/vm-100-disk-0"));
    }

    #[tokio::test]
    async fn stopping_a_running_machine_without_warning_it_needs_the_acknowledgement() {
        let h = harness("stop-ack").await;
        let vm = h.service.create(create("web01")).await.unwrap();
        h.service.start(vm.vmid).await.unwrap();

        assert!(matches!(
            h.service
                .stop(vm.vmid, Acknowledgements::default())
                .await
                .unwrap_err(),
            VirtError::Invalid(_)
        ));
        assert_eq!(h.virt.state_of("web01"), Some(DomainState::Running));

        let stopped = h
            .service
            .stop(
                vm.vmid,
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(stopped.state, DomainState::ShutOff);
    }

    #[tokio::test]
    async fn a_rejected_machine_leaves_nothing_behind() {
        let h = harness("rejected").await;
        let mut request = create("web01");
        request.nics[0].bridge = "br9".into();
        let err = h.service.create(request).await.unwrap_err();
        match err {
            VirtError::Invalid(errors) => {
                assert_eq!(errors[0].code, ValidationCode::UnknownBridge)
            }
            other => panic!("expected a validation failure, got {other:?}"),
        }
        assert!(h.virt.names().is_empty());
        assert!(!h.zfs.has_dataset("rpool/lumen/vm-100-disk-0"));
    }

    /// The volume is created before the machine is defined, so a define that
    /// fails has to take the volume with it.
    #[tokio::test]
    async fn a_machine_that_cannot_be_defined_takes_its_volumes_with_it() {
        let h = harness("rollback").await;
        let mut request = create("web01");
        // A machine type the document renderer will emit and the mock's parser
        // accepts, but with a name the mock refuses to define.
        request.name = "web01".into();
        h.service.create(request).await.unwrap();

        // Now the second machine collides on the volume name only if the
        // identifier repeats, which the allocator prevents — so instead force
        // the storage layer to fail and check the machine did not survive it.
        h.zfs.fail_next_create("no space left");
        let err = h.service.create(create("web02")).await.unwrap_err();
        assert!(matches!(err, VirtError::Backend(_)), "{err:?}");
        assert!(!h.virt.is_defined("web02"));
    }

    #[tokio::test]
    async fn a_disk_can_be_attached_and_detached_again() {
        let h = harness("disks").await;
        let vm = h.service.create(create("web01")).await.unwrap();

        let updated = h
            .service
            .attach_disk(
                vm.vmid,
                DiskCreate {
                    pool: "rpool".into(),
                    size_gib: 16,
                    bus: DiskBus::VirtioScsi,
                    cache: CacheMode::None,
                    discard: true,
                    blocksize: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.vm.disks.len(), 2);
        assert_eq!(updated.vm.disks[1].id, "sda");
        assert_eq!(
            updated.vm.disks[1].source,
            "/dev/zvol/rpool/lumen/vm-100-disk-1"
        );
        // The machine is not running, so nothing is waiting on anything.
        assert!(updated.pending_reboot.is_empty());
        assert!(updated.applied_live.is_empty());

        let after = h
            .service
            .detach_disk(
                vm.vmid,
                "sda",
                true,
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(after.vm.disks.len(), 1);
        assert!(!h.zfs.has_dataset("rpool/lumen/vm-100-disk-1"));
    }

    #[tokio::test]
    async fn a_disk_larger_than_the_pool_never_becomes_a_volume() {
        let h = harness("disk-too-big").await;
        let vm = h.service.create(create("web01")).await.unwrap();
        let err = h
            .service
            .attach_disk(
                vm.vmid,
                DiskCreate {
                    pool: "rpool".into(),
                    size_gib: 4096,
                    bus: DiskBus::VirtioBlk,
                    cache: CacheMode::None,
                    discard: true,
                    blocksize: None,
                },
            )
            .await
            .unwrap_err();
        match err {
            VirtError::Invalid(errors) => {
                assert_eq!(errors[0].code, ValidationCode::DiskExceedsPool)
            }
            other => panic!("expected a validation failure, got {other:?}"),
        }
        assert!(!h.zfs.has_dataset("rpool/lumen/vm-100-disk-1"));
    }

    #[tokio::test]
    async fn an_adapter_can_be_attached_and_detached_again() {
        let h = harness("nics").await;
        let vm = h.service.create(create("web01")).await.unwrap();
        let updated = h
            .service
            .attach_nic(
                vm.vmid,
                NicCreate {
                    bridge: "br0".into(),
                    model: NicModel::Virtio,
                    vlan_tag: Some(100),
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.vm.nics.len(), 2);
        assert_eq!(updated.vm.nics[1].id, generate_mac(100, 1));
        assert_eq!(updated.vm.nics[1].vlan_tag, Some(100));

        let after = h
            .service
            .detach_nic(vm.vmid, &generate_mac(100, 1))
            .await
            .unwrap();
        assert_eq!(after.vm.nics.len(), 1);
    }

    /// The distinction the console exists to show: what reached the guest and
    /// what is waiting for a restart, with the hypervisor's own answer behind
    /// both.
    #[tokio::test]
    async fn a_change_the_running_machine_cannot_take_is_reported_as_waiting() {
        let h = harness("pending").await;
        let vm = h.service.create(create("web01")).await.unwrap();
        h.service.start(vm.vmid).await.unwrap();

        // Memory the running machine can take.
        let grew = h
            .service
            .update(
                vm.vmid,
                VmPatch {
                    memory_mib: Some(6144),
                    ..VmPatch::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(grew.applied_live.len(), 1);
        assert!(grew.pending_reboot.is_empty());
        assert_eq!(grew.vm.current_memory_mib, Some(6144));

        // …and one it cannot.
        h.virt.refuse_live_changes("cannot exceed the boot maximum");
        let waited = h
            .service
            .update(
                vm.vmid,
                VmPatch {
                    memory_mib: Some(8192),
                    firmware: Some(Firmware::Bios),
                    ..VmPatch::default()
                },
            )
            .await
            .unwrap();
        assert!(waited.applied_live.is_empty());
        assert!(
            waited.pending_reboot.iter().any(|p| p.contains("memory")),
            "{:?}",
            waited.pending_reboot
        );
        // Firmware has no live path at all, so it is always a restart.
        assert!(
            waited.pending_reboot.iter().any(|p| p.contains("firmware")),
            "{:?}",
            waited.pending_reboot
        );

        // The stored configuration has it either way, and the view says the
        // running machine has not caught up.
        assert_eq!(waited.vm.memory_mib, 8192);
        assert_eq!(waited.vm.current_memory_mib, Some(6144));
        assert!(!waited.vm.pending_reboot.is_empty());

        // …and a restart is what settles it.
        h.virt.allow_live_changes();
        h.service.shutdown(vm.vmid).await.unwrap();
        let restarted = h.service.start(vm.vmid).await.unwrap();
        assert_eq!(restarted.current_memory_mib, Some(8192));
        assert!(restarted.pending_reboot.is_empty());
    }

    #[tokio::test]
    async fn a_stopped_machine_has_nothing_pending_because_nothing_is_running() {
        let h = harness("stopped-update").await;
        let vm = h.service.create(create("web01")).await.unwrap();
        let updated = h
            .service
            .update(
                vm.vmid,
                VmPatch {
                    memory_mib: Some(8192),
                    firmware: Some(Firmware::Bios),
                    ..VmPatch::default()
                },
            )
            .await
            .unwrap();
        assert!(updated.pending_reboot.is_empty());
        assert!(updated.applied_live.is_empty());
        assert_eq!(updated.vm.memory_mib, 8192);
        assert_eq!(updated.vm.firmware, Firmware::Bios);
    }

    #[tokio::test]
    async fn description_tags_and_start_on_boot_round_trip_through_the_hypervisor() {
        let h = harness("options").await;
        let vm = h.service.create(create("web01")).await.unwrap();
        let updated = h
            .service
            .update(
                vm.vmid,
                VmPatch {
                    description: Some("  Public web server  ".into()),
                    tags: Some(vec!["Production".into(), "web".into(), "production".into()]),
                    start_on_boot: Some(true),
                    ..VmPatch::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            updated.vm.description.as_deref(),
            Some("Public web server"),
            "surrounding space is not part of a description"
        );
        // Tags are lower-cased, sorted, and de-duplicated.
        assert_eq!(updated.vm.tags, vec!["production", "web"]);
        assert!(updated.vm.start_on_boot);

        // …and it really is the hypervisor's own flag, not something we kept.
        assert!(h.virt.domain("web01").await.unwrap().autostart);

        // Clearing the description is sending an empty one.
        let cleared = h
            .service
            .update(
                vm.vmid,
                VmPatch {
                    description: Some(String::new()),
                    ..VmPatch::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(cleared.vm.description, None);
    }

    #[tokio::test]
    async fn a_machine_can_only_be_renamed_while_it_is_stopped() {
        let h = harness("rename").await;
        let vm = h.service.create(create("web01")).await.unwrap();
        h.service.start(vm.vmid).await.unwrap();
        assert!(matches!(
            h.service
                .update(
                    vm.vmid,
                    VmPatch {
                        name: Some("web02".into()),
                        ..VmPatch::default()
                    }
                )
                .await
                .unwrap_err(),
            VirtError::Conflict(_)
        ));

        h.service.shutdown(vm.vmid).await.unwrap();
        let renamed = h
            .service
            .update(
                vm.vmid,
                VmPatch {
                    name: Some("web02".into()),
                    ..VmPatch::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.vm.name, "web02");
        assert!(h.virt.is_defined("web02"));
        assert!(!h.virt.is_defined("web01"), "the old name is gone");
    }

    #[tokio::test]
    async fn two_machines_cannot_share_a_name_or_an_identifier() {
        let h = harness("collisions").await;
        h.service.create(create("web01")).await.unwrap();

        let err = h.service.create(create("web01")).await.unwrap_err();
        match err {
            VirtError::Invalid(errors) => {
                assert!(errors
                    .iter()
                    .any(|e| e.code == ValidationCode::DuplicateName))
            }
            other => panic!("expected a validation failure, got {other:?}"),
        }

        let mut same_id = create("web02");
        same_id.vmid = Some(100);
        let err = h.service.create(same_id).await.unwrap_err();
        match err {
            VirtError::Invalid(errors) => {
                assert!(errors
                    .iter()
                    .any(|e| e.code == ValidationCode::DuplicateVmid))
            }
            other => panic!("expected a validation failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn machines_are_listed_by_identifier_and_grouped_by_node() {
        let h = harness("list").await;
        let mut second = create("b");
        second.vmid = Some(105);
        h.service.create(second).await.unwrap();
        h.service.create(create("a")).await.unwrap();

        let response = h.service.list().await.unwrap();
        assert_eq!(response.nodes.len(), 1);
        let ids: Vec<u32> = response.nodes[0].vms.iter().map(|vm| vm.vmid).collect();
        assert_eq!(ids, vec![100, 105]);
        assert_eq!(response.nodes[0].node, h.service.node());
    }

    /// A machine somebody defined with the hypervisor's own tools is not
    /// Lumen's to show, but the node still has its name.
    #[tokio::test]
    async fn a_domain_lumen_did_not_define_is_skipped_but_still_owns_its_name() {
        let h = harness("foreign").await;
        h.virt
            .define(
                "<domain type='kvm'><name>legacy</name><memory unit='KiB'>1048576</memory>\
                 <vcpu>1</vcpu><os><type arch='x86_64' machine='q35'>hvm</type></os>\
                 <devices/></domain>",
            )
            .await
            .unwrap();

        assert!(h.service.list().await.unwrap().nodes[0].vms.is_empty());

        let mut request = create("legacy");
        request.disks.clear();
        request.nics.clear();
        let err = h.service.create(request).await.unwrap_err();
        match err {
            VirtError::Invalid(errors) => {
                assert!(errors
                    .iter()
                    .any(|e| e.code == ValidationCode::DuplicateName))
            }
            other => panic!("expected a name collision, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_machine_that_is_not_there_is_a_not_found() {
        let h = harness("missing").await;
        assert!(matches!(
            h.service.get(999).await.unwrap_err(),
            VirtError::NotFound(_)
        ));
        assert!(matches!(
            h.service.start(999).await.unwrap_err(),
            VirtError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn a_machine_can_be_defined_and_started_in_one_request() {
        let h = harness("start-now").await;
        let mut request = create("web01");
        request.start = true;
        request.start_on_boot = true;
        let vm = h.service.create(request).await.unwrap();
        assert_eq!(vm.state, DomainState::Running);
        assert!(vm.start_on_boot);
    }

    /// A machine built to install an operating system: media in the first
    /// drive, the driver disc in the second, booting off the media. All of it
    /// has to survive the document, because the document is the database.
    #[tokio::test]
    async fn a_machine_is_defined_with_its_installation_media() {
        let h = harness("media").await;
        seed_image(&h, "rpool", "almalinux-10.iso").await;
        seed_image(&h, "rpool", "virtio-win.iso").await;

        let mut request = create("win01");
        request.os_id = Some("http://microsoft.com/win/11".into());
        request.boot_order = Some(vec![BootDevice::Cdrom, BootDevice::Disk]);
        request.cdroms = vec![
            CdromCreate {
                storage: Some("rpool".into()),
                image: Some("almalinux-10.iso".into()),
                source: None,
            },
            CdromCreate {
                storage: Some("rpool".into()),
                image: Some("virtio-win.iso".into()),
                source: None,
            },
        ];
        let vm = h.service.create(request).await.unwrap();

        assert_eq!(vm.cdroms.len(), 2);
        // Targets are allocated around the disk, which is on virtio and takes
        // the other prefix.
        assert_eq!(vm.disks[0].id, "vda");
        assert_eq!(vm.cdroms[0].id, "sda");
        assert_eq!(vm.cdroms[1].id, "sdb");
        assert!(vm.cdroms[0]
            .source
            .as_deref()
            .unwrap()
            .ends_with("almalinux-10.iso"));
        assert_eq!(vm.os_id.as_deref(), Some("http://microsoft.com/win/11"));
        // The media boots first, and it is the drives that carry the numbers.
        assert_eq!(vm.boot_order, vec![BootDevice::Cdrom, BootDevice::Disk]);
        assert_eq!(vm.cdroms[0].boot_index, Some(1));
        assert_eq!(vm.disks[0].boot_index, Some(3));
    }

    /// The drive is empty, which is a real thing to ask for, and stays a drive.
    #[tokio::test]
    async fn a_drive_with_nothing_in_it_is_still_a_drive() {
        let h = harness("empty-drive").await;
        let mut request = create("web01");
        request.cdroms = vec![CdromCreate {
            storage: None,
            image: None,
            source: None,
        }];
        let vm = h.service.create(request).await.unwrap();
        assert_eq!(vm.cdroms.len(), 1);
        assert_eq!(vm.cdroms[0].source, None);
        // Nothing in it, so it is not in the boot order.
        assert_eq!(vm.boot_order, vec![BootDevice::Disk]);
    }

    /// Media that is not on the node must be refused *before* the machine is
    /// defined: a domain pointing at a file that is not there boots to a
    /// firmware prompt with nothing to explain it.
    #[tokio::test]
    async fn media_that_is_not_on_the_node_is_refused_before_anything_is_created() {
        let h = harness("missing-media").await;
        let cases = [
            ("no such image", Some("rpool"), Some("nothere.iso")),
            ("no such pool", Some("tank"), Some("almalinux-10.iso")),
            (
                "a name that is a path",
                Some("rpool"),
                Some("../../etc/passwd.iso"),
            ),
            ("a storage with no image", Some("rpool"), None),
            ("an image with no storage", None, Some("almalinux-10.iso")),
        ];
        for (label, storage, image) in cases {
            let mut request = create("web01");
            request.cdroms = vec![CdromCreate {
                storage: storage.map(String::from),
                image: image.map(String::from),
                source: None,
            }];
            assert!(h.service.create(request).await.is_err(), "{label}");
        }
        // And nothing was left behind on the way to being refused.
        assert!(h.service.list().await.unwrap().nodes[0].vms.is_empty());
        assert!(!h.zfs.has_dataset("rpool/lumen/vm-100-disk-0"));

        // A caller trying to name a path directly is refused too — the storage
        // domain owns where media may live, and this is the only door.
        let mut request = create("web01");
        request.cdroms = vec![CdromCreate {
            storage: None,
            image: None,
            source: Some("/etc/shadow".into()),
        }];
        assert!(h.service.create(request).await.is_err());
    }

    #[tokio::test]
    async fn the_node_reports_what_processors_and_guests_it_offers() {
        let h = harness("catalogues").await;

        let cpus = h.service.cpu_models().await.unwrap();
        assert!(cpus.host_passthrough);
        assert_eq!(cpus.host_model.as_deref(), Some("EPYC-Rome"));
        assert!(cpus.usable("EPYC"));
        assert!(!cpus.usable("Skylake-Server"));

        // No database installed under the test root, so the catalogue is empty
        // and says why rather than failing the request.
        let guests = h.service.os_catalog().await.unwrap();
        assert!(guests.is_empty());
        assert!(guests.reason.is_some());
    }
}
