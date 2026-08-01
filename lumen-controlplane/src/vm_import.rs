//! Importing a machine from a VMware appliance archive.
//!
//! The whole feature is three acts, and this module is the second and third:
//! the console uploads an OVA into the spool (streamed, like an installation
//! image), `lumen_virt::ovf` reads what machine is in it, the operator
//! corrects the proposal — which bridge each source network maps to, which
//! pool each disk lands in — and the commit runs here as a background job:
//! define the machine with blank volumes of the right sizes, then fill each
//! volume from the archive with `qemu-img`, which reads the compressed VMDK
//! straight out of the tar through an offset and writes raw blocks.
//!
//! There is no format plumbing on the other end because there is nothing to
//! plumb: every disk this appliance gives a machine is a raw block device,
//! local zvol and replicated volume alike, so "import" is precisely "create
//! as usual, then fill".
//!
//! ## The spool
//!
//! `/var/lib/lumen/import` — a plain directory, not a dataset, because unlike
//! the media library nothing here outlives the import: the archive is removed
//! when the machine it described is running (or kept, if the operator asked).
//! The unit grants it with the same `ReadWritePaths` relaxation the media
//! library has. Uploads land as `<name>.part` and only take their real name
//! once whole, exactly as `lumen_zfs::iso` does, and for the same reason.
//!
//! ## One job at a time
//!
//! The same in-memory slot the pool workflows use, with the same atomically
//! claimed beginning. Conversion is minutes of sequential reads against one
//! spool directory — a second import queued behind the first would only
//! starve it, so the second is refused with the first one's name in the
//! sentence.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use lumen_cluster::join::{StepProgress, StepState, WorkflowPhase};
use lumen_virt::ovf::OvaAppliance;
use lumen_virt::service::{DiskCreate, EfiDiskCreate, NicCreate, VmCreate};
use lumen_virt::{Acknowledgements, CpuModel, DiskBus, Firmware, NicModel, VideoModel};

use crate::error::ApiError;
use crate::AppState;

/// Where uploaded archives wait between upload and commit.
const SPOOL_ROOT: &str = "/var/lib/lumen/import";

const GIB: u64 = 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// The spool.

