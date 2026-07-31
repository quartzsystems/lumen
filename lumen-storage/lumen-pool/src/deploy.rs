//! The privileged acts a pool deployment is made of, behind the house exec
//! seam.
//!
//! `lumen-controlplane` never runs a privileged command itself — every one
//! goes through a domain crate's backend holding `Arc<dyn Exec>`, so the
//! workflows test against `MockExec` asserting exact argv. This module is
//! that backend for the pool: formatting a brick, writing and removing the
//! daemon's drop-in, starting and stopping the unit, wiping a brick, and
//! restarting the control plane that must re-read it all.
//!
//! Nothing here decides anything. Which disks, which tiers, which member
//! is told what — that is the workflow's business; these are the hands.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lumen_sys::exec::{Exec, Request as ExecRequest};

use crate::config::{PoolConfig, FSD_CONF};

const LUMEN_FSD: &str = "/usr/sbin/lumen-fsd";
const INSTALL: &str = "/usr/bin/install";
const RM: &str = "/usr/bin/rm";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
const WIPEFS: &str = "/usr/sbin/wipefs";
const DD: &str = "/usr/bin/dd";

/// Formatting reads and writes a whole disk's opening sectors and zeroes
/// its anchor slots; a shelf of disks takes its time.
const FORMAT_DEADLINE: Duration = Duration::from_secs(300);

pub struct PoolDeploy {
    exec: Arc<dyn Exec>,
}

/// One brick to format, as the workflow planned it.
#[derive(Debug, Clone)]
pub struct BrickFormat {
    /// The stable device path — `/dev/disk/by-id/…`.
    pub path: String,
    pub tier: u8,
    pub wal_holder: bool,
    /// The identity the workflow minted for it, lowercase hex — minted
    /// up front so the holder's roster is known without parsing stdout.
    pub brick_uuid: String,
}

impl PoolDeploy {
    pub fn new(exec: Arc<dyn Exec>) -> PoolDeploy {
        PoolDeploy { exec }
    }

    async fn run(&self, description: &str, request: ExecRequest) -> Result<(), String> {
        let outcome = self
            .exec
            .run(request)
            .await
            .map_err(|err| format!("{description}: {err}"))?;
        if !outcome.ok() {
            return Err(format!("{description}: {}", outcome.failure()));
        }
        Ok(())
    }

    /// Format one brick with the daemon's own `format` subcommand. The
    /// holder formats **last**, its `--roster` naming every other brick —
    /// the ordering that keeps a crash from leaving an anchor naming
    /// bricks that do not exist. `roster` is exactly those others, as
    /// `uuid:tier` pairs; empty for every non-holder.
    pub async fn format_brick(
        &self,
        brick: &BrickFormat,
        pool_uuid: &str,
        roster: &[(String, u8)],
    ) -> Result<(), String> {
        let mut request = ExecRequest::new("format a disk as a LumenFS brick", LUMEN_FSD)
            .args(["format", &brick.path, "--tier", &brick.tier.to_string()])
            .args(["--pool-uuid", pool_uuid])
            .args(["--brick-uuid", &brick.brick_uuid]);
        if brick.wal_holder {
            request = request.arg("--wal");
            for (uuid, tier) in roster {
                request = request.args(["--roster", &format!("{uuid}:{tier}")]);
            }
        }
        self.run(
            &format!("formatting {} failed", brick.path),
            with_deadline(request, FORMAT_DEADLINE),
        )
        .await
    }

    /// Write the daemon's drop-in. Content over the pipe, mode and target
    /// as typed arguments, no shell — the same rule every configuration
    /// write in the appliance follows.
    pub async fn write_conf(&self, config: &PoolConfig) -> Result<(), String> {
        let content = config.render().map_err(|err| err.to_string())?;
        self.run(
            "writing the pool daemon's drop-in failed",
            ExecRequest::new("write the pool daemon's drop-in", INSTALL)
                .args(["-D", "-m", "0600", "/dev/stdin", FSD_CONF])
                .stdin(content),
        )
        .await
    }

    /// Remove the drop-in — after this, the node carries no pool.
    pub async fn remove_conf(&self) -> Result<(), String> {
        self.run(
            "removing the pool daemon's drop-in failed",
            ExecRequest::new("remove the pool daemon's drop-in", RM).args(["-f", FSD_CONF]),
        )
        .await
    }

    /// Enable and start the daemon — the preset ships it disabled until a
    /// drop-in exists, so enabling is the workflow's act.
    pub async fn enable_daemon(&self) -> Result<(), String> {
        self.run(
            "starting the pool daemon failed",
            ExecRequest::new("enable and start the pool daemon", SYSTEMCTL).args([
                "enable",
                "--now",
                "lumen-fsd",
            ]),
        )
        .await
    }

