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
//! Nothing here needs the control plane's unit relaxed: dataset and volume
//! operations reach the kernel through `/dev/zfs`, which `ProtectSystem=strict`
//! does not cover. See docs/compute.md for the reproduction.

use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use crate::backend::ZfsBackend;
use crate::error::{Result, ZfsError};
use crate::model::{
    is_lumen_volume, is_reserved_leaf, iso_dataset, iso_mountpoint, lumen_root, valid_pool_name,
    Dataset, DatasetKind, Pool, PoolHealth, VolumeRequest,
};

/// Columns asked for by name, so a release that adds one does not shift the
/// fields out from under the parser.
const POOL_COLUMNS: &str = "name,size,alloc,free,frag,dedup,health,readonly";
const DATASET_COLUMNS: &str = "name,type,used,avail,refer,volsize,volblocksize,mountpoint";

pub struct CliBackend {
    zfs: String,
    zpool: String,
}

impl Default for CliBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CliBackend {
    pub fn new() -> Self {
        Self {
            zfs: "zfs".into(),
            zpool: "zpool".into(),
        }
    }

    /// Confirm the tools are actually there, so a node without storage swaps
    /// in `UnavailableBackend` at startup instead of failing every request
    /// with a spawn error.
    pub async fn probe() -> Result<Self> {
        let backend = Self::new();
        backend
            .run(&backend.zpool.clone(), &["list", "-H", "-o", "name"])
            .await?;
        Ok(backend)
    }

    /// One invocation. Output is captured; a non-zero exit becomes an error
    /// carrying what the tool actually said, because "storage command failed"
    /// helps nobody.
    async fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        let output = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|err| {
                ZfsError::Backend(anyhow::Error::from(err).context(format!(
                    "running {program} {} — is the storage software installed?",
                    args.join(" ")
                )))
            })?;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `zpool list -H -p` output, tab separated.
    #[test]
    fn pools_parse_from_the_machine_readable_form() {
        let stdout = "rpool\t1000204886016\t4194304\t1000200691712\t0\t1.00\tONLINE\toff\n\
                      tank\t2000409772032\t1000204886016\t1000204886016\t12\t1.24x\tDEGRADED\ton\n";
        let pools = parse_pools(stdout);
        assert_eq!(pools.len(), 2);

        assert_eq!(pools[0].name, "rpool");
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
        let stdout = "rpool\t100\t10\t90\t-\t-\tONLINE\toff\n";
        let pools = parse_pools(stdout);
        assert_eq!(pools[0].fragmentation, None);
        assert_eq!(pools[0].dedup_ratio, None);
    }

    #[test]
    fn datasets_and_volumes_parse_from_one_listing() {
        let stdout = "rpool\tfilesystem\t4194304\t1000200691712\t98304\t-\t-\t/rpool\n\
                      rpool/lumen\tfilesystem\t98304\t1000200691712\t98304\t-\t-\tnone\n\
                      rpool/lumen/vm-101-disk-0\tvolume\t34359738368\t1000200691712\t56\t34359738368\t16384\t-\n";
        let datasets = parse_datasets(stdout);
        assert_eq!(datasets.len(), 3);

        assert_eq!(datasets[0].kind, DatasetKind::Filesystem);
        assert_eq!(datasets[0].mountpoint.as_deref(), Some("/rpool"));
        // "none" is not a mount point, it is the absence of one.
        assert_eq!(datasets[1].mountpoint, None);

        let volume = &datasets[2];
        assert_eq!(volume.kind, DatasetKind::Volume);
        assert_eq!(volume.volsize, Some(34_359_738_368));
        assert_eq!(volume.volblocksize, Some(16_384));
        assert_eq!(volume.mountpoint, None);
        assert!(volume.is_lumen_managed());
        assert_eq!(volume.pool(), "rpool");
    }

    #[test]
    fn empty_output_is_a_node_with_no_pools_not_an_error() {
        assert!(parse_pools("").is_empty());
        assert!(parse_datasets("\n").is_empty());
    }

    /// The two guards, checked at the backend boundary as well as in the
    /// service — the destroy path is the one place a mistake is unrecoverable.
    #[test]
    fn the_backend_refuses_paths_and_pools_it_should_not_touch() {
        assert!(reject_outside_namespace("rpool/lumen/vm-101-disk-0").is_ok());
        assert!(reject_outside_namespace("rpool/data/important").is_err());
        assert!(reject_outside_namespace("rpool").is_err());
        // Shaped like a volume, but it is the media library.
        assert!(reject_outside_namespace("rpool/lumen/iso").is_err());
        assert!(reject_bad_pool("rpool").is_ok());
        assert!(reject_bad_pool("rpool/lumen").is_err());
        assert!(reject_bad_pool("-rf").is_err());
    }
}