/// A spooled archive name: one file name ending in `.ova`, nothing that
/// could be read as a path. The same whitelist discipline as the media
/// library — this is the only place an operator-supplied string becomes a
/// path under the spool, so it is the only place traversal has to be
/// stopped.
pub fn valid_ova_name(name: &str) -> bool {
    name.len() <= 128
        && name.to_lowercase().ends_with(".ova")
        && name.len() > ".ova".len()
        && !name.starts_with('.')
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// One spooled archive, as the console lists it.
#[derive(Debug, Clone, Serialize)]
pub struct ImportView {
    pub name: String,
    /// Bytes on disk — the archive, not the machine.
    pub size: u64,
    /// The machine the archive describes. Absent when the archive can no
    /// longer be read, with the reason alongside.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub appliance: Option<OvaAppliance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// An upload in progress, streamed straight to disk. The same shape as
/// `lumen_zfs::IsoUpload` because it answers the same problem.
pub struct OvaUpload {
    file: tokio::fs::File,
    partial: PathBuf,
    final_path: PathBuf,
    written: u64,
}

impl OvaUpload {
    pub async fn write(&mut self, chunk: &[u8]) -> Result<(), ApiError> {
        use tokio::io::AsyncWriteExt;
        self.file
            .write_all(chunk)
            .await
            .map_err(|err| anyhow::anyhow!("writing the uploaded archive: {err}"))?;
        self.written += chunk.len() as u64;
        Ok(())
    }

    /// Flush, then give the file its real name. Empty uploads are refused —
    /// a zero-byte archive would sit in the list looking importable.
    pub async fn finish(mut self) -> Result<u64, ApiError> {
        use tokio::io::AsyncWriteExt;
        self.file.flush().await.ok();
        self.file.sync_all().await.ok();
        drop(self.file);
        if self.written == 0 {
            let _ = tokio::fs::remove_file(&self.partial).await;
            return Err(ApiError::Conflict(
                "The upload contained no data, so nothing was stored.".into(),
            ));
        }
        tokio::fs::rename(&self.partial, &self.final_path)
            .await
            .map_err(|err| {
                ApiError::Internal(anyhow::anyhow!(
                    "publishing {}: {err}",
                    self.final_path.display()
                ))
            })?;
        Ok(self.written)
    }

    pub async fn abort(self) {
        drop(self.file);
        let _ = tokio::fs::remove_file(&self.partial).await;
    }
}

// ---------------------------------------------------------------------------
// Progress.

/// One import as the console polls it.
#[derive(Debug, Clone, Serialize)]
pub struct ImportProgress {
    /// The archive being imported.
    pub source: String,
    /// Known once the machine is defined — which is the moment there is a
    /// machine for the console to link to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vmid: Option<u32>,
    pub phase: WorkflowPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub steps: Vec<StepProgress>,
}

/// Everything the import feature keeps on [`AppState`], as one field: the
/// spool, the converter behind its seam, and the job slot. One field because
/// thirteen `AppState` literals should not each grow three lines.
#[derive(Clone)]
pub struct ImportState {
    spool: PathBuf,
    converter: Arc<dyn Convert>,
    slot: Arc<Mutex<Option<ImportProgress>>>,
}

impl Default for ImportState {
    fn default() -> Self {
        Self::new(SPOOL_ROOT, Arc::new(QemuImg))
    }
}

impl ImportState {
    /// Rooted somewhere else with a different converter — the seam the tests
    /// use, so none of them needs a hypervisor's tool or the appliance's
    /// directory layout.
    pub fn new(spool: impl Into<PathBuf>, converter: Arc<dyn Convert>) -> Self {
        Self {
            spool: spool.into(),
            converter,
            slot: Arc::new(Mutex::new(None)),
        }
    }

    /// The absolute path of one archive, both components checked.
    pub fn path(&self, name: &str) -> Result<PathBuf, ApiError> {
        if !valid_ova_name(name) {
            return Err(ApiError::Conflict(format!(
                "\"{name}\" is not a usable archive name. Use a single file name ending in .ova."
            )));
        }
        Ok(self.spool.join(name))
    }

    /// Open an archive for upload. The spool is made on first use; a node
    /// whose unit predates the import feature cannot write here, and the
    /// remedy — updated packages and a restarted control plane — is said in
    /// the refusal.
    pub async fn begin_upload(
        &self,
        name: &str,
        expected: Option<u64>,
    ) -> Result<OvaUpload, ApiError> {
        let final_path = self.path(name)?;
        tokio::fs::create_dir_all(&self.spool)
            .await
            .map_err(|err| {
                ApiError::Conflict(format!(
                    "The import spool at {} cannot be written ({err}). Update the appliance \
                 packages and restart the control plane so its unit allows the spool.",
                    self.spool.display()
                ))
            })?;
        if tokio::fs::metadata(&final_path).await.is_ok() {
            return Err(ApiError::Conflict(format!(
                "\"{name}\" is already in the import spool. Remove it first, or import it."
            )));
        }
        // An archive is tens of gigabytes; finding out at 98% that the last
        // two do not fit helps nobody. The size is known up front whenever
        // the console sends one, so it is checked up front.
        if let (Some(expected), Some(free)) = (expected, free_bytes(&self.spool)) {
            if expected > free {
                return Err(ApiError::Conflict(format!(
                    "The archive is {expected} bytes but the spool has {free} free. Free some \
                     space on the node, or import from a smaller export."
                )));
            }
        }
        let partial = final_path.with_extension("ova.part");
        let file = tokio::fs::File::create(&partial).await.map_err(|err| {
            ApiError::Internal(anyhow::anyhow!("creating {}: {err}", partial.display()))
        })?;
        Ok(OvaUpload {
            file,
            partial,
            final_path,
            written: 0,
        })
    }

    /// Every archive in the spool, each with the machine it describes.
    pub async fn list(&self) -> Result<Vec<ImportView>, ApiError> {
        let mut entries = match tokio::fs::read_dir(&self.spool).await {
            Ok(entries) => entries,
            // No spool yet is the ordinary state of a node that has never
            // imported anything.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(ApiError::Internal(anyhow::anyhow!(
                    "reading the import spool: {err}"
                )))
            }
        };
        let mut found = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|err| ApiError::Internal(anyhow::anyhow!("listing the import spool: {err}")))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            // The same rule the writer enforces, applied to what is on disk.
            // `.part` files are uploads that never finished — evidence, not
            // offers.
            if !valid_ova_name(&name) {
                continue;
            }
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let path = entry.path();
            let appliance =
                tokio::task::spawn_blocking(move || lumen_virt::ovf::read_appliance(&path))
                    .await
                    .map_err(|err| ApiError::Internal(err.into()))?;
            let (appliance, error) = match appliance {
                Ok(appliance) => (Some(appliance), None),
                Err(err) => (None, Some(err.to_string())),
            };
            found.push(ImportView {
                name,
                size: meta.len(),
                appliance,
                error,
            });
        }
        found.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(found)
    }

    /// Remove one archive. Refused while the running import is reading it —
    /// deleting the source under a half-filled disk is not a race worth
    /// having.
    pub async fn delete(&self, name: &str) -> Result<(), ApiError> {
        let path = self.path(name)?;
        if let Some(progress) = self.snapshot() {
            if progress.phase == WorkflowPhase::Running && progress.source == name {
                return Err(ApiError::Conflict(format!(
                    "\"{name}\" is being imported right now. Wait for the import to finish."
                )));
            }
        }
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(ApiError::NotFound(
                format!("No archive named \"{name}\" in the import spool."),
            )),
            Err(err) => Err(ApiError::Internal(anyhow::anyhow!(
                "removing {}: {err}",
                path.display()
            ))),
        }
    }

    pub fn snapshot(&self) -> Option<ImportProgress> {
        self.slot.lock().unwrap().clone()
    }

    /// Take the job slot, atomically — the same one locked act as the pool
    /// workflows' `try_begin`, for the same race.
    fn try_begin(&self, progress: ImportProgress) -> bool {
        let mut slot = self.slot.lock().unwrap();
        if matches!(
            slot.as_ref(),
            Some(ImportProgress {
                phase: WorkflowPhase::Running,
                ..
            })
        ) {
            return false;
        }
        *slot = Some(progress);
        true
    }

    fn set_vmid(&self, vmid: u32) {
        if let Some(progress) = self.slot.lock().unwrap().as_mut() {
            progress.vmid = Some(vmid);
        }
    }

    fn set_step(&self, step: &str, state: StepState, detail: Option<String>) {
        if let Some(progress) = self.slot.lock().unwrap().as_mut() {
            if let Some(found) = progress.steps.iter_mut().find(|s| s.step == step) {
                found.state = state;
                found.detail = detail;
            }
        }
    }

    fn finish(&self, phase: WorkflowPhase, error: Option<String>) {
        if let Some(progress) = self.slot.lock().unwrap().as_mut() {
            progress.phase = phase;
            progress.error = error;
        }
    }
}

