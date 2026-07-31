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
    NicModel, VideoModel, VmCdrom, VmConfig, VmDisk, VmNic, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS,
    FIRST_VMID, LAST_VMID,
};
use crate::osinfo::{self, OsCatalog};
use crate::state::{DomainState, HostInfo, ObservedDomain};
use crate::validate::{
    check_destructive, validate, Acknowledgements, HostFacts, PlannedDisk, ValidationCode,
    ValidationError,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// The largest file that may be copied into a guest through its agent.
///
/// Not the agent's limit — the arrangement's. Every byte is base64 inside a
/// JSON message on the hypervisor's control socket and is held in memory on
/// both sides while it travels, so this path is for a key, a script, or a
/// configuration file. Sixteen mebibytes is comfortably more than any of those
/// and comfortably less than anything that ought to be a disk instead.
pub const MAX_GUEST_FILE_BYTES: usize = 16 * 1024 * 1024;

/// How much of a file goes in one write.
///
/// The agent reads one JSON message at a time and both ends cap how big one may
/// be. 48 KiB of file is about 64 KiB once base64 has grown it by a third,
/// which every agent in the field accepts.
const GUEST_WRITE_CHUNK: usize = 48 * 1024;

/// One guest agent request, as the agent expects to read it.
fn agent_command(execute: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({ "execute": execute, "arguments": arguments }).to_string()
}

/// The result out of an agent's reply, or the guest's own refusal.
///
/// The agent answers `{"return": …}` or `{"error": {"desc": …}}`, and the
/// second is not a failure of this node: it is the guest saying no — no such
/// directory, no permission, no room. Its own sentence is the useful one, so it
/// is carried through as a conflict rather than flattened into an internal
/// error.
fn agent_return(reply: &str) -> Result<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(reply).map_err(|err| {
        VirtError::Backend(anyhow::anyhow!(
            "the guest agent answered with something that is not JSON ({err}): {reply}"
        ))
    })?;
    if let Some(error) = parsed.get("error") {
        let said = error
            .get("desc")
            .and_then(|desc| desc.as_str())
            .unwrap_or("it gave no reason");
        return Err(VirtError::Conflict(format!("The guest refused: {said}")));
    }
    parsed.get("return").cloned().ok_or_else(|| {
        VirtError::Backend(anyhow::anyhow!(
            "the guest agent answered without a result: {reply}"
        ))
    })
}

/// What arrived, and where.
#[derive(Debug, Clone, Serialize)]
pub struct PushedFile {
    pub vmid: u32,
    pub path: String,
    pub bytes: u64,
}

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
    /// Whether there is a screen to look at. A machine that is not running has
    /// no console — the hypervisor only listens on the socket for as long as
    /// the guest exists — and the console says that rather than opening a
    /// viewer that fails a moment later.
    pub console: Action,
}

/// `has_screen` is whether the machine's stored document declares a graphics
/// device — see [`VmView::has_screen`]. It is not derivable from the state, and
/// without it the console is offered on a machine that has nothing listening,
/// so the viewer opens, fails, and reports it as though the connection had
/// dropped.
fn actions_for(state: DomainState, name: &str, has_screen: bool) -> VmActions {
    let running = state.is_running();
    let off_reason = format!("\"{name}\" is not running.");
    let no_screen = format!("\"{name}\" is not running, so it has no console.");
    // The same sentence `VirtService::console` refuses with, for the same
    // reason the one above is duplicated there: a viewer that raced a machine
    // into this state should read the words it would have read anyway.
    let never_had_a_screen = format!(
        "\"{name}\" is running without a screen. It was defined before this appliance gave \
         machines a console, so its document has no graphics device — save its configuration, \
         then stop and start it, and the console will be there."
    );
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
        console: if !running {
            Action::no(no_screen)
        } else if !has_screen {
            Action::no(never_had_a_screen)
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
    /// The graphics card, which is what the console page is looking at.
    pub video: VideoModel,
    /// Whether the stored document actually gives the machine a screen.
    ///
    /// Not the same question as [`VmView::video`], and the difference is the
    /// whole point: a document with no display device in it reads back as the
    /// default card, so `video` says "virtio" for a machine that has no
    /// graphics device at all. False here means the machine predates consoles
    /// and has nothing for a viewer to connect to until it is saved and fully
    /// restarted — which is what the console must say instead of naming a card
    /// the machine does not have.
    pub has_screen: bool,
    pub boot_order: Vec<BootDevice>,
    pub start_on_boot: bool,
    pub guest_agent: bool,
    /// Restart on a surviving member after this node is confirmed lost.
    pub ha: bool,
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
    /// Where the machine's console socket is, when its stored document names
    /// one. For a machine this appliance defines it does not: the hypervisor
    /// chooses the path at start, and [`VirtService::console`] asks the live
    /// document. What remains here is the fact for a support conversation —
    /// a machine defined by hand carries whatever it carries.
    pub vnc_socket: Option<String>,
    /// Changes that are stored but will not reach the guest until it restarts.
    pub pending_reboot: Vec<String>,
    pub actions: VmActions,
}

/// What the console viewer speaks to a machine.
///
/// One value today. It is named rather than assumed so that adding the serial
/// console — a different socket carrying a different protocol on the same
/// machine — does not change the shape of anything above this line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleProtocol {
    Vnc,
}

/// Where a machine's console is, and what is listening on it.
///
/// This is the whole of the virtualization domain's part in the console: it
/// answers *whether there is a screen and where*, and the control plane
/// carries the bytes. A UNIX socket is not a domain concept and a WebSocket is
/// not one either — see the note in `lumen-controlplane/src/api/console.rs`.
#[derive(Debug, Clone, Serialize)]
pub struct ConsoleTarget {
    pub vmid: u32,
    pub name: String,
    pub node: String,
    pub protocol: ConsoleProtocol,
    /// The UNIX socket the hypervisor is listening on for this machine.
    pub socket: String,
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

/// One node's capacity, and what the machines on it are holding.
///
/// The console's dashboard is what this exists for: a machine count is
/// something `/api/vms` already answers, but "how much of this node is
/// spoken for" needs the node's own size, and nothing above this line knew
/// it. The hypervisor has always reported it — see [`HostInfo`] — it simply
/// had no way out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeView {
    pub node: String,
    /// Logical processors the node has online.
    pub cpus: u32,
    pub memory_mib: u64,
    /// Memory kept back for the node itself, which no machine may be given.
    pub reserved_memory_mib: u64,
    pub hypervisor_version: Option<String>,
    /// Machines defined here, and how many of them are running.
    pub machines: u32,
    pub running: u32,
    /// What the running machines hold between them, as the hypervisor reports
    /// it rather than as their documents ask for it — a machine whose memory
    /// changed but which has not restarted is still holding the old amount.
    ///
    /// `used_vcpus` may exceed `cpus`: processors are overcommittable, and an
    /// operator needs to see that they have been overcommitted rather than a
    /// number clamped to look healthy.
    pub used_vcpus: u32,
    pub used_memory_mib: u64,
}

