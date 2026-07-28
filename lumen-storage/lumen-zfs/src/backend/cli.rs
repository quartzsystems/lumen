//! The real backend: the supported command-line interface, driven with typed
//! argument arrays.
//!
//! **No shell, ever.** Every invocation is an `argv` array handed straight to
//! `execve` through `tokio::process::Command`, built from typed values — there
//! is no string to interpolate into and therefore nothing to escape. This is
//! the same discipline `lumen_net::plan::Op` enforces on the networking side,
//! where the set of things that can be asked for is an enumeration rather than
//! a sentence.
//!
//! It is the command line rather than the library on purpose: `libzfs` has no
//! stable ABI, changes shape between releases, and binding it would put a
//! generated-bindings step (and libclang) into a toolchain that deliberately
//! has neither. The command line is the interface upstream supports, `-H -p`
//! makes it machine-readable, and a version that adds a column does not break
//! a reader that asks for its columns by name.
//!
//! Almost nothing here needs the control plane's unit relaxed: dataset and
//! volume operations reach the kernel through `/dev/zfs`, which
//! `ProtectSystem=strict` does not cover. See docs/compute.md for the
//! reproduction.
//!
//! **Pool** operations are the exception, and the only one. `zpool create` and
//! `zpool destroy` write `/etc/zfs/zpool.cache`, which that same setting makes
//! read-only, so those two are handed to systemd through
//! [`lumen_sys::exec`] and run outside the daemon's namespace. Everything else
//! on this page is a direct `execve` from inside it. The split is exactly the
//! set of operations that touch that one file — see docs/system.md.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;
use tokio::time::timeout;

use lumen_sys::exec::{Exec, Request as ExecRequest};

use crate::backend::ZfsBackend;
use crate::devices::{self, DeviceRoots};
use crate::error::{Result, ZfsError};
use crate::model::{
    is_lumen_volume, is_reserved_leaf, iso_dataset, iso_mountpoint, lumen_root, valid_device_path,
    valid_new_pool_name, valid_pool_name, BlockDevice, Dataset, DatasetKind, Pool, PoolHealth,
    PoolRequest, VolumeRequest, DEFAULT_ASHIFT,
};

/// Columns asked for by name, so a release that adds one does not shift the
/// fields out from under the parser.
const POOL_COLUMNS: &str = "name,size,alloc,free,frag,dedup,health,readonly";
const DATASET_COLUMNS: &str = "name,type,used,avail,refer,volsize,volblocksize,mountpoint";

/// How long one of these may take before it is given up on.
///
/// These are the *reads* — `zpool list`, `zfs list` — and they answer in
/// milliseconds on a healthy node. They do not answer at all on an unhealthy
/// one: a pool with a disk that has stopped responding blocks in the kernel,
/// and `zpool list` blocks with it. Several of these run before a pool is ever
/// created (`create_pool` observes the existing pools first), so an unbounded
/// read here hangs the create request without a single command having been
/// delegated — which looks exactly like a hung `zpool create` and is not one.
const DEADLINE: Duration = Duration::from_secs(30);

/// util-linux, which is on every node there has ever been — so clearing a
/// disk adds no package dependency. Absolute for the same reason `zpool` is:
/// the transient unit systemd starts has its own environment and should not
/// be resolving a program out of this daemon's `PATH`.
const WIPEFS: &str = "/usr/sbin/wipefs";
const BLOCKDEV: &str = "/usr/sbin/blockdev";

pub struct CliBackend {
    zfs: String,
    zpool: String,
    /// How a pool operation leaves the sandbox. Only the two that write
    /// `/etc/zfs/zpool.cache` go through it.
    exec: Arc<dyn Exec>,
    /// Absolute, because the transient unit systemd starts has its own
    /// environment and should not be resolving a program out of a `PATH` this
    /// daemon happened to have.
    zpool_path: String,
    devices: DeviceRoots,
}

impl CliBackend {
    pub fn new(exec: Arc<dyn Exec>) -> Self {
        Self {
            zfs: "zfs".into(),
            zpool: "zpool".into(),
            exec,
            zpool_path: "/usr/sbin/zpool".into(),
            devices: DeviceRoots::default(),
        }
    }

    /// Read the node's disks from somewhere else, for a test.
    pub fn with_device_roots(mut self, devices: DeviceRoots) -> Self {
        self.devices = devices;
        self
    }