/// Free bytes on the filesystem holding `path`, when the OS will say.
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let cstr = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cstr.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail.saturating_mul(stat.f_frsize))
}

// ---------------------------------------------------------------------------
// The converter seam.

/// Filling one block device from one disk inside one archive. A trait for
/// the same reason the hypervisor is one: the real implementation shells out
/// to a tool the test runner does not have.
#[async_trait]
pub trait Convert: Send + Sync {
    /// Read the disk at `offset`..`offset+size` inside `archive` and write
    /// its decompressed blocks over `device`. `progress` is called with
    /// 0–100 as the tool reports it.
    async fn fill(
        &self,
        archive: &Path,
        offset: u64,
        size: u64,
        device: &str,
        progress: &(dyn Fn(u8) + Send + Sync),
    ) -> anyhow::Result<()>;
}

/// The real converter: `qemu-img convert`, reading the VMDK in place.
///
/// The source is a stacked block-layer spec rather than a file name: the
/// `raw` driver's `offset`/`size` options window the archive down to one
/// member, and the `vmdk` driver reads its stream-compressed format through
/// that window. The archive is never unpacked — the spool holds one copy of
/// the bytes, not two.
pub struct QemuImg;

#[async_trait]
impl Convert for QemuImg {
    async fn fill(
        &self,
        archive: &Path,
        offset: u64,
        size: u64,
        device: &str,
        progress: &(dyn Fn(u8) + Send + Sync),
    ) -> anyhow::Result<()> {
        let source = serde_json::json!({
            "driver": "vmdk",
            "file": {
                "driver": "raw",
                "offset": offset,
                "size": size,
                "file": { "driver": "file", "filename": archive.to_string_lossy() },
            },
        });
        let mut child = tokio::process::Command::new("qemu-img")
            .arg("convert")
            .arg("-p")
            // -n: the target volume already exists at the right size, made
            // by the same path every machine's disks are made by. qemu-img
            // must write it, not try to create it.
            .arg("-n")
            .args(["-O", "raw"])
            .arg(format!("json:{source}"))
            .arg(device)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("starting qemu-img — is the qemu-img package installed?")?;

        // -p writes "(12.34/100%)" to stdout as it goes; the last one seen
        // is the truth. Read as bytes: the updates are \r-separated.
        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut stderr = child.stderr.take().expect("stderr was piped");
        let reader = async move {
            let mut tail = String::new();
            let mut buffer = [0u8; 4096];
            loop {
                match stdout.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        tail.push_str(&String::from_utf8_lossy(&buffer[..n]));
                        if let Some(percent) = last_percent(&tail) {
                            progress(percent);
                        }
                        // Only the tail can hold a partial number; the rest
                        // has been read.
                        if tail.len() > 256 {
                            tail = tail.split_off(tail.len() - 64);
                        }
                    }
                }
            }
        };
        let errors = async move {
            let mut text = String::new();
            stderr.read_to_string(&mut text).await.ok();
            text
        };
        let (_, errors, status) = tokio::join!(reader, errors, child.wait());
        let status = status.context("waiting for qemu-img")?;
        if !status.success() {
            let reason = errors.trim();
            anyhow::bail!(
                "qemu-img could not fill the disk{}",
                if reason.is_empty() {
                    format!(" (exit {status})")
                } else {
                    format!(": {reason}")
                }
            );
        }
        progress(100);
        Ok(())
    }
}