    /// Restart it — the grow workflow's way of handing a member its new
    /// conf: the daemon reopens the same bricks under the new peer set,
    /// says hello, and resyncs whatever the restart window missed.
    pub async fn restart_daemon(&self) -> Result<(), String> {
        self.run(
            "restarting the pool daemon failed",
            ExecRequest::new("restart the pool daemon", SYSTEMCTL).args(["restart", "lumen-fsd"]),
        )
        .await
    }

    /// Stop and disable it — teardown's first act, so nothing is serving
    /// from the bricks about to be wiped.
    pub async fn disable_daemon(&self) -> Result<(), String> {
        self.run(
            "stopping the pool daemon failed",
            ExecRequest::new("stop and disable the pool daemon", SYSTEMCTL).args([
                "disable",
                "--now",
                "lumen-fsd",
            ]),
        )
        .await
    }

    /// Wipe a brick's identity. Deliberately **not** the storage domain's
    /// guarded wipe: the scanner reports a brick as claimed (which is what
    /// keeps the pickers honest), so the guarded path would refuse the one
    /// wipe the destroy workflow is *for*. The guard here is the workflow
    /// above — daemon stopped first, vdisks checked first.
    ///
    /// Zeroing the opening sectors is the real act — found on hardware:
    /// `wipefs` erases only signatures it recognizes, and a LumenFS
    /// superblock is not one of them, so a wipefs-only wipe left the disk
    /// still claiming to be a brick. The first sixteen KiB carry both
    /// superblock slots and both anchor slots; zeros there are what "not a
    /// brick" durably means. `wipefs -a` still runs after, for anything
    /// else a disk may once have been.
    pub async fn wipe_brick(&self, path: &str) -> Result<(), String> {
        self.run(
            &format!("zeroing {path}'s brick identity failed"),
            ExecRequest::new("zero a retired brick's identity sectors", DD).args([
                "if=/dev/zero",
                &format!("of={path}"),
                "bs=4096",
                "count=4",
                "conv=fsync",
            ]),
        )
        .await?;
        self.run(
            &format!("wiping {path} failed"),
            ExecRequest::new("wipe a retired brick's other signatures", WIPEFS).args(["-a", path]),
        )
        .await
    }

    /// Restart this node's control plane, detached: the transient unit
    /// survives the process it restarts, which is what lets a handler
    /// reply first and restart after.
    pub async fn restart_controlplane(&self) -> Result<(), String> {
        self.run(
            "restarting the control plane failed",
            ExecRequest::new("restart the control plane to adopt the pool", SYSTEMCTL).args([
                "restart",
                "--no-block",
                "lumen-controlplane",
            ]),
        )
        .await
    }
}

fn with_deadline(request: ExecRequest, _deadline: Duration) -> ExecRequest {
    // The exec seam's own deadline is fixed per runner; the constant above
    // documents the budget formatting is entitled to, and the runner in
    // main.rs is constructed with it. Kept as a seam so a per-request
    // deadline can arrive without changing callers.
    request
}