    /// Confirm the tools are actually there, so a node without storage swaps
    /// in `UnavailableBackend` at startup instead of failing every request
    /// with a spawn error.
    pub async fn probe(exec: Arc<dyn Exec>) -> Result<Self> {
        let backend = Self::new(exec);
        backend
            .run(&backend.zpool.clone(), &["list", "-H", "-o", "name"])
            .await?;
        Ok(backend)
    }

    /// A pool command, run outside the sandbox.
    async fn run_privileged(&self, description: String, args: Vec<String>) -> Result<()> {
        let outcome = self
            .exec
            .run(ExecRequest::new(description, &self.zpool_path).args(args))
            .await
            .map_err(ZfsError::Backend)?;
        if !outcome.ok() {
            // The tool's own words. "The pool operation failed" helps nobody,
            // and zpool's messages are unusually good.
            return Err(ZfsError::Conflict(outcome.failure()));
        }
        Ok(())
    }

    /// A privileged command that is not `zpool`. Clearing a disk needs
    /// util-linux, and it needs it outside the sandbox for the same reason
    /// `zpool create` runs outside it: this writes to a block device.
    async fn run_privileged_program(
        &self,
        description: String,
        program: &str,
        args: Vec<String>,
    ) -> Result<()> {
        let outcome = self
            .exec
            .run(ExecRequest::new(description, program).args(args))
            .await
            .map_err(ZfsError::Backend)?;
        if !outcome.ok() {
            return Err(ZfsError::Conflict(outcome.failure()));
        }
        Ok(())
    }

    /// The argument array for `zpool create`.
    ///
    /// Split out so it can be asserted on without a pool: this is the one
    /// command in the tree where getting an argument wrong destroys data, and
    /// a test that reads it is worth more than one that runs it.
    fn create_argv(&self, request: &PoolRequest) -> Vec<String> {
        let mut argv: Vec<String> = vec!["create".into()];
        if request.force {
            // Only ever set when the operator acknowledged a disk that already
            // had something on it — the service will not build the request
            // otherwise.
            argv.push("-f".into());
        }
        argv.extend([
            "-o".into(),
            format!("ashift={}", request.ashift.unwrap_or(DEFAULT_ASHIFT)),
            "-o".into(),
            format!("autotrim={}", if request.autotrim { "on" } else { "off" }),
            "-O".into(),
            format!("compression={}", request.compression.as_str()),
            // The four the installer sets on the root pool, so a pool made
            // here behaves like the one made at install time rather than
            // subtly differently.
            "-O".into(),
            "acltype=posixacl".into(),
            "-O".into(),
            "xattr=sa".into(),
            "-O".into(),
            "dnodesize=auto".into(),
            "-O".into(),
            "relatime=on".into(),
            // The pool root holds nothing itself and must not appear at
            // /<name> — a pool called "boot" mounting over /boot is precisely
            // the accident this avoids.
            "-O".into(),
            "canmount=off".into(),
            "-O".into(),
            "mountpoint=none".into(),
            request.name.clone(),
        ]);
        if let Some(keyword) = request.vdev.keyword() {
            argv.push(keyword.into());
        }
        argv.extend(request.disks.iter().cloned());
        argv
    }

    /// One invocation. Output is captured; a non-zero exit becomes an error
    /// carrying what the tool actually said, because "storage command failed"
    /// helps nobody.
    async fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        let invocation = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();

        // Bounded: see DEADLINE. `kill_on_drop` means giving up also ends the
        // process, which is safe precisely because everything routed through
        // here is a read — there is no half-finished write to leave behind.
        let output = match timeout(DEADLINE, invocation).await {
            Ok(result) => result.map_err(|err| {
                ZfsError::Backend(anyhow::Error::from(err).context(format!(
                    "running {program} {} — is the storage software installed?",
                    args.join(" ")
                )))
            })?,
            Err(_) => {
                return Err(ZfsError::Backend(anyhow::anyhow!(
                    "{program} {} did not answer within {} seconds. That is usually a pool with a \
                     disk that has stopped responding — `zpool status` from a shell on the node \
                     will say which, and will block in the same place if so.",
                    args.join(" "),
                    DEADLINE.as_secs()
                )));
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                format!("{program} exited with {}", output.status)
            } else {
                stderr
            };
            return Err(ZfsError::Backend(anyhow::anyhow!("{detail}")));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// `-H` separates fields with a single tab and never pads, so splitting on tab
/// is exact. A value the tool has none of is a bare `-`.
fn fields(line: &str) -> Vec<&str> {
    line.split('\t').collect()
}

fn optional<'a>(value: Option<&&'a str>) -> Option<&'a str> {
    value.copied().filter(|v| *v != "-" && !v.is_empty())
}