/// The last "(NN.NN/100%)" in the text, as a whole percent.
fn last_percent(text: &str) -> Option<u8> {
    let start = text.rfind('(')?;
    let rest = &text[start + 1..];
    let slash = rest.find('/')?;
    let value: f32 = rest[..slash].trim().parse().ok()?;
    Some(value.clamp(0.0, 100.0) as u8)
}

/// A converter that records what it was asked and touches nothing — what
/// the flow tests inject, the way every domain injects its mock backend.
#[derive(Default)]
pub struct MockConvert {
    pub calls: Mutex<Vec<(PathBuf, u64, u64, String)>>,
    /// Fail the nth call (0-based), to exercise the unwind.
    pub fail_on: Option<usize>,
}

#[async_trait]
impl Convert for MockConvert {
    async fn fill(
        &self,
        archive: &Path,
        offset: u64,
        size: u64,
        device: &str,
        progress: &(dyn Fn(u8) + Send + Sync),
    ) -> anyhow::Result<()> {
        let count = {
            let mut calls = self.calls.lock().unwrap();
            calls.push((archive.to_path_buf(), offset, size, device.to_string()));
            calls.len() - 1
        };
        if self.fail_on == Some(count) {
            anyhow::bail!("the pretend converter was told to fail here");
        }
        progress(100);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The commit.

/// POST /api/vms/import/{name} — the operator's corrected proposal.
///
/// Every hardware fact the archive already states is optional here and
/// defaults to what the archive said; what the archive *cannot* know — which
/// bridge each source network maps to, which pool each disk lands in — is
/// exactly what the required fields are.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmImportCommit {
    /// The machine's name here.
    pub name: String,
    #[serde(default)]
    pub vmid: Option<u32>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub vcpus: Option<u32>,
    #[serde(default)]
    pub memory_mib: Option<u64>,
    #[serde(default)]
    pub cpu_model: Option<CpuModel>,
    /// Absent honors the archive: the machine boots with the firmware it was
    /// built on, because the other one does not boot it at all.
    #[serde(default)]
    pub firmware: Option<Firmware>,
    #[serde(default)]
    pub tpm: bool,
    /// Where the UEFI variable store lives, for a UEFI import.
    #[serde(default)]
    pub efi_disk: Option<EfiDiskCreate>,
    #[serde(default)]
    pub video: Option<VideoModel>,
    #[serde(default = "default_true")]
    pub guest_agent: bool,
    #[serde(default)]
    pub start_on_boot: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub os_id: Option<String>,
    /// One entry per archive disk, in the archive's order.
    pub disks: Vec<ImportDiskTarget>,
    /// One entry per archive adapter, in the archive's order. An entry with
    /// no bridge imports the machine without that adapter.
    #[serde(default)]
    pub nics: Vec<ImportNicTarget>,
    /// Start it once its disks are filled.
    #[serde(default)]
    pub start: bool,
    /// Leave the archive in the spool afterwards — for importing the same
    /// appliance more than once.
    #[serde(default)]
    pub keep_source: bool,
}

fn default_true() -> bool {
    true
}

/// Where one archive disk lands.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportDiskTarget {
    /// The local pool, empty for a replicated disk.
    #[serde(default)]
    pub pool: String,
    #[serde(default)]
    pub replicated: bool,
    /// Absent keeps the bus the source machine had it on.
    #[serde(default)]
    pub bus: Option<DiskBus>,
    #[serde(default = "default_true")]
    pub discard: bool,
}