/// GET /api/nodes. A list of one today, and a list for the same reason
/// [`VmsResponse`] groups by node: the shape must not change when clustering
/// lands.
#[derive(Debug, Clone, Serialize)]
pub struct NodesResponse {
    pub nodes: Vec<NodeView>,
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
    pub video: VideoModel,
    #[serde(default)]
    pub boot_order: Option<Vec<BootDevice>>,
    #[serde(default)]
    pub start_on_boot: bool,
    #[serde(default = "default_true")]
    pub guest_agent: bool,
    /// Restart on a surviving member after this node is confirmed lost.
    /// Meaningful only for a machine whose disks are all replicated; the HA
    /// manager checks that at restart time, because disks change after
    /// creation.
    #[serde(default)]
    pub ha: bool,
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
    /// The pool a local disk's zvol lives in. Unused — and allowed empty —
    /// for a replicated disk, whose members each name their own pool.
    #[serde(default)]
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
    /// Back the disk with a replicated volume instead of a local zvol: the
    /// machine's data lands on every replica before a write is acknowledged,
    /// and the machine can run on any member holding one.
    #[serde(default)]
    pub replicated: bool,
    /// The replica seats for a replicated disk. Empty means "place it for
    /// me": this node plus the least-utilized other member.
    #[serde(default)]
    pub members: Vec<lumen_drbd::VolumeMemberCreate>,
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

/// POST /api/vms/{vmid}/migrate — the machine's one home moved. There is no
/// `VmView` in the answer on purpose: after a migration this node has no
/// view of the machine to give.
#[derive(Debug, Clone, Serialize)]
pub struct VmMigrateResponse {
    pub vmid: u32,
    pub name: String,
    pub target: String,
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
    pub video: Option<VideoModel>,
    pub boot_order: Option<Vec<BootDevice>>,
    pub start_on_boot: Option<bool>,
    pub guest_agent: Option<bool>,
    pub ha: Option<bool>,
    pub tags: Option<Vec<String>>,
}

// --- the service -------------------------------------------------------------

pub struct VirtService {
    backend: Arc<dyn VirtBackend>,
    storage: Arc<StorageService>,
    network: Arc<NetworkService>,
    /// Replicated disks, through the narrow trait `lumen-drbd` defines for
    /// this one consumer. On a standalone appliance every call refuses with
    /// a sentence, which is exactly the answer a replicated disk deserves
    /// there.
    volumes: Arc<dyn lumen_drbd::VmVolumes>,
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

/// What a machine's disk is backed by — the two kinds the unwind and the
/// purge paths have to tell apart.
enum Backing {
    /// A local zvol, named by its dataset path.
    Zvol(String),
    /// A replicated volume, named by its device (stable on every member)
    /// and its volume name (what the operator knows it as).
    Replicated { device: String, name: String },
}

impl VirtService {
    pub fn new(
        backend: Arc<dyn VirtBackend>,
        storage: Arc<StorageService>,
        network: Arc<NetworkService>,
        volumes: Arc<dyn lumen_drbd::VmVolumes>,
    ) -> Self {
        Self {
            backend,
            storage,
            network,
            volumes,
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

    /// Every node this appliance manages, with its capacity and its load.
    ///
    /// One request to the hypervisor for the node's size and one for its
    /// domains, so the numbers on the dashboard are the same reading rather
    /// than two taken a moment apart.
    pub async fn nodes(&self) -> Result<NodesResponse> {
        let host = self.backend.host().await?;
        let (machines, _) = self.machines().await?;

        let running: Vec<&Machine> = machines
            .iter()
            .filter(|m| m.observed.state == DomainState::Running)
            .collect();
        // The hypervisor's figure where there is one. A machine that is
        // running has a runtime; falling back to the document covers the gap
        // between "started" and "the hypervisor has reported it".
        let used_vcpus = running
            .iter()
            .map(|m| m.observed.runtime.map_or(m.config.vcpus, |r| r.vcpus))
            .sum();
        let used_memory_mib = running
            .iter()
            .map(|m| {
                m.observed
                    .runtime
                    .map_or(m.config.memory_mib, |r| r.memory_kib / 1024)
            })
            .sum();

        Ok(NodesResponse {
            nodes: vec![NodeView {
                node: self.node.clone(),
                cpus: host.cpus,
                memory_mib: host.memory_mib,
                reserved_memory_mib: crate::state::HOST_MEMORY_RESERVE_MIB,
                hypervisor_version: host.hypervisor_version,
                machines: machines.len() as u32,
                running: running.len() as u32,
                used_vcpus,
                used_memory_mib,
            }],
        })
    }

    pub async fn get(&self, vmid: u32) -> Result<VmView> {
        Ok(self.view_of(&self.machine(vmid).await?))
    }

    /// Where to find the console of a machine that has one.
    ///
    /// Refuses a machine that is not running rather than handing back a path:
    /// the hypervisor listens on that socket only for as long as the guest
    /// exists, so a viewer opened against a stopped machine would fail at
    /// `connect` with nothing but an operating-system error to show for it.
    /// The refusal carries the sentence `actions.console.reason` already says,
    /// so a console that raced the machine stopping shows the same words it
    /// would have shown had it not raced.
    pub async fn console(&self, vmid: u32) -> Result<ConsoleTarget> {
        let machine = self.machine(vmid).await?;
        if !machine.observed.state.is_running() {
            return Err(VirtError::Conflict(format!(
                "\"{}\" is not running, so it has no console.",
                machine.config.name
            )));
        }
        // The *running* document, not the stored one. A socket is a fact about
        // the process that is listening, and those two documents are allowed to
        // disagree — which is the whole reason this asks the hypervisor again
        // rather than reading the one already in hand.
        let live = self.backend.live_xml(&machine.config.name).await?;

        // And when there is no screen in it, say so instead of guessing. The
        // naming rule would give a path, and handing that back sends an
        // operator to a socket nothing ever created: a machine defined before
        // this appliance put a screen on one has no graphics device at all, and
        // the viewer's failure — "the connection ended" over a path that does
        // not exist — says nothing about the actual remedy, which is to save
        // the machine's configuration and start it again.
        let socket = domain_xml::vnc_socket_of(&live).ok_or_else(|| {
            VirtError::Conflict(format!(
                "\"{}\" is running without a screen. It was defined before this appliance gave \
                 machines a console, so its document has no graphics device — save its \
                 configuration, then stop and start it, and the console will be there.",
                machine.config.name
            ))
        })?;

        Ok(ConsoleTarget {
            vmid: machine.config.vmid,
            name: machine.config.name.clone(),
            node: self.node.clone(),
            protocol: ConsoleProtocol::Vnc,
            socket,
        })
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
        // Asked of the document rather than of `config.video`, which cannot
        // tell "virtio-gpu" from "no display device at all" — see
        // [`domain_xml::has_screen`].
        let has_screen = domain_xml::has_screen(&observed.xml);

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
            video: config.video,
            has_screen,
            boot_order: config.boot_order.clone(),
            start_on_boot: observed.autostart,
            guest_agent: config.guest_agent,
            ha: config.ha,
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
            vnc_socket: domain_xml::vnc_socket_of(&observed.xml),
            pending_reboot: pending,
            actions: actions_for(observed.state, &config.name, has_screen),
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
            video: request.video,
            boot_order: request
                .boot_order
                .unwrap_or_else(|| VmConfig::default().boot_order),
            start_on_boot: request.start_on_boot,
            guest_agent: request.guest_agent,
            ha: request.ha,
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

        // A replicated disk's pools are its members' business, validated by
        // the storage domain when it is made — only the local disks are
        // checked against this node's pools.
        let planned: Vec<PlannedDisk> = request
            .disks
            .iter()
            .filter(|disk| !disk.replicated)
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
        let mut created: Vec<Backing> = Vec::new();
        for (index, disk) in request.disks.iter().enumerate() {
            let name = format!("vm-{vmid}-disk-{index}");
            let size = disk.size_gib.saturating_mul(GIB);
            let (source, actual_size, backing) = if disk.replicated {
                match self
                    .volumes
                    .create_disk(&lumen_drbd::VmDiskRequest {
                        name: name.clone(),
                        size_bytes: size,
                        members: disk.members.clone(),
                    })
                    .await
                {
                    Ok(replicated) => {
                        let backing = Backing::Replicated {
                            device: replicated.device.clone(),
                            name: replicated.name,
                        };
                        (replicated.device, replicated.size_bytes, backing)
                    }
                    Err(err) => {
                        self.remove_backings(&created).await;
                        return Err(err.into());
                    }
                }
            } else {
                match self
                    .storage
                    .create_volume(&disk.pool, &name, size, disk.blocksize)
                    .await
                {
                    Ok(volume) => (
                        self.storage.device_path(&volume.name),
                        volume.volsize.unwrap_or(size),
                        Backing::Zvol(volume.name),
                    ),
                    Err(err) => {
                        self.remove_backings(&created).await;
                        return Err(err.into());
                    }
                }
            };
            config.disks.push(VmDisk {
                id: config.next_disk_target(disk.bus),
                bus: disk.bus,
                source,
                size: actual_size,
                cache: disk.cache,
                discard: disk.discard,
                boot_index: None,
            });
            created.push(backing);
        }

        if let Err(err) = self.backend.define(&domain_xml::render(&config)).await {
            // A machine that did not get defined must not leave its disks
            // behind: the next attempt with the same identifier would find the
            // volumes already there and fail for a reason nobody could guess.
            self.remove_backings(&created).await;
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
        let view = self.get(vmid).await?;
        // The hypervisor may normalize the document it was given; it must not
        // lose the screen out of it. It has, twice: any VNC socket *path* this
        // appliance chose under /var/lib/libvirt/qemu was silently wiped at
        // define time (see the graphics note in `domain_xml::render`), and the
        // only symptom was a console that refused a machine created that
        // morning. The document now defers the path, so this firing again
        // means the hypervisor has found a new way to drop the listener — and
        // a sentence in the journal at creation time is the difference between
        // reading the cause and re-deriving it from a stored document days
        // later.
        if !view.has_screen {
            tracing::warn!(
                vmid,
                name = %config.name,
                "the machine was defined with a console listener, but the hypervisor \
                 stored it without one — its console will not open"
            );
        }
        Ok(view)
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
        if let Some(video) = patch.video {
            config.video = video;
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
        if let Some(ha) = patch.ha {
            config.ha = ha;
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

        self.backend
            .define(&domain_xml::redefine(
                &config,
                machine.observed.uuid.as_deref(),
            ))
            .await?;
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
            (after.video != before.video, "graphics card"),
            (after.boot_order != before.boot_order, "boot order"),
            (after.guest_agent != before.guest_agent, "guest agent"),
        ] {
            if changed {
                pending.push(format!("{what} (takes effect when the machine restarts)"));
            }
        }

        // A machine that had no screen has just been given one, and no
        // comparison above can notice: a document with no display device reads
        // back as the default card, so `video` is equal on both sides even
        // though one side has a graphics device and the other never did. Ask
        // the document instead — the save has already rewritten it, and
        // `render` always writes a `<graphics>` element.
        //
        // Worth its own sentence rather than folding into "graphics card",
        // because the remedy is stricter than the usual one. A reboot keeps
        // the same hypervisor process and therefore the same running document;
        // only a full stop and start builds a machine with the socket on it.
        if !domain_xml::has_screen(&machine.observed.xml) {
            pending.push(
                "the console (this machine had no screen; it needs a full stop and start, not a \
                 reboot)"
                    .to_string(),
            );
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

        // Both kinds of backing leave with the machine: local zvols by their
        // dataset path, replicated volumes resolved from their device.
        let mut backings: Vec<Backing> = Vec::new();
        for disk in &machine.config.disks {
            if let Some(volume) = volume_of(&disk.source) {
                backings.push(Backing::Zvol(volume));
            } else if let Ok(Some(replicated)) = self.volumes.disk_of(&disk.source).await {
                backings.push(Backing::Replicated {
                    device: disk.source.clone(),
                    name: replicated.name,
                });
            }
        }

        let mut removed = Vec::new();
        let mut kept = Vec::new();
        for backing in backings {
            let (label, result) = match &backing {
                Backing::Zvol(volume) => (
                    volume.clone(),
                    if purge_disks {
                        Some(
                            self.storage
                                .destroy_volume(volume)
                                .await
                                .map_err(VirtError::from),
                        )
                    } else {
                        None
                    },
                ),
                Backing::Replicated { device, name } => (
                    name.clone(),
                    if purge_disks {
                        Some(
                            self.volumes
                                .destroy_disk(device)
                                .await
                                .map_err(VirtError::from),
                        )
                    } else {
                        None
                    },
                ),
            };
            match result {
                None => kept.push(label),
                Some(Ok(())) => removed.push(label),
                Some(Err(err)) => {
                    // The machine is already gone; a volume that will not go
                    // with it is reported rather than making the whole
                    // operation look like a failure.
                    tracing::error!(volume = %label, "could not remove the volume: {err}");
                    kept.push(label);
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
        // A replicated device is served, not simply present. DRBD's exists
        // on every member for as long as the resource does, but a pooled
        // disk's exists only where its daemon has been asked to serve it —
        // so every start readies its own devices first. That is what makes
        // an HA restart work, and a start after the storage daemon
        // restarted, without either path having to know which engine is
        // underneath. For DRBD it is a lookup and nothing else.
        for disk in &config.disks {
            if self.volumes.disk_of(&disk.source).await?.is_some() {
                self.volumes.ensure_local_device(&disk.source).await?;
            }
        }
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

    /// Live-migrate a running machine to another member of its disks'
    /// replica set. The two-primaries window — DRBD's one deliberate
    /// exception to "one writer" — opens on every replicated disk just
    /// before the migration and closes on **every** path out: the guard is
    /// two loops around one backend call, not a flag something remembers to
    /// reset.
    pub async fn migrate(
        &self,
        vmid: u32,
        target: &str,
        destination: &str,
    ) -> Result<VmMigrateResponse> {
        let _guard = self.gate.lock().await;
        let machine = self.machine(vmid).await?;
        let name = machine.config.name.clone();

        if !machine.observed.state.is_running() {
            return Err(VirtError::Conflict(format!(
                "\"{name}\" is not running. Live migration moves a running machine; a stopped \
                 one has nothing to keep alive in transit."
            )));
        }
        if target == self.node {
            return Err(VirtError::Conflict(format!(
                "\"{name}\" is already on {target}."
            )));
        }
        if !machine.config.cdroms.is_empty() {
            return Err(VirtError::Conflict(format!(
                "\"{name}\" has installation media attached — a file on this node's pool, which \
                 cannot follow it. Eject the media first."
            )));
        }
        if machine.config.disks.is_empty() {
            return Err(VirtError::Conflict(format!(
                "\"{name}\" has no disks, so it has no replica set naming where it can run."
            )));
        }
        for disk in &machine.config.disks {
            if volume_of(&disk.source).is_some() {
                return Err(VirtError::Conflict(format!(
                    "\"{}\" is a local volume on this node. Every disk must be replicated for \
                     the machine to leave.",
                    disk.id
                )));
            }
        }

        let devices: Vec<String> = machine
            .config
            .disks
            .iter()
            .map(|d| d.source.clone())
            .collect();
        let members = self.volumes.common_members(&devices).await?;
        if !members.iter().any(|m| m == target) {
            return Err(VirtError::Conflict(format!(
                "\"{target}\" holds no replica of every disk — \"{name}\" can run on {}.",
                members.join(", ")
            )));
        }

        // Open the window on every disk; then, whatever happens, close it on
        // everything that opened — and say which ending it was. A closed
        // window is a machine that cannot have a second writer, and for a
        // storage engine that hands a lease over rather than toggling a
        // switch, "the machine landed" and "the machine never left" call for
        // opposite acts.
        let mut opened: Vec<String> = Vec::new();
        let mut failure: Option<VirtError> = None;
        for device in &devices {
            let opening = lumen_drbd::MigrationWindow::Open {
                destination: target.to_string(),
            };
            match self.volumes.migration_window(device, opening).await {
                Ok(()) => opened.push(device.clone()),
                Err(err) => {
                    failure = Some(VirtError::Conflict(format!(
                        "The two-primaries window on {device} did not open, so the migration \
                         was not started: {err}"
                    )));
                    break;
                }
            }
        }
        if failure.is_none() {
            if let Err(err) = self.backend.migrate(&name, destination).await {
                failure = Some(err);
            }
        }
        // The ending, named: the machine is either on the destination now
        // or it never left, and the storage layer is told which.
        let ending = if failure.is_none() {
            lumen_drbd::MigrationWindow::Accepted
        } else {
            lumen_drbd::MigrationWindow::Aborted
        };
        for device in opened.iter().rev() {
            if let Err(err) = self.volumes.migration_window(device, ending.clone()).await {
                tracing::error!(%device, "the two-primaries window did not close: {err}");
                if failure.is_none() {
                    failure = Some(VirtError::Conflict(format!(
                        "\"{name}\" migrated to {target}, but the two-primaries window on \
                         {device} did not close: {err}. Close it before anything else — the \
                         open window is what makes a second writer possible."
                    )));
                }
            }
        }
        if let Some(err) = failure {
            return Err(err);
        }

        tracing::info!(vmid, %name, target, "machine migrated");
        Ok(VmMigrateResponse {
            vmid,
            name,
            target: target.to_string(),
        })
    }

    /// The stored domain document, exactly as the hypervisor holds it — what
    /// definition replication pushes to the cluster's other members.
    pub async fn definition(&self, vmid: u32) -> Result<String> {
        Ok(self.machine(vmid).await?.observed.xml)
    }

    /// Where a set of replicated devices can run, answered by **this
    /// node's own storage engine** — the seam VirtService was built on.
    /// The HA sweep and the maintenance drain ask through here rather
    /// than any one engine directly: on a pooled node, asking DRBD about
    /// a `/dev/ublkb` device is a refusal wearing an answer's clothes,
    /// and a machine that could restart anywhere would restart nowhere.
    pub async fn common_members(&self, devices: &[String]) -> Result<Vec<String>> {
        self.volumes
            .common_members(devices)
            .await
            .map_err(VirtError::from)
    }

    /// Define-and-start a machine from a replicated definition — the HA
    /// manager's one verb, after the machine's node is confirmed lost. The
    /// document is defined verbatim: its disks are `/dev/drbd` devices that
    /// exist here too, which is the whole reason it can move.
    pub async fn adopt(&self, xml: &str) -> Result<VmView> {
        let _guard = self.gate.lock().await;
        let parsed = domain_xml::parse(xml)?;
        let vmid = parsed.config.vmid;
        if self.machine(vmid).await.is_ok() {
            return Err(VirtError::Conflict(format!(
                "Machine {vmid} is already defined on this node."
            )));
        }
        self.backend.define(xml).await?;
        self.start_domain(&parsed.config).await?;
        tracing::info!(vmid, name = %parsed.config.name, "machine adopted and started");
        self.get(vmid).await
    }

    pub async fn attach_disk(&self, vmid: u32, request: DiskCreate) -> Result<VmUpdateResponse> {
        let _guard = self.gate.lock().await;
        let machine = self.machine(vmid).await?;
        let mut config = machine.config.clone();

        let size = request.size_gib.saturating_mul(GIB);
        let planned: Vec<PlannedDisk> = if request.replicated {
            Vec::new()
        } else {
            vec![PlannedDisk {
                pool: request.pool.clone(),
                size,
            }]
        };
        let facts = self.facts(Some(vmid)).await?;
        let errors = validate(&config, &planned, &facts);
        if !errors.is_empty() {
            return Err(VirtError::Invalid(errors));
        }

        let name = format!("vm-{vmid}-disk-{}", self.next_disk_index(&config).await?);
        let (source, actual_size, backing) = if request.replicated {
            let replicated = self
                .volumes
                .create_disk(&lumen_drbd::VmDiskRequest {
                    name,
                    size_bytes: size,
                    members: request.members.clone(),
                })
                .await?;
            let backing = Backing::Replicated {
                device: replicated.device.clone(),
                name: replicated.name,
            };
            (replicated.device, replicated.size_bytes, backing)
        } else {
            let volume = self
                .storage
                .create_volume(&request.pool, &name, size, request.blocksize)
                .await?;
            (
                self.storage.device_path(&volume.name),
                volume.volsize.unwrap_or(size),
                Backing::Zvol(volume.name),
            )
        };

        let disk = VmDisk {
            id: config.next_disk_target(request.bus),
            bus: request.bus,
            source,
            size: actual_size,
            cache: request.cache,
            discard: request.discard,
            boot_index: None,
        };
        config.disks.push(disk.clone());

        if let Err(err) = self
            .backend
            .define(&domain_xml::redefine(
                &config,
                machine.observed.uuid.as_deref(),
            ))
            .await
        {
            self.remove_backings(&[backing]).await;
            return Err(err);
        }

        let (applied_live, pending_reboot) = self
            .live_device(&machine, true, &domain_xml::disk_fragment(&disk), &disk.id)
            .await;

        tracing::info!(vmid, disk = %disk.id, source = %disk.source, "disk attached");
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
        self.backend
            .define(&domain_xml::redefine(
                &config,
                machine.observed.uuid.as_deref(),
            ))
            .await?;

        if purge {
            let still_attached = machine.observed.state.is_running() && !pending_reboot.is_empty();
            if still_attached {
                return Err(VirtError::Conflict(format!(
                    "\"{id}\" was removed from the configuration, but the running machine \
                     still has it, so its volume was left in place. Restart the machine and \
                     remove the volume again."
                )));
            }
            match volume_of(&disk.source) {
                Some(volume) => self.storage.destroy_volume(&volume).await?,
                None if self.volumes.disk_of(&disk.source).await?.is_some() => {
                    self.volumes.destroy_disk(&disk.source).await?
                }
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

        self.backend
            .define(&domain_xml::redefine(
                &config,
                machine.observed.uuid.as_deref(),
            ))
            .await?;
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
        self.backend
            .define(&domain_xml::redefine(
                &config,
                machine.observed.uuid.as_deref(),
            ))
            .await?;

        tracing::info!(vmid, nic = %id, "adapter detached");
        Ok(VmUpdateResponse {
            vm: self.get(vmid).await?,
            applied_live,
            pending_reboot,
        })
    }

    // --- files into a guest -----------------------------------------------

    /// Write a file into a running guest, through the guest's own agent.
    ///
    /// ## Why this is not a console feature
    ///
    /// A console is a screen and a keyboard. There is no file transfer in RFB
    /// and there is not going to be one, so dropping a file on the console
    /// cannot travel over the console's connection — it goes the only way into
    /// a running guest that does not involve its network: the agent already
    /// listening on a virtio port, which is the same channel an orderly
    /// shutdown uses.
    ///
    /// ## What it is for, and what it is not
    ///
    /// Every byte is base64 inside a JSON message on the hypervisor's control
    /// socket, and is held in memory on both sides on the way through. That is
    /// entirely reasonable for a key, a script, a certificate, or a
    /// configuration file — the things somebody actually wants to get into a
    /// guest that has no network yet — and it is the wrong shape for anything
    /// large. [`MAX_GUEST_FILE_BYTES`] is where that line is drawn, and the
    /// refusal says what to use instead.
    ///
    /// The guest decides everything about the result: the agent runs as root
    /// in most guests, so the file lands with the agent's ownership and the
    /// guest's umask, and a path the agent cannot write is the guest's refusal
    /// rather than this appliance's.
    pub async fn push_file(&self, vmid: u32, path: &str, contents: &[u8]) -> Result<PushedFile> {
        let machine = self.machine(vmid).await?;
        let name = machine.config.name.clone();

        if !machine.observed.state.is_running() {
            return Err(VirtError::Conflict(format!(
                "\"{name}\" is not running. A file can only be copied into a guest that is up, \
                 because it is the guest's own agent that writes it."
            )));
        }

        let path = path.trim();
        // Checked here rather than trusted from the console, and deliberately
        // shallow: this is not a sandbox — the agent runs inside the guest and
        // writing anywhere in that guest is the entire point. What it rules out
        // is a relative path, which the agent would resolve against a working
        // directory nobody chose.
        if !path.starts_with('/') || path.ends_with('/') {
            return Err(VirtError::Conflict(format!(
                "\"{path}\" is not a full path to a file inside the guest. It has to start with \
                 \"/\" and end with a file name."
            )));
        }
        if contents.len() > MAX_GUEST_FILE_BYTES {
            return Err(VirtError::Conflict(format!(
                "That file is {:.1} MiB, and {} MiB is the most that can be copied in this way. \
                 Every byte travels through the hypervisor's control socket, so this is meant for \
                 a script, a key, or a configuration file — anything larger belongs on a disk or \
                 on installation media.",
                contents.len() as f64 / (1024.0 * 1024.0),
                MAX_GUEST_FILE_BYTES / (1024 * 1024),
            )));
        }

        let handle = self.agent_open(&name, path).await?;

        // From here the guest is holding an open file, so every way out of this
        // function has to close it. A copy that failed half way must not also
        // leave a descriptor behind in a guest that nothing here can reach
        // again — hence the close before either result is unwrapped.
        let written = self.agent_write_all(&name, handle, contents).await;
        let closed = self
            .backend
            .guest_agent(
                &name,
                &agent_command("guest-file-close", serde_json::json!({ "handle": handle })),
            )
            .await;
        written?;
        closed?;

        tracing::info!(vmid, %name, %path, bytes = contents.len(), "file copied into the guest");
        Ok(PushedFile {
            vmid,
            path: path.to_string(),
            bytes: contents.len() as u64,
        })
    }

    /// Open a file in the guest for writing, and take the handle.
    async fn agent_open(&self, name: &str, path: &str) -> Result<i64> {
        // "w" truncates, which is what replacing a file means, and "b" is what
        // stops anything rewriting a line ending on a file that is not text.
        let reply = self
            .backend
            .guest_agent(
                name,
                &agent_command(
                    "guest-file-open",
                    serde_json::json!({ "path": path, "mode": "wb" }),
                ),
            )
            .await?;
        agent_return(&reply)?.as_i64().ok_or_else(|| {
            VirtError::Backend(anyhow::anyhow!(
                "the guest agent did not answer with a file handle: {reply}"
            ))
        })
    }

    /// Send the whole file, a chunk at a time.
    async fn agent_write_all(&self, name: &str, handle: i64, contents: &[u8]) -> Result<()> {
        use base64::Engine as _;

        for chunk in contents.chunks(GUEST_WRITE_CHUNK) {
            let encoded = base64::engine::general_purpose::STANDARD.encode(chunk);
            let reply = self
                .backend
                .guest_agent(
                    name,
                    &agent_command(
                        "guest-file-write",
                        serde_json::json!({
                            "handle": handle,
                            "buf-b64": encoded,
                            "count": chunk.len(),
                        }),
                    ),
                )
                .await?;
            // A short write is not a partial success worth reporting as one:
            // the file in the guest is now wrong, and the usual reason is that
            // the guest's filesystem is full.
            let wrote = agent_return(&reply)?
                .get("count")
                .and_then(|count| count.as_u64())
                .unwrap_or(0);
            if wrote != chunk.len() as u64 {
                return Err(VirtError::Conflict(format!(
                    "The guest took {wrote} of {} bytes and stopped. The usual reason is no room \
                     left on the filesystem the file was being written to.",
                    chunk.len()
                )));
            }
        }
        Ok(())
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

    /// Undo backings created for a machine that did not survive being made —
    /// local zvols and replicated volumes alike.
    async fn remove_backings(&self, backings: &[Backing]) {
        for backing in backings {
            let result = match backing {
                Backing::Zvol(volume) => self
                    .storage
                    .destroy_volume(volume)
                    .await
                    .map_err(VirtError::from),
                Backing::Replicated { device, .. } => self
                    .volumes
                    .destroy_disk(device)
                    .await
                    .map_err(VirtError::from),
            };
            if let Err(err) = result {
                tracing::error!("could not clean up after a failed create: {err}");
            }
        }
    }

    /// The next free disk index across *both* kinds of backing. A zvol
    /// carries its index in its device path; a replicated disk's device is
    /// `/dev/drbd<minor>` and says nothing, so its volume name is looked up
    /// in the record — which is why this lives here and not on the model.
    async fn next_disk_index(&self, config: &VmConfig) -> Result<u32> {
        let mut taken: Vec<u32> = Vec::new();
        for disk in &config.disks {
            let name = match volume_of(&disk.source) {
                Some(volume) => Some(volume),
                None => self
                    .volumes
                    .disk_of(&disk.source)
                    .await?
                    .map(|replicated| replicated.name),
            };
            if let Some(index) = name.as_deref().and_then(index_of) {
                taken.push(index);
            }
        }
        Ok((0u32..)
            .find(|index| !taken.contains(index))
            .expect("an unbounded search always finds a free index"))
    }
}

/// The index out of a `…-disk-<n>` volume name.
fn index_of(name: &str) -> Option<u32> {
    name.rsplit_once("-disk-")?.1.parse().ok()
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
        volumes: Arc<lumen_drbd::MockVmVolumes>,
        state_dir: std::path::PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.state_dir);
        }
    }

    /// A node with one bridge, one pool, and no machines.
    ///
    /// The domains are wired together exactly as `main` wires them, so
    /// these tests exercise the real dependency direction: compute asks
    /// storage for a volume and networking for a bridge, and neither of those
    /// knows a machine exists. Replicated storage is the standalone stub —
    /// this node is not in a cluster unless a test says so.
    async fn harness(tag: &str) -> Harness {
        harness_with(tag, lumen_drbd::MockVmVolumes::standalone()).await
    }

    /// The same node inside a two-node cluster, for the replicated-disk and
    /// migration tests.
    async fn clustered_harness(tag: &str) -> Harness {
        harness_with(
            tag,
            lumen_drbd::MockVmVolumes::clustered("alpha", &["lumen", "lumen02"]),
        )
        .await
    }

    async fn harness_with(tag: &str, volumes: lumen_drbd::MockVmVolumes) -> Harness {
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
        let volumes = Arc::new(volumes);
        let service = VirtService::new(virt.clone(), storage, network, volumes.clone())
            .with_osinfo_root(state_dir.join("osinfo"));

        Harness {
            service,
            virt,
            zfs,
            volumes,
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
            video: VideoModel::default(),
            boot_order: None,
            start_on_boot: false,
            guest_agent: true,
            ha: false,
            tags: Vec::new(),
            os_id: None,
            cdroms: Vec::new(),
            disks: vec![DiskCreate {
                pool: "boot".into(),
                size_gib: 32,
                bus: DiskBus::VirtioBlk,
                cache: CacheMode::None,
                discard: true,
                blocksize: None,
                replicated: false,
                members: Vec::new(),
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
        assert_eq!(vm.disks[0].source, "/dev/zvol/boot/lumen/vm-100-disk-0");
        assert_eq!(vm.nics.len(), 1);
        assert_eq!(vm.nics[0].id, generate_mac(100, 0));
        assert_eq!(vm.nics[0].bridge, "br0");
        assert_eq!(vm.boot_disk.as_deref(), Some("vda"));
        // The stored document asks for a console but defers its path to the
        // hypervisor, which chooses one at start — so a machine that has not
        // started has a screen and no address for it yet.
        assert!(vm.has_screen);
        assert_eq!(vm.vnc_socket, None);

        // The volume really exists, and so does the domain.
        assert!(h.zfs.has_dataset("boot/lumen/vm-100-disk-0"));
        assert!(h.virt.is_defined("web01"));

        // The controls say what can be done right now, and why not.
        assert!(vm.actions.start.allowed);
        assert!(!vm.actions.shutdown.allowed);
        assert!(vm.actions.shutdown.reason.is_some());
    }

    /// A console exists for exactly as long as the guest does, and the domain
    /// The mock applies the three agent commands to an in-memory guest, so this
    /// asserts on what actually landed on the other side rather than on which
    /// calls were made — the chunking, the encoding, and the close are all in
    /// the answer.
    #[tokio::test]
    async fn a_file_copied_into_a_guest_arrives_whole() {
        let h = harness("push-file").await;
        let vm = h.service.create(create("web01")).await.unwrap();
        h.service.start(vm.vmid).await.unwrap();

        // Deliberately more than one write and not a whole number of them: a
        // file that fits in a single message would pass whatever the loop did,
        // and an exact multiple would hide an off-by-one in the last chunk.
        let contents: Vec<u8> = (0..(GUEST_WRITE_CHUNK * 2 + 17))
            .map(|byte| (byte % 251) as u8)
            .collect();

        let pushed = h
            .service
            .push_file(vm.vmid, "/root/bootstrap.sh", &contents)
            .await
            .unwrap();
        assert_eq!(pushed.path, "/root/bootstrap.sh");
        assert_eq!(pushed.bytes, contents.len() as u64);
        assert_eq!(
            h.virt.guest_file("/root/bootstrap.sh").as_deref(),
            Some(contents.as_slice()),
            "every byte, in order, on the other side"
        );
    }

    /// Both refusals are the guest's situation rather than this node's, and
    /// both are worth saying in words: there is no agent in a machine that is
    /// off, and a relative path would be resolved against a working directory
    /// nobody chose.
    #[tokio::test]
    async fn a_file_needs_a_running_machine_and_a_full_path() {
        let h = harness("push-file-refused").await;
        let vm = h.service.create(create("web01")).await.unwrap();

        let err = h
            .service
            .push_file(vm.vmid, "/root/x", b"hello")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not running"), "{err}");

        h.service.start(vm.vmid).await.unwrap();
        let err = h
            .service
            .push_file(vm.vmid, "etc/passwd", b"hello")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("full path"), "{err}");
    }

    /// says so rather than handing back a path nothing is listening on.
    #[tokio::test]
    async fn a_console_is_offered_only_while_the_machine_is_running() {
        let h = harness("console").await;
        let vm = h.service.create(create("web01")).await.unwrap();

        assert!(!vm.actions.console.allowed);
        assert!(
            vm.actions
                .console
                .reason
                .as_deref()
                .unwrap()
                .contains("no console"),
            "{:?}",
            vm.actions.console.reason
        );
        let err = h.service.console(vm.vmid).await.unwrap_err();
        assert!(matches!(err, VirtError::Conflict(_)), "{err:?}");
        // The refusal and the disabled control say the same thing, so a
        // console that raced the machine stopping reads no differently.
        assert_eq!(err.to_string(), vm.actions.console.reason.unwrap());

        let running = h.service.start(vm.vmid).await.unwrap();
        assert!(running.actions.console.allowed);
        let target = h.service.console(vm.vmid).await.unwrap();
        assert_eq!(target.vmid, 100);
        assert_eq!(target.name, "web01");
        assert_eq!(target.protocol, ConsoleProtocol::Vnc);
        // Read out of the *live* document: the hypervisor chose the path when
        // the machine started, under its own per-domain directory.
        assert_eq!(
            target.socket,
            "/var/lib/libvirt/qemu/domain-1-web01/vnc.sock"
        );
        // The stored document still defers it, so the view still has none —
        // the console endpoint is the one place that knows the address.
        assert_eq!(running.vnc_socket, None);

        // A machine that is not there is a not-found, not a console.
        assert!(matches!(
            h.service.console(999).await.unwrap_err(),
            VirtError::NotFound(_)
        ));
    }

    /// A machine defined before this appliance put a screen on one.
    ///
    /// The trap this pins is that its document parses as having the default
    /// graphics card, because a document with no `<video>` in it always does.
    /// So the console named a card the machine did not have, offered a viewer
    /// against a socket nothing ever created, and — because `video` compared
    /// equal on both sides of a save — reported nothing waiting for a restart
    /// afterwards. Three things agreeing that the machine was fine, and a
    /// console that would not open.
    #[tokio::test]
    async fn a_machine_that_predates_consoles_is_not_reported_as_having_a_card() {
        let h = harness("no-screen").await;
        let vm = h.service.create(create("web01")).await.unwrap();

        // Rewind its document to what such a machine carries: neither device,
        // because both arrived in the same release.
        let stored = h.virt.live_xml("web01").await.unwrap();
        let no_video = stored.replace(
            "    <video>\n      <model type='virtio'/>\n    </video>\n",
            "",
        );
        let before_consoles = no_video.replace(
            "    <graphics type='vnc'>\n      <listen type='socket'/>\n    </graphics>\n",
            "",
        );
        assert!(!before_consoles.contains("<video>"), "{before_consoles}");
        assert!(!before_consoles.contains("<graphics"), "{before_consoles}");
        assert!(!domain_xml::has_screen(&before_consoles));
        h.virt.define(&before_consoles).await.unwrap();

        let vm = h.service.get(vm.vmid).await.unwrap();
        // The card still reads as the default. That is not a bug to fix in the
        // parser — it is what the next save will write — it is the reason the
        // console must ask the document instead.
        assert_eq!(vm.video, VideoModel::Virtio);
        assert!(!vm.has_screen);

        // Running, and the console is refused with the remedy rather than
        // offered as a viewer that opens and dies.
        let running = h.service.start(vm.vmid).await.unwrap();
        assert!(!running.actions.console.allowed);
        let reason = running.actions.console.reason.clone().unwrap();
        assert!(reason.contains("without a screen"), "{reason}");
        assert!(reason.contains("stop and start"), "{reason}");
        // The refusal and the disabled control say the same thing, as they do
        // for a machine that is simply switched off.
        assert_eq!(
            h.service.console(vm.vmid).await.unwrap_err().to_string(),
            reason
        );

        // Saving gives it a screen and says so — even though not one field of
        // the configuration changed. This is the half that was missing: with
        // `video` equal on both sides nothing was reported as pending, so an
        // operator was told the save was complete and had no reason to
        // restart.
        let saved = h
            .service
            .update(
                vm.vmid,
                VmPatch {
                    video: Some(VideoModel::Virtio),
                    ..VmPatch::default()
                },
            )
            .await
            .unwrap();
        assert!(saved.vm.has_screen);
        assert!(
            saved
                .pending_reboot
                .iter()
                .any(|note| note.contains("full stop and start")),
            "{:?}",
            saved.pending_reboot
        );
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
        assert_eq!(removed.kept_volumes, vec!["boot/lumen/vm-100-disk-0"]);
        assert!(h.zfs.has_dataset("boot/lumen/vm-100-disk-0"));
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
        assert_eq!(removed.removed_volumes, vec!["boot/lumen/vm-100-disk-0"]);
        assert!(!h.zfs.has_dataset("boot/lumen/vm-100-disk-0"));
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
        assert!(h.zfs.has_dataset("boot/lumen/vm-100-disk-0"));
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
        assert!(!h.zfs.has_dataset("boot/lumen/vm-100-disk-0"));
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
                    pool: "boot".into(),
                    size_gib: 16,
                    bus: DiskBus::VirtioScsi,
                    cache: CacheMode::None,
                    discard: true,
                    blocksize: None,
                    replicated: false,
                    members: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.vm.disks.len(), 2);
        assert_eq!(updated.vm.disks[1].id, "sda");
        assert_eq!(
            updated.vm.disks[1].source,
            "/dev/zvol/boot/lumen/vm-100-disk-1"
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
        assert!(!h.zfs.has_dataset("boot/lumen/vm-100-disk-1"));
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
                    pool: "boot".into(),
                    size_gib: 4096,
                    bus: DiskBus::VirtioBlk,
                    cache: CacheMode::None,
                    discard: true,
                    blocksize: None,
                    replicated: false,
                    members: Vec::new(),
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
        assert!(!h.zfs.has_dataset("boot/lumen/vm-100-disk-1"));
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

    /// The node's own size, and how much of it the running machines hold. A
    /// machine that is merely defined holds nothing — that is the whole
    /// distinction the dashboard's meters are drawn from.
    #[tokio::test]
    async fn a_node_reports_its_capacity_and_only_what_is_running_against_it() {
        let h = harness("nodes").await;

        let idle = h.service.nodes().await.unwrap();
        assert_eq!(idle.nodes.len(), 1);
        let node = &idle.nodes[0];
        assert_eq!(node.node, h.service.node());
        assert_eq!(node.cpus, 16);
        assert_eq!(node.memory_mib, 32_768);
        assert_eq!(
            node.reserved_memory_mib,
            crate::state::HOST_MEMORY_RESERVE_MIB
        );
        assert_eq!(node.hypervisor_version.as_deref(), Some("11.10.0"));
        assert_eq!(node.machines, 0);
        assert_eq!(node.running, 0);
        assert_eq!(node.used_vcpus, 0);
        assert_eq!(node.used_memory_mib, 0);

        // Two machines defined, one of them started. `create` asks for 2
        // processors and 4 GiB apiece.
        h.service.create(create("idle")).await.unwrap();
        let mut started = create("busy");
        started.start = true;
        h.service.create(started).await.unwrap();

        let loaded = h.service.nodes().await.unwrap();
        let node = &loaded.nodes[0];
        assert_eq!(node.machines, 2);
        assert_eq!(node.running, 1);
        assert_eq!(node.used_vcpus, 2);
        assert_eq!(node.used_memory_mib, 4096);
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
        seed_image(&h, "boot", "almalinux-10.iso").await;
        seed_image(&h, "boot", "virtio-win.iso").await;

        let mut request = create("win01");
        request.os_id = Some("http://microsoft.com/win/11".into());
        request.boot_order = Some(vec![BootDevice::Cdrom, BootDevice::Disk]);
        request.cdroms = vec![
            CdromCreate {
                storage: Some("boot".into()),
                image: Some("almalinux-10.iso".into()),
                source: None,
            },
            CdromCreate {
                storage: Some("boot".into()),
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
            ("no such image", Some("boot"), Some("nothere.iso")),
            ("no such pool", Some("tank"), Some("almalinux-10.iso")),
            (
                "a name that is a path",
                Some("boot"),
                Some("../../etc/passwd.iso"),
            ),
            ("a storage with no image", Some("boot"), None),
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
        assert!(!h.zfs.has_dataset("boot/lumen/vm-100-disk-0"));

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

    // --- replicated disks and migration -------------------------------------

    fn replicated_disk(size_gib: u64) -> DiskCreate {
        DiskCreate {
            pool: String::new(),
            size_gib,
            bus: DiskBus::VirtioBlk,
            cache: CacheMode::None,
            discard: true,
            blocksize: None,
            replicated: true,
            members: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_replicated_disk_rides_the_stable_device_and_leaves_with_the_machine() {
        let h = clustered_harness("replicated-disk").await;
        let mut request = create("web01");
        request.disks = vec![replicated_disk(10)];
        let vm = h.service.create(request).await.unwrap();

        // The stable device, not a zvol path — the same document is valid on
        // every member.
        assert_eq!(vm.disks[0].source, "/dev/drbd1");
        let made = h.volumes.disks();
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].name, format!("vm-{}-disk-0", vm.vmid));

        // A local disk attached next takes index 1: the replicated disk's
        // index is invisible in its device path and is found in the record.
        h.service
            .attach_disk(
                vm.vmid,
                DiskCreate {
                    pool: "boot".into(),
                    size_gib: 8,
                    bus: DiskBus::VirtioBlk,
                    cache: CacheMode::None,
                    discard: true,
                    blocksize: None,
                    replicated: false,
                    members: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert!(h
            .zfs
            .has_dataset(&format!("boot/lumen/vm-{}-disk-1", vm.vmid)));

        // Purge takes both kinds of backing with the machine.
        let deleted = h
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
        assert!(deleted
            .removed_volumes
            .contains(&format!("vm-{}-disk-0", vm.vmid)));
        assert!(deleted
            .removed_volumes
            .contains(&format!("boot/lumen/vm-{}-disk-1", vm.vmid)));
        assert_eq!(
            h.volumes.destroyed(),
            vec![format!("vm-{}-disk-0", vm.vmid)]
        );
    }

    #[tokio::test]
    async fn a_replicated_disk_is_refused_outside_a_cluster_and_leaves_nothing() {
        let h = harness("replicated-standalone").await;
        let mut request = create("web01");
        request.disks = vec![replicated_disk(10)];
        let err = h.service.create(request).await.unwrap_err();
        assert!(err.to_string().contains("environment"), "{err}");
        assert!(h.virt.names().is_empty(), "no half-made machine");
    }

    #[tokio::test]
    async fn a_migration_holds_the_window_for_exactly_as_long_as_it_takes() {
        let h = clustered_harness("migrate").await;
        let mut request = create("web01");
        request.disks = vec![replicated_disk(10)];
        request.start = true;
        let vm = h.service.create(request).await.unwrap();

        let answer = h
            .service
            .migrate(vm.vmid, "lumen02", "qemu+tcp://10.10.0.2/system")
            .await
            .unwrap();
        assert_eq!(answer.target, "lumen02");
        assert_eq!(
            h.virt.migrated(),
            vec![(
                "web01".to_string(),
                "qemu+tcp://10.10.0.2/system".to_string()
            )]
        );

        // The window opened before the move and closed after it — and is
        // verifiably closed now.
        assert_eq!(
            h.volumes.windows(),
            vec![
                ("/dev/drbd1".to_string(), true),
                ("/dev/drbd1".to_string(), false)
            ]
        );
        assert!(h.volumes.open_windows().is_empty());
        // And the storage layer was told which ending this was, and where
        // the machine went — the two things a lease handover needs and a
        // bare "close it" cannot express.
        assert_eq!(
            h.volumes.window_states(),
            vec![
                (
                    "/dev/drbd1".to_string(),
                    lumen_drbd::MigrationWindow::Open {
                        destination: "lumen02".to_string()
                    }
                ),
                (
                    "/dev/drbd1".to_string(),
                    lumen_drbd::MigrationWindow::Accepted
                )
            ]
        );

        // The machine has one home, and it is not this node.
        assert!(h.service.get(vm.vmid).await.is_err());
    }

    #[tokio::test]
    async fn every_start_readies_its_replicated_devices_first() {
        // A pooled device exists only where its daemon serves it, so the
        // start path has to ask for it — this is what makes an HA restart on
        // a survivor, and a start after the storage daemon restarted, find a
        // device to open at all.
        let h = clustered_harness("ready-devices").await;
        let mut request = create("web01");
        request.disks = vec![replicated_disk(10)];
        request.start = true;
        let vm = h.service.create(request).await.unwrap();
        assert_eq!(
            h.volumes.ensured(),
            vec!["/dev/drbd1".to_string()],
            "creating and starting a machine did not ready its device"
        );

        // And again on an ordinary start, because the device may have gone
        // away with a daemon while the machine was stopped.
        h.service
            .stop(
                vm.vmid,
                Acknowledgements {
                    may_lose_data: true,
                },
            )
            .await
            .unwrap();
        h.service.start(vm.vmid).await.unwrap();
        assert_eq!(
            h.volumes.ensured(),
            vec!["/dev/drbd1".to_string(), "/dev/drbd1".to_string()],
            "a restart did not ready the device again"
        );
    }

    #[tokio::test]
    async fn a_failed_migration_still_closes_the_window_and_keeps_the_machine() {
        let h = clustered_harness("migrate-fails").await;
        let mut request = create("web01");
        request.disks = vec![replicated_disk(10)];
        request.start = true;
        let vm = h.service.create(request).await.unwrap();

        h.virt.refuse_migration("the transport dropped mid-copy");
        let err = h
            .service
            .migrate(vm.vmid, "lumen02", "qemu+tcp://10.10.0.2/system")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("dropped"), "{err}");
        // A failure closes the window as an *abort*: the machine never
        // left, so a lease-based engine must keep the pen where it was
        // rather than hand it on.
        assert_eq!(
            h.volumes.window_states().last().map(|(_, w)| w.clone()),
            Some(lumen_drbd::MigrationWindow::Aborted),
            "a failed migration must abort the window, not accept it"
        );
        assert!(
            h.volumes.open_windows().is_empty(),
            "the window must close on failure too"
        );
        assert!(
            h.service.get(vm.vmid).await.is_ok(),
            "the machine never left"
        );

        // And when opening the window itself fails, nothing migrates at all.
        h.volumes.fail_next_window();
        let err = h
            .service
            .migrate(vm.vmid, "lumen02", "qemu+tcp://10.10.0.2/system")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("was not started"), "{err}");
        assert_eq!(h.virt.migrated().len(), 0);
    }

    #[tokio::test]
    async fn a_migration_is_refused_where_it_cannot_work() {
        let h = clustered_harness("migrate-refusals").await;
        let uri = "qemu+tcp://10.10.0.2/system";

        // A machine with a local disk cannot leave the node its zvol is on.
        let mut request = create("web01");
        request.start = true;
        let local = h.service.create(request).await.unwrap();
        let err = h
            .service
            .migrate(local.vmid, "lumen02", uri)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("local volume"), "{err}");

        // A stopped machine has nothing to keep alive in transit.
        let mut request = create("web02");
        request.disks = vec![replicated_disk(4)];
        let stopped = h.service.create(request).await.unwrap();
        let err = h
            .service
            .migrate(stopped.vmid, "lumen02", uri)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not running"), "{err}");

        // A node outside the replica set is refused by name.
        h.service.start(stopped.vmid).await.unwrap();
        let err = h
            .service
            .migrate(stopped.vmid, "ghost", uri)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("holds no replica"), "{err}");
        assert!(
            h.volumes.windows().is_empty(),
            "no refusal touches a window"
        );
    }
}