fn number(value: Option<&&str>) -> Option<u64> {
    optional(value).and_then(|v| v.parse().ok())
}

/// A percentage that may or may not carry its sign, depending on release.
fn percent(value: Option<&&str>) -> Option<u8> {
    optional(value)
        .map(|v| v.trim_end_matches('%'))
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.round().clamp(0.0, 100.0) as u8)
}

/// A ratio printed either as `1.00` or as `1.00x`.
fn ratio(value: Option<&&str>) -> Option<f64> {
    optional(value)
        .map(|v| v.trim_end_matches('x'))
        .and_then(|v| v.parse().ok())
}

fn parse_pools(stdout: &str) -> Vec<Pool> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let f = fields(line);
            let name = optional(f.first())?.to_string();
            Some(Pool {
                name,
                size: number(f.get(1)).unwrap_or(0),
                allocated: number(f.get(2)).unwrap_or(0),
                free: number(f.get(3)).unwrap_or(0),
                fragmentation: percent(f.get(4)),
                dedup_ratio: ratio(f.get(5)),
                health: optional(f.get(6))
                    .map(PoolHealth::parse)
                    .unwrap_or_default(),
                read_only: optional(f.get(7)) == Some("on"),
            })
        })
        .collect()
}

/// `zpool list -vHP`: which device each imported pool is built on.
///
/// The tree arrives flattened, and indentation is the only thing that says
/// what belongs to what. A row starting at the left margin names a pool; an
/// indented row is one of its vdevs, which is either a container
/// (`mirror-0`, `raidz1-0`, `logs`, `spares`) or an actual device. Only the
/// devices matter here, and `-P` is what makes them recognisable: with whole
/// paths, a device row is exactly the one starting with a slash.
///
/// Log, cache, and spare devices are counted the same as data ones. They
/// carry pool labels too, and clearing one out from under a running pool is
/// the accident this list exists to prevent.
fn parse_pool_members(stdout: &str) -> Vec<(String, String)> {
    let mut members = Vec::new();
    let mut pool: Option<&str> = None;

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indented = line.starts_with(char::is_whitespace);
        // A child row begins with the separator itself, so the first field is
        // empty and the name is in the second. Taking the first field with
        // anything in it covers both shapes without counting tabs.
        let Some(first) = fields(line)
            .into_iter()
            .map(str::trim)
            .find(|f| !f.is_empty())
        else {
            continue;
        };
        if !indented {
            pool = Some(first);
            continue;
        }
        // A container vdev, not a disk. Named rather than pathed, which is
        // what tells them apart once `-P` is in play.
        if !first.starts_with('/') {
            continue;
        }
        if let Some(name) = pool {
            members.push((first.to_string(), name.to_string()));
        }
    }
    members
}

fn parse_datasets(stdout: &str) -> Vec<Dataset> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let f = fields(line);
            let name = optional(f.first())?.to_string();
            Some(Dataset {
                name,
                kind: optional(f.get(1))
                    .map(DatasetKind::parse)
                    .unwrap_or_default(),
                used: number(f.get(2)).unwrap_or(0),
                available: number(f.get(3)),
                referenced: number(f.get(4)).unwrap_or(0),
                volsize: number(f.get(5)),
                volblocksize: number(f.get(6)),
                // A volume reports "-", and so does an unmounted filesystem.
                mountpoint: optional(f.get(7))
                    .filter(|m| *m != "none" && *m != "legacy")
                    .map(String::from),
            })
        })
        .collect()
}

fn reject_bad_pool(pool: &str) -> Result<()> {
    if valid_pool_name(pool) {
        return Ok(());
    }
    Err(ZfsError::Conflict(format!(
        "\"{pool}\" is not a usable pool name."
    )))
}

/// The last line of defence before a destroy. The service checks this too; a
/// backend that trusts its caller is one refactor away from not being safe.
///
/// The media library matches the volume shape and is explicitly excluded: it
/// is a filesystem full of an operator's installation media, and losing it to
/// a mistyped disk name would be unrecoverable.
fn reject_outside_namespace(path: &str) -> Result<()> {
    if is_reserved_leaf(path) {
        return Err(ZfsError::Conflict(format!(
            "\"{path}\" is this node's installation media library, not a machine's disk."
        )));
    }
    if is_lumen_volume(path) {
        return Ok(());
    }
    Err(ZfsError::Conflict(format!(
        "\"{path}\" is not a volume this appliance created, so it will not be removed. Only \
         volumes under a pool's \"lumen\" dataset are managed here."
    )))
}