/// What one source adapter becomes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportNicTarget {
    /// The bridge it attaches to. Absent leaves the adapter behind.
    #[serde(default)]
    pub bridge: Option<String>,
    /// Absent keeps the source's model, so its driver still binds.
    #[serde(default)]
    pub model: Option<NicModel>,
    #[serde(default)]
    pub vlan_tag: Option<u16>,
    /// Absent keeps the source's address.
    #[serde(default)]
    pub mac: Option<String>,
}

/// Start the import job: validate against the archive, claim the slot,
/// answer 202 with the first snapshot while the work runs behind it.
pub async fn start_import(
    state: &Arc<AppState>,
    source: String,
    commit: VmImportCommit,
    by: String,
) -> Result<ImportProgress, ApiError> {
    let path = state.import.path(&source)?;
    if tokio::fs::metadata(&path).await.is_err() {
        return Err(ApiError::NotFound(format!(
            "No archive named \"{source}\" in the import spool."
        )));
    }
    let appliance = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || lumen_virt::ovf::read_appliance(&path))
            .await
            .map_err(|err| ApiError::Internal(err.into()))??
    };

    // The commit maps the archive's hardware one-to-one; a count that does
    // not match means the console and the archive disagree about what is
    // being imported, and nothing should be created off that.
    if commit.disks.len() != appliance.disks.len() {
        return Err(ApiError::Conflict(format!(
            "\"{source}\" contains {} disk(s), but the request places {}.",
            appliance.disks.len(),
            commit.disks.len()
        )));
    }
    if commit.nics.len() != appliance.nics.len() {
        return Err(ApiError::Conflict(format!(
            "\"{source}\" contains {} network adapter(s), but the request maps {}.",
            appliance.nics.len(),
            commit.nics.len()
        )));
    }

    let create = plan(&appliance, &commit);

    // The steps, all visible from the first poll: define, one fill per
    // disk, then what the operator asked for afterwards.
    let mut steps = vec![StepProgress {
        step: "define".into(),
        node: None,
        state: StepState::Pending,
        detail: Some(format!("Define {}", commit.name)),
    }];
    for disk in &appliance.disks {
        steps.push(StepProgress {
            step: disk.file.clone(),
            node: None,
            state: StepState::Pending,
            detail: None,
        });
    }
    if commit.start {
        steps.push(StepProgress {
            step: "start".into(),
            node: None,
            state: StepState::Pending,
            detail: None,
        });
    }
    if !commit.keep_source {
        steps.push(StepProgress {
            step: "tidy".into(),
            node: None,
            state: StepState::Pending,
            detail: Some(format!("Remove {source} from the spool")),
        });
    }

    let progress = ImportProgress {
        source: source.clone(),
        vmid: None,
        phase: WorkflowPhase::Running,
        error: None,
        steps,
    };
    if !state.import.try_begin(progress.clone()) {
        let running = state
            .import
            .snapshot()
            .map(|p| p.source)
            .unwrap_or_default();
        return Err(ApiError::Conflict(format!(
            "An import of \"{running}\" is already running. One import at a time."
        )));
    }

    let state = state.clone();
    tokio::spawn(async move {
        run_import(&state, path, appliance, create, commit, by).await;
    });
    Ok(progress)
}