/// Mint a brick identity in the impure shell — the engine itself owns no
/// randomness. Same recipe as the daemon's own `format` default: wall
/// clock and pid, hashed.
pub fn mint_brick_uuid(salt: &str) -> String {
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = lumen_fs::hash_block(
        format!("lumen-pool brick {salt} {clock} {}", std::process::id()).as_bytes(),
    );
    seed.as_bytes()[0..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Mint a pool identity — the coordinator's act, once per pool.
pub fn mint_pool_uuid() -> String {
    mint_brick_uuid("pool")
}

/// Whether the ublk control device this pool's exports need is present —
/// the preflight's honest, cheap answer. The module is in-tree in the
/// appliance kernel; its absence means a kernel this pool cannot serve
/// guests from.
pub fn ublk_available() -> bool {
    Path::new("/dev/ublk-control").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_sys::exec::MockExec;
    use std::path::PathBuf;

    fn deploy() -> (Arc<MockExec>, PoolDeploy) {
        let exec = MockExec::working();
        (exec.clone(), PoolDeploy::new(exec))
    }

    /// The exact argv the appliance will run, pinned — a formatting
    /// command that drifts from the daemon's grammar fails on the first
    /// real node, which is the wrong place to find out.
    #[tokio::test]
    async fn a_non_holder_formats_with_its_tier_and_identity_only() {
        let (exec, deploy) = deploy();
        deploy
            .format_brick(
                &BrickFormat {
                    path: "/dev/disk/by-id/ata-slow.0002".into(),
                    tier: 1,
                    wal_holder: false,
                    brick_uuid: "bb".repeat(16),
                },
                &"aa".repeat(16),
                &[],
            )
            .await
            .unwrap();
        let ran = exec.ran().await;
        assert_eq!(ran[0].program, LUMEN_FSD);
        assert_eq!(
            ran[0].args,
            vec![
                "format",
                "/dev/disk/by-id/ata-slow.0002",
                "--tier",
                "1",
                "--pool-uuid",
                &"aa".repeat(16),
                "--brick-uuid",
                &"bb".repeat(16),
            ]
        );
    }

    #[tokio::test]
    async fn the_holder_formats_with_the_wal_and_the_whole_roster() {
        let (exec, deploy) = deploy();
        deploy
            .format_brick(
                &BrickFormat {
                    path: "/dev/disk/by-id/nvme-eui.0001".into(),
                    tier: 0,
                    wal_holder: true,
                    brick_uuid: "cc".repeat(16),
                },
                &"aa".repeat(16),
                &[("bb".repeat(16), 1)],
            )
            .await
            .unwrap();
        let ran = exec.ran().await;
        assert!(ran[0].args.contains(&"--wal".to_string()));
        assert!(ran[0]
            .args
            .windows(2)
            .any(|w| w[0] == "--roster" && w[1] == format!("{}:1", "bb".repeat(16))));
    }

    /// The drop-in rides stdin with a locked mode, never argv — the same
    /// rule as every configuration write in the appliance.
    #[tokio::test]
    async fn the_drop_in_rides_stdin_with_a_locked_mode() {
        let (exec, deploy) = deploy();
        let config = PoolConfig {
            bricks: vec![PathBuf::from("/dev/disk/by-id/nvme-eui.0001")],
            node: 0,
            peers: vec![crate::config::PeerRole::Listen(
                "10.10.0.1:7800".parse().unwrap(),
            )],
            members: vec![0, 1],
            control: crate::config::DEFAULT_CONTROL.parse().unwrap(),
        };
        deploy.write_conf(&config).await.unwrap();
        let ran = exec.ran().await;
        assert_eq!(ran[0].program, INSTALL);
        assert_eq!(
            ran[0].args,
            vec!["-D", "-m", "0600", "/dev/stdin", FSD_CONF]
        );
        let stdin = ran[0].stdin.as_deref().unwrap();
        assert!(stdin.contains("LUMEN_FSD_BRICK=/dev/disk/by-id/nvme-eui.0001"));
        assert!(stdin.contains("LUMEN_FSD_PEER=--listen 10.10.0.1:7800 --members 0,1"));
    }

    #[tokio::test]
    async fn teardown_verbs_run_the_exact_commands() {
        let (exec, deploy) = deploy();
        deploy.disable_daemon().await.unwrap();
        deploy.remove_conf().await.unwrap();
        deploy
            .wipe_brick("/dev/disk/by-id/nvme-eui.0001")
            .await
            .unwrap();
        deploy.enable_daemon().await.unwrap();
        deploy.restart_controlplane().await.unwrap();
        assert!(
            exec.ran_with(SYSTEMCTL, &["disable", "--now", "lumen-fsd"])
                .await
        );
        assert!(exec.ran_with(RM, &["-f", FSD_CONF]).await);
        // The zeroing is the real wipe — wipefs alone cannot see a
        // LumenFS superblock; the hardware proved it.
        assert!(
            exec.ran_with(
                DD,
                &[
                    "if=/dev/zero",
                    "of=/dev/disk/by-id/nvme-eui.0001",
                    "bs=4096",
                    "count=4",
                    "conv=fsync",
                ]
            )
            .await
        );
        assert!(
            exec.ran_with(WIPEFS, &["-a", "/dev/disk/by-id/nvme-eui.0001"])
                .await
        );
        assert!(
            exec.ran_with(SYSTEMCTL, &["enable", "--now", "lumen-fsd"])
                .await
        );
        assert!(
            exec.ran_with(SYSTEMCTL, &["restart", "--no-block", "lumen-controlplane"])
                .await
        );
    }

    #[tokio::test]
    async fn a_failed_command_carries_its_description_and_the_stderr() {
        let (exec, deploy) = deploy();
        exec.fail_next(1, "dd: error writing '/dev/sdz': No space left")
            .await;
        let err = deploy.wipe_brick("/dev/sdz").await.unwrap_err();
        assert!(err.contains("zeroing /dev/sdz"), "{err}");
        assert!(err.contains("No space left"), "{err}");
    }

    #[test]
    fn minted_identities_are_hex_and_distinct() {
        let a = mint_brick_uuid("a");
        let b = mint_brick_uuid("b");
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