#[async_trait]
impl ZfsBackend for CliBackend {
    async fn pools(&self) -> Result<Vec<Pool>> {
        let stdout = self
            .run(&self.zpool, &["list", "-H", "-p", "-o", POOL_COLUMNS])
            .await?;
        Ok(parse_pools(&stdout))
    }

    async fn block_devices(&self) -> Result<Vec<BlockDevice>> {
        Ok(devices::list(&self.devices).await)
    }

    async fn pool_members(&self) -> Result<Vec<(String, String)>> {
        // `-v` descends into the vdev tree, `-P` forces whole paths rather
        // than the shortened names the human format prints, and `-H` drops
        // the header and separates with tabs.
        let stdout = self
            .run(&self.zpool, &["list", "-vHP", "-o", "name"])
            .await?;
        Ok(parse_pool_members(&stdout))
    }

    async fn wipe_disk(&self, device: &BlockDevice) -> Result<()> {
        // Every partition, then the disk. In that order and never the other
        // way round: clearing the table first makes the partitions disappear
        // as devices, and the ZFS labels on them survive to be found by the
        // next `zpool create`.
        let mut targets = devices::partition_paths(&self.devices, &device.name);
        targets.push(device.kernel_path.clone());

        // ZFS labels first, with the tool that knows where they are — four
        // per device, two at each end, which no signature scanner finds all
        // of. Failure here is expected and ignored: most of these devices
        // never held a pool, and `labelclear` says so rather than shrugging.
        for target in &targets {
            let _ = self
                .run_privileged_program(
                    format!("clear any pool label on {target}"),
                    &self.zpool_path,
                    vec!["labelclear".into(), "-f".into(), target.clone()],
                )
                .await;
        }

        // Then every remaining signature, the partition table included. This
        // one must succeed — it is what makes the disk read as empty.
        self.run_privileged_program(
            format!("clear the signatures on {}", device.kernel_path),
            WIPEFS,
            std::iter::once("-a".to_string())
                .chain(targets.iter().cloned())
                .collect(),
        )
        .await?;

        // Tell the kernel the table changed, so the partitions stop existing
        // now rather than at the next reboot. A disk the console still lists
        // partitions for is a disk an operator will think the wipe missed.
        self.run_privileged_program(
            format!("re-read the partition table of {}", device.kernel_path),
            BLOCKDEV,
            vec!["--rereadpt".into(), device.kernel_path.clone()],
        )
        .await
    }

    async fn create_pool(&self, request: &PoolRequest) -> Result<Pool> {
        // Checked again here as well as in the service. A backend that trusts
        // its caller is one refactor away from not being safe, and this is the
        // command that reformats disks.
        if !valid_new_pool_name(&request.name) {
            return Err(ZfsError::Conflict(format!(
                "\"{}\" is not a usable name for a new pool.",
                request.name
            )));
        }
        if request.disks.is_empty() {
            return Err(ZfsError::Conflict("A pool needs at least one disk.".into()));
        }
        for disk in &request.disks {
            if !valid_device_path(disk) {
                return Err(ZfsError::Conflict(format!(
                    "\"{disk}\" is not a device this appliance will build a pool on."
                )));
            }
        }

        self.run_privileged(
            format!("create the storage pool \"{}\"", request.name),
            self.create_argv(request),
        )
        .await?;

        // Read it back rather than assuming: the size a pool reports is the
        // one after ZFS's own overhead, and it is never quite the number the
        // arithmetic in the dialog predicted.
        self.pools()
            .await?
            .into_iter()
            .find(|pool| pool.name == request.name)
            .ok_or_else(|| {
                ZfsError::Backend(anyhow::anyhow!(
                    "created the pool \"{}\" but it is not listed afterwards",
                    request.name
                ))
            })
    }

    async fn destroy_pool(&self, name: &str) -> Result<()> {
        reject_bad_pool(name)?;
        // No -f: a pool with a dataset in use must fail loudly and say which,
        // rather than being torn out from under whatever has it open.
        self.run_privileged(
            format!("destroy the storage pool \"{name}\""),
            vec!["destroy".into(), name.to_string()],
        )
        .await
    }