/// The archive's machine and the operator's corrections, as the create
/// request the compute domain already knows how to refuse or perform.
fn plan(appliance: &OvaAppliance, commit: &VmImportCommit) -> VmCreate {
    let disks = appliance
        .disks
        .iter()
        .zip(&commit.disks)
        .map(|(disk, target)| DiskCreate {
            pool: target.pool.clone(),
            // The volume must hold every byte the guest thinks it has;
            // rounding up to the operator's unit is the cheap direction.
            size_gib: disk.capacity.div_ceil(GIB).max(1),
            bus: target.bus.unwrap_or(disk.bus),
            cache: Default::default(),
            discard: target.discard,
            blocksize: None,
            replicated: target.replicated,
        })
        .collect();
    let nics = appliance
        .nics
        .iter()
        .zip(&commit.nics)
        .filter_map(|(nic, target)| {
            target.bridge.clone().map(|bridge| NicCreate {
                bridge,
                model: target.model.unwrap_or(nic.model),
                vlan_tag: target.vlan_tag,
                mac: target.mac.clone().or_else(|| nic.mac.clone()),
            })
        })
        .collect();
    VmCreate {
        name: commit.name.clone(),
        vmid: commit.vmid,
        description: commit.description.clone(),
        vcpus: commit.vcpus.unwrap_or(appliance.vcpus),
        memory_mib: commit.memory_mib.unwrap_or(appliance.memory_mib),
        cpu_model: commit.cpu_model.clone().unwrap_or_default(),
        topology: None,
        machine: None,
        firmware: commit.firmware.unwrap_or(appliance.firmware),
        scsi_controller: appliance.scsi_controller,
        tpm: commit.tpm,
        efi_disk: commit.efi_disk.clone(),
        video: commit.video.unwrap_or_default(),
        // No installer here: the machine boots the disk it arrived on.
        boot_order: None,
        start_on_boot: commit.start_on_boot,
        guest_agent: commit.guest_agent,
        ha: false,
        tags: commit.tags.clone(),
        os_id: commit.os_id.clone(),
        disks,
        cdroms: Vec::new(),
        nics,
        // Never at define time: the disks are blank until the fills below
        // finish, and a machine booted off a blank disk answers nothing.
        start: false,
    }
}