    async fn datasets(&self, pool: &str) -> Result<Vec<Dataset>> {
        reject_bad_pool(pool)?;
        let stdout = self
            .run(
                &self.zfs,
                &[
                    "list",
                    "-H",
                    "-p",
                    "-r",
                    "-t",
                    "filesystem,volume",
                    "-o",
                    DATASET_COLUMNS,
                    pool,
                ],
            )
            .await
            .map_err(|err| match err {
                // The tool's own words are the clearest answer here, but a
                // missing pool is a 404 rather than a 500.
                ZfsError::Backend(inner) if inner.to_string().contains("does not exist") => {
                    ZfsError::NotFound(format!("No pool named \"{pool}\" on this node."))
                }
                other => other,
            })?;
        Ok(parse_datasets(&stdout))
    }

    async fn ensure_namespace(&self, pool: &str) -> Result<()> {
        reject_bad_pool(pool)?;
        let root = lumen_root(pool);
        // Holds volumes only, so it has nothing to mount.
        let result = self
            .run(&self.zfs, &["create", "-o", "mountpoint=none", &root])
            .await;
        match result {
            Ok(_) => Ok(()),
            // Already there is the ordinary case on every call after the first.
            Err(ZfsError::Backend(err)) if err.to_string().contains("already exists") => Ok(()),
            Err(err) => Err(err),
        }
    }

    async fn ensure_iso_store(&self, pool: &str) -> Result<String> {
        reject_bad_pool(pool)?;
        self.ensure_namespace(pool).await?;
        let dataset = iso_dataset(pool);
        let mountpoint = iso_mountpoint(pool);
        // Unlike the parent, this one holds files and therefore has to be
        // mounted — at the fixed path the control plane's unit names, not at
        // the pool's natural one, which no static unit could enumerate.
        let result = self
            .run(
                &self.zfs,
                &[
                    "create",
                    "-p",
                    "-o",
                    &format!("mountpoint={mountpoint}"),
                    &dataset,
                ],
            )
            .await;
        match result {
            Ok(_) => Ok(mountpoint),
            // Already there is the ordinary case on every call after the
            // first. The mount point is not re-asserted: an operator who moved
            // the library deliberately keeps it where they put it, and the
            // service reads the real one back from the dataset listing.
            Err(ZfsError::Backend(err)) if err.to_string().contains("already exists") => {
                Ok(mountpoint)
            }
            Err(err) => Err(err),
        }
    }

    async fn create_volume(&self, request: &VolumeRequest) -> Result<Dataset> {
        reject_outside_namespace(&request.path)?;
        let size = request.size.to_string();
        let blocksize = request.blocksize.map(|b| b.to_string());

        let mut args: Vec<&str> = vec!["create", "-V", &size];
        if let Some(blocksize) = blocksize.as_deref() {
            args.extend_from_slice(&["-b", blocksize]);
        }
        // volmode=dev: the volume appears under /dev/zvol for the hypervisor
        // and the host does not go looking for partition tables inside a guest
        // disk, which is both a waste and a way for host tooling to latch onto
        // a filesystem the guest owns.
        args.extend_from_slice(&["-o", "volmode=dev", &request.path]);
        self.run(&self.zfs, &args).await?;

        // Read it back rather than assuming: the pool rounds a volume up to
        // its block size, and the number the console shows should be the
        // number the box has.
        let pool = request.path.split('/').next().unwrap_or_default();
        self.datasets(pool)
            .await?
            .into_iter()
            .find(|d| d.name == request.path)
            .ok_or_else(|| {
                ZfsError::Backend(anyhow::anyhow!(
                    "created {} but it is not listed afterwards",
                    request.path
                ))
            })
    }

    async fn destroy_volume(&self, path: &str) -> Result<()> {
        reject_outside_namespace(path)?;
        // Deliberately no -r and no -f: a volume with snapshots, or one a
        // guest still has open, must fail loudly rather than take the
        // snapshots with it.
        self.run(&self.zfs, &["destroy", path]).await?;
        Ok(())
    }

    async fn resize_volume(&self, path: &str, size: u64) -> Result<()> {
        reject_outside_namespace(path)?;
        let volsize = format!("volsize={size}");
        self.run(&self.zfs, &["set", &volsize, path]).await?;
        Ok(())
    }

    async fn snapshot_volume(&self, path: &str, snapshot: &str) -> Result<()> {
        reject_outside_namespace(path)?;
        let target = format!("{path}@{snapshot}");
        self.run(&self.zfs, &["snapshot", &target]).await?;
        Ok(())
    }

    async fn rollback_volume(&self, path: &str, snapshot: &str) -> Result<()> {
        reject_outside_namespace(path)?;
        let target = format!("{path}@{snapshot}");
        // -r discards the snapshots after the one rolled back to — the only
        // way zfs will roll back past them, and exactly what "roll back"
        // means. The acknowledgement for that lives with the caller.
        self.run(&self.zfs, &["rollback", "-r", &target]).await?;
        Ok(())
    }

    async fn destroy_snapshot(&self, path: &str, snapshot: &str) -> Result<()> {
        reject_outside_namespace(path)?;
        let target = format!("{path}@{snapshot}");
        self.run(&self.zfs, &["destroy", &target]).await?;
        Ok(())
    }

    async fn snapshots(&self, path: &str) -> Result<Vec<crate::model::SnapshotInfo>> {
        reject_outside_namespace(path)?;
        let stdout = self
            .run(
                &self.zfs,
                &[
                    "list",
                    "-H",
                    "-p",
                    "-t",
                    "snapshot",
                    "-o",
                    "name,used,creation",
                    "-s",
                    "creation",
                    path,
                ],
            )
            .await?;
        Ok(parse_snapshots(&stdout))
    }
}

/// `zfs list -H -p -t snapshot -o name,used,creation`: tab-separated, one
/// snapshot per line, the name carrying the dataset before the `@`.
fn parse_snapshots(stdout: &str) -> Vec<crate::model::SnapshotInfo> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let full = fields.next()?;
            let name = full.split_once('@')?.1.to_string();
            let used = fields.next()?.trim().parse().unwrap_or(0);
            let created = fields.next()?.trim().parse().unwrap_or(0);
            Some(crate::model::SnapshotInfo {
                name,
                used,
                created,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Compression, VdevKind};

    /// Real `zpool list -H -p` output, tab separated.
    #[test]
    fn pools_parse_from_the_machine_readable_form() {
        let stdout = "boot\t1000204886016\t4194304\t1000200691712\t0\t1.00\tONLINE\toff\n\
                      tank\t2000409772032\t1000204886016\t1000204886016\t12\t1.24x\tDEGRADED\ton\n";
        let pools = parse_pools(stdout);
        assert_eq!(pools.len(), 2);

        assert_eq!(pools[0].name, "boot");
        assert_eq!(pools[0].size, 1_000_204_886_016);
        assert_eq!(pools[0].allocated, 4_194_304);
        assert_eq!(pools[0].free, 1_000_200_691_712);
        assert_eq!(pools[0].fragmentation, Some(0));
        assert_eq!(pools[0].dedup_ratio, Some(1.0));
        assert_eq!(pools[0].health, PoolHealth::Online);
        assert!(!pools[0].read_only);

        // The trailing "x" some releases print on the ratio is not a parse
        // failure, and neither is a health word we do not have an arm for.
        assert_eq!(pools[1].dedup_ratio, Some(1.24));
        assert_eq!(pools[1].health, PoolHealth::Degraded);
        assert!(pools[1].read_only);
        assert_eq!(pools[1].used_percent(), 50);
    }

    #[test]
    fn a_column_with_nothing_in_it_reads_as_absent_not_as_zero() {
        let stdout = "boot\t100\t10\t90\t-\t-\tONLINE\toff\n";
        let pools = parse_pools(stdout);
        assert_eq!(pools[0].fragmentation, None);
        assert_eq!(pools[0].dedup_ratio, None);
    }

    #[test]
    fn datasets_and_volumes_parse_from_one_listing() {
        let stdout = "boot\tfilesystem\t4194304\t1000200691712\t98304\t-\t-\t/boot\n\
                      boot/lumen\tfilesystem\t98304\t1000200691712\t98304\t-\t-\tnone\n\
                      boot/lumen/vm-101-disk-0\tvolume\t34359738368\t1000200691712\t56\t34359738368\t16384\t-\n";
        let datasets = parse_datasets(stdout);
        assert_eq!(datasets.len(), 3);

        assert_eq!(datasets[0].kind, DatasetKind::Filesystem);
        assert_eq!(datasets[0].mountpoint.as_deref(), Some("/boot"));
        // "none" is not a mount point, it is the absence of one.
        assert_eq!(datasets[1].mountpoint, None);

        let volume = &datasets[2];
        assert_eq!(volume.kind, DatasetKind::Volume);
        assert_eq!(volume.volsize, Some(34_359_738_368));
        assert_eq!(volume.volblocksize, Some(16_384));
        assert_eq!(volume.mountpoint, None);
        assert!(volume.is_lumen_managed());
        assert_eq!(volume.pool(), "boot");
    }

    #[test]
    fn empty_output_is_a_node_with_no_pools_not_an_error() {
        assert!(parse_pools("").is_empty());
        assert!(parse_datasets("\n").is_empty());
    }

    fn pool_request(vdev: VdevKind, disks: &[&str]) -> PoolRequest {
        PoolRequest {
            name: "tank".into(),
            vdev,
            disks: disks.iter().map(|d| d.to_string()).collect(),
            ashift: None,
            compression: Compression::Lz4,
            autotrim: true,
            force: false,
        }
    }

    fn backend() -> CliBackend {
        CliBackend::new(lumen_sys::exec::MockExec::working())
    }

    /// The one command in this tree where a wrong argument destroys data, so
    /// it is read rather than run.
    #[test]
    fn a_pool_is_created_with_the_arrangement_that_was_asked_for() {
        let argv = backend().create_argv(&pool_request(
            VdevKind::Raidz2,
            &[
                "/dev/disk/by-id/a",
                "/dev/disk/by-id/b",
                "/dev/disk/by-id/c",
                "/dev/disk/by-id/d",
            ],
        ));

        // The keyword comes after the pool name and before the disks; getting
        // that order wrong builds a stripe and says nothing.
        let name = argv.iter().position(|a| a == "tank").unwrap();
        let keyword = argv.iter().position(|a| a == "raidz2").unwrap();
        let first_disk = argv.iter().position(|a| a == "/dev/disk/by-id/a").unwrap();
        assert!(name < keyword && keyword < first_disk, "{argv:?}");
        assert_eq!(
            &argv[first_disk..],
            [
                "/dev/disk/by-id/a",
                "/dev/disk/by-id/b",
                "/dev/disk/by-id/c",
                "/dev/disk/by-id/d"
            ]
        );

        // 4 KiB sectors unless told otherwise: the value cannot be changed
        // after creation, and a pool built at ashift=9 is slow forever.
        assert!(argv.contains(&"ashift=12".to_string()));
        assert!(argv.contains(&"compression=lz4".to_string()));
        // The pool root must never mount at /<name> — a pool called "boot"
        // appearing over /boot is exactly the accident this avoids.
        assert!(argv.contains(&"mountpoint=none".to_string()));
        assert!(argv.contains(&"canmount=off".to_string()));
        // Nothing is forced unless the operator acknowledged it.
        assert!(!argv.contains(&"-f".to_string()));
    }

    /// A stripe has no keyword — bare disks *are* the stripe — so the command
    /// for one must not gain a stray word.
    #[test]
    fn a_stripe_is_bare_disks_and_nothing_else() {
        let argv =
            backend().create_argv(&pool_request(VdevKind::Stripe, &["/dev/sda", "/dev/sdb"]));
        let name = argv.iter().position(|a| a == "tank").unwrap();
        assert_eq!(&argv[name + 1..], ["/dev/sda", "/dev/sdb"]);
        for keyword in ["mirror", "raidz1", "raidz2", "raidz3"] {
            assert!(!argv.contains(&keyword.to_string()), "{argv:?}");
        }
    }

    #[test]
    fn forcing_is_only_ever_what_the_caller_asked_for() {
        let mut request = pool_request(VdevKind::Mirror, &["/dev/sda", "/dev/sdb"]);
        request.force = true;
        let argv = backend().create_argv(&request);
        assert_eq!(argv[0], "create");
        assert_eq!(argv[1], "-f", "-f belongs immediately after the verb");
    }

    /// The backend checks again even though the service already did.
    #[tokio::test]
    async fn a_request_that_did_not_come_from_the_picker_is_refused_here_too() {
        let backend = backend();

        let mut named = pool_request(VdevKind::Stripe, &["/dev/sda"]);
        named.name = "mirror".into();
        assert!(backend.create_pool(&named).await.is_err(), "a vdev keyword");

        let mut escaped = pool_request(VdevKind::Stripe, &["/etc/passwd"]);
        escaped.name = "tank".into();
        assert!(
            backend.create_pool(&escaped).await.is_err(),
            "not under /dev"
        );

        let traversal = pool_request(VdevKind::Stripe, &["/dev/../etc/passwd"]);
        assert!(backend.create_pool(&traversal).await.is_err());

        let flag = pool_request(VdevKind::Stripe, &["/dev/-rf"]);
        assert!(backend.create_pool(&flag).await.is_err());

        let empty = pool_request(VdevKind::Stripe, &[]);
        assert!(backend.create_pool(&empty).await.is_err());
    }

    /// The two guards, checked at the backend boundary as well as in the
    /// service — the destroy path is the one place a mistake is unrecoverable.
    #[test]
    fn the_backend_refuses_paths_and_pools_it_should_not_touch() {
        assert!(reject_outside_namespace("boot/lumen/vm-101-disk-0").is_ok());
        assert!(reject_outside_namespace("boot/data/important").is_err());
        assert!(reject_outside_namespace("boot").is_err());
        // Shaped like a volume, but it is the media library.
        assert!(reject_outside_namespace("boot/lumen/iso").is_err());
        assert!(reject_bad_pool("boot").is_ok());
        assert!(reject_bad_pool("boot/lumen").is_err());
        assert!(reject_bad_pool("-rf").is_err());
    }

    /// `zpool list -vHP` on a node with a mirrored root pool and a raidz data
    /// pool with a log device — the shapes the parser has to walk past to find
    /// the disks.
    const ZPOOL_LIST_VERBOSE: &str = "\
boot\t928G\t8.1G\t920G\t-\t-\t0%\t0%\t1.00x\tONLINE\t-
\tmirror-0\t928G\t8.1G\t920G\t-\t-\t0%\t0%\t-\t-\t-
\t  /dev/disk/by-id/nvme-Samsung_SSD_983_A-part3\t930G\t-\t-\t-\t-\t-\t-\t-\tONLINE\t-
\t  /dev/disk/by-id/nvme-Samsung_SSD_983_B-part3\t930G\t-\t-\t-\t-\t-\t-\t-\tONLINE\t-
tank\t3.6T\t1.2T\t2.4T\t-\t-\t2%\t33%\t1.00x\tONLINE\t-
\traidz1-0\t3.6T\t1.2T\t2.4T\t-\t-\t2%\t33%\t-\t-\t-
\t  /dev/disk/by-id/nvme-INTEL_A\t932G\t-\t-\t-\t-\t-\t-\t-\tONLINE\t-
\t  /dev/disk/by-id/nvme-INTEL_B\t932G\t-\t-\t-\t-\t-\t-\t-\tONLINE\t-
\t  /dev/disk/by-id/nvme-INTEL_C\t932G\t-\t-\t-\t-\t-\t-\t-\tONLINE\t-
\tlogs\t-\t-\t-\t-\t-\t-\t-\t-\t-\t-
\t  /dev/nvme4n1p1\t100G\t-\t-\t-\t-\t-\t-\t-\tONLINE\t-
";

    /// Only the devices, attributed to the pool whose subtree they are in.
    ///
    /// The container vdevs are the trap: `mirror-0`, `raidz1-0`, and `logs`
    /// are indented exactly as the disks are, and the only thing telling them
    /// apart is that `-P` prints a whole path for a real device.
    #[test]
    fn the_verbose_pool_listing_yields_every_device_and_the_pool_that_has_it() {
        let members = parse_pool_members(ZPOOL_LIST_VERBOSE);
        let pool_of = |path: &str| {
            members
                .iter()
                .find(|(candidate, _)| candidate == path)
                .map(|(_, pool)| pool.as_str())
        };

        assert_eq!(members.len(), 6, "{members:?}");
        assert_eq!(
            pool_of("/dev/disk/by-id/nvme-Samsung_SSD_983_A-part3"),
            Some("boot")
        );
        assert_eq!(pool_of("/dev/disk/by-id/nvme-INTEL_C"), Some("tank"));
        // A log device is a pool member too. It carries labels, and clearing
        // it out from under a running pool is exactly what this list prevents.
        assert_eq!(pool_of("/dev/nvme4n1p1"), Some("tank"));

        // No container vdev made it in.
        assert!(
            !members
                .iter()
                .any(|(path, _)| path == "mirror-0" || path == "raidz1-0" || path == "logs"),
            "{members:?}"
        );
    }

    /// A node with no pools answers with no members — not with an error, and
    /// not with a phantom entry that would make every disk look spoken for.
    #[test]
    fn a_node_with_no_pools_has_no_members() {
        assert!(parse_pool_members("").is_empty());
        assert!(parse_pool_members("\n\n").is_empty());
    }
}