/// The job itself. Every early return has already written its failure into
/// the slot; a failure after the machine exists removes it again, disks and
/// all — half an import must not be left looking like a machine.
async fn run_import(
    state: &Arc<AppState>,
    archive: PathBuf,
    appliance: OvaAppliance,
    create: VmCreate,
    commit: VmImportCommit,
    by: String,
) {
    let import = &state.import;
    let source = archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Define, with blank volumes at the right sizes.
    import.set_step("define", StepState::Running, None);
    let requested = create.vmid;
    let start_wanted = commit.start;
    let result = state.virt.create(create).await;
    let vm = match result {
        Ok(vm) => vm,
        Err(err) => {
            let reason = err.to_string();
            if let Some(vmid) = requested {
                state.tasks.record(
                    vmid,
                    "import",
                    format!("Import {source}"),
                    by,
                    Some(reason.clone()),
                );
            }
            import.set_step("define", StepState::Failed, Some(reason.clone()));
            import.finish(WorkflowPhase::Failed, Some(reason));
            return;
        }
    };
    let vmid = vm.vmid;
    import.set_vmid(vmid);
    import.set_step(
        "define",
        StepState::Done,
        Some(format!("{} is machine {vmid}", vm.name)),
    );
    state.tasks.record(
        vmid,
        "import",
        format!("Import {source} as {}", vm.name),
        by.clone(),
        None,
    );
    crate::api::vms::sync_definition(state, vmid).await;

    // Fill each volume from the archive, in order. The device paths come
    // from the defined machine itself — the same truth the machine will
    // boot from.
    for (index, disk) in appliance.disks.iter().enumerate() {
        let step = disk.file.clone();
        let Some(device) = vm.disks.get(index).map(|d| d.source.clone()) else {
            // Cannot happen — the create request had one disk per archive
            // disk — but if it ever does, refusing beats filling the wrong
            // volume.
            unwind(
                state,
                vmid,
                &step,
                "The defined machine is missing a disk.",
                &by,
            )
            .await;
            return;
        };
        import.set_step(&step, StepState::Running, Some("0%".into()));
        let progress = {
            let import = import.clone();
            let step = step.clone();
            move |percent: u8| {
                import.set_step(&step, StepState::Running, Some(format!("{percent}%")));
            }
        };
        let filled = import
            .converter
            .fill(&archive, disk.offset, disk.size, &device, &progress)
            .await;
        if let Err(err) = filled {
            unwind(state, vmid, &step, &format!("{err:#}"), &by).await;
            return;
        }
        import.set_step(&step, StepState::Done, None);
    }

    if start_wanted {
        import.set_step("start", StepState::Running, None);
        match state.virt.start(vmid).await {
            Ok(_) => import.set_step("start", StepState::Done, None),
            Err(err) => {
                // The machine is imported whole; only the start failed. That
                // is a finished import with a sentence attached, not a
                // failure to unwind — removing a complete machine over it
                // would destroy exactly what was just built.
                import.set_step("start", StepState::Failed, Some(err.to_string()));
                state.tasks.record(
                    vmid,
                    "start",
                    "Start after import".into(),
                    by.clone(),
                    Some(err.to_string()),
                );
            }
        }
    }

    if !commit.keep_source {
        import.set_step("tidy", StepState::Running, None);
        match tokio::fs::remove_file(&archive).await {
            Ok(()) => import.set_step("tidy", StepState::Done, None),
            Err(err) => import.set_step(
                "tidy",
                StepState::Failed,
                Some(format!("The archive could not be removed: {err}")),
            ),
        }
    }
    import.finish(WorkflowPhase::Complete, None);
}

/// A failure after the machine exists: mark the step, remove the machine
/// and every volume it was given, and close the job as failed.
async fn unwind(state: &Arc<AppState>, vmid: u32, step: &str, reason: &str, by: &str) {
    let import = &state.import;
    import.set_step(step, StepState::Failed, Some(reason.to_string()));
    state.tasks.record(
        vmid,
        "import",
        "Remove the half-imported machine".into(),
        by.to_string(),
        Some(reason.to_string()),
    );
    let removed = state
        .virt
        .delete(
            vmid,
            true,
            Acknowledgements {
                may_lose_data: true,
            },
        )
        .await;
    let error = match removed {
        Ok(_) => reason.to_string(),
        // The disks that could not be removed are named so the operator can
        // finish the job by hand — the one thing worse than a failed import
        // is one that quietly kept the space.
        Err(err) => format!("{reason} Removing the half-imported machine also failed: {err}"),
    };
    import.finish(WorkflowPhase::Failed, Some(error));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_names_stay_file_names() {
        for good in ["web.ova", "web-frontend_v2.OVA", "a.b.ova"] {
            assert!(valid_ova_name(good), "{good}");
        }
        for bad in [
            "",
            ".ova",
            "web.iso",
            "../../etc/passwd.ova",
            "a/b.ova",
            "-flag.ova",
            ".hidden.ova",
            "sp ace.ova",
        ] {
            assert!(!valid_ova_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_last_percent_wins() {
        assert_eq!(last_percent("    (0.00/100%)\r    (12.34/100%)"), Some(12));
        assert_eq!(last_percent("(100.00/100%)"), Some(100));
        assert_eq!(last_percent("no numbers here"), None);
        assert_eq!(last_percent("(garbage/100%)"), None);
    }
}
