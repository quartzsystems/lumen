//! The supported command line: `drbdsetup status --json` for state, and
//! `drbdadm` handed to systemd as transient units for everything that
//! changes the node.
//!
//! The resource file is the one write with a secret in it, so it travels as
//! the standard input of `install -m 0600 /dev/stdin` — content over the
//! pipe, mode and target as typed arguments, no shell, and nothing secret in
//! argv. The same rule as the corosync key and the BMC password.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lumen_sys::exec::{Exec, Request as ExecRequest};
use tokio::time::timeout;

use super::DrbdBackend;
use crate::error::{DrbdError, Result};
use crate::state::{parse_status, ResourceStatus};

/// Status answers in milliseconds on a healthy node; a hung DRBD must not
/// hang the storage view with it. Same reasoning as the cluster domain's.
const DEADLINE: Duration = Duration::from_secs(30);

const DRBD_CONF_DIR: &str = "/etc/drbd.d";
const DRBDADM: &str = "/usr/sbin/drbdadm";
const INSTALL: &str = "/usr/bin/install";
const RM: &str = "/usr/bin/rm";

pub struct CliBackend {
    drbdsetup: String,
    exec: Arc<dyn Exec>,
}

impl CliBackend {
    pub fn new(exec: Arc<dyn Exec>) -> Self {
        CliBackend {
            drbdsetup: "/usr/sbin/drbdsetup".into(),
            exec,
        }
    }

    fn resource_path(resource: &str) -> String {
        format!("{DRBD_CONF_DIR}/{resource}.res")
    }

    async fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = timeout(DEADLINE, command.output()).await.map_err(|_| {
            DrbdError::backend(anyhow::anyhow!(
                "{program} did not answer within {} seconds — DRBD on this node may have hung.",
                DEADLINE.as_secs()
            ))
        })?;
        let output = output
            .map_err(|err| DrbdError::backend(anyhow::anyhow!("could not run {program}: {err}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DrbdError::backend(anyhow::anyhow!(
                "{program} failed: {}",
                stderr.lines().last().unwrap_or("no output").trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn run_privileged(&self, description: String, request: ExecRequest) -> Result<()> {
        let outcome = self.exec.run(request).await.map_err(DrbdError::Backend)?;
        if !outcome.ok() {
            return Err(DrbdError::Conflict(format!(
                "{description}: {}",
                outcome.failure()
            )));
        }
        Ok(())
    }

    async fn drbdadm(&self, what: &str, failed: &str, args: &[&str]) -> Result<()> {
        self.run_privileged(
            failed.to_string(),
            ExecRequest::new(what, DRBDADM).args(args.to_vec()),
        )
        .await
    }
}

#[async_trait]
impl DrbdBackend for CliBackend {
    async fn status(&self) -> Result<Vec<ResourceStatus>> {
        let json = self.run(&self.drbdsetup, &["status", "--json"]).await?;
        parse_status(&json)
    }

    async fn write_resource(&self, resource: &str, content: &str) -> Result<()> {
        let path = Self::resource_path(resource);
        self.run_privileged(
            format!("writing the resource file for {resource} failed"),
            ExecRequest::new("write a replicated volume's resource file", INSTALL)
                .args(["-D", "-m", "0600", "/dev/stdin", &path])
                .stdin(content),
        )
        .await
    }

    async fn remove_resource_file(&self, resource: &str) -> Result<()> {
        let path = Self::resource_path(resource);
        self.run_privileged(
            format!("removing the resource file for {resource} failed"),
            ExecRequest::new("remove a replicated volume's resource file", RM).args(["-f", &path]),
        )
        .await
    }

    async fn create_metadata(&self, resource: &str, peers: usize) -> Result<()> {
        let max_peers = format!("--max-peers={peers}");
        self.drbdadm(
            "create replication metadata",
            &format!("creating metadata for {resource} failed"),
            &["create-md", "--force", &max_peers, resource],
        )
        .await
    }

    async fn up(&self, resource: &str) -> Result<()> {
        self.drbdadm(
            "bring a replicated volume up",
            &format!("bringing {resource} up failed"),
            &["up", resource],
        )
        .await
    }

    async fn down(&self, resource: &str) -> Result<()> {
        self.drbdadm(
            "take a replicated volume down",
            &format!("taking {resource} down failed"),
            &["down", resource],
        )
        .await
    }

    async fn skip_initial_sync(&self, resource: &str) -> Result<()> {
        // Fresh zvols read as zeros on every member, so there is nothing to
        // copy: declare the current (empty) state the one true state instead
        // of replicating terabytes of zeros at creation.
        let target = format!("{resource}/0");
        self.drbdadm(
            "skip the initial sync of a fresh volume",
            &format!("declaring {resource} in sync failed"),
            &["new-current-uuid", "--clear-bitmap", &target],
        )
        .await
    }

    async fn resize(&self, resource: &str) -> Result<()> {
        self.drbdadm(
            "grow a replicated volume",
            &format!("growing {resource} failed"),
            &["resize", resource],
        )
        .await
    }

    async fn set_two_primaries(&self, resource: &str, allow: bool) -> Result<()> {
        let option = if allow {
            "--allow-two-primaries=yes"
        } else {
            "--allow-two-primaries=no"
        };
        self.drbdadm(
            "adjust a volume's two-primaries window",
            &format!("adjusting the two-primaries window of {resource} failed"),
            &["net-options", option, resource],
        )
        .await
    }

    async fn invalidate_remote(&self, resource: &str) -> Result<()> {
        self.drbdadm(
            "declare the peers of a volume out of date",
            &format!("invalidating the peers of {resource} failed"),
            &["invalidate-remote", resource],
        )
        .await
    }

    async fn read_resource(&self, resource: &str) -> Result<String> {
        let path = Self::resource_path(resource);
        tokio::fs::read_to_string(&path)
            .await
            .map_err(|err| DrbdError::Conflict(format!("{path} is not readable ({err}).")))
    }

    async fn adjust(&self, resource: &str) -> Result<()> {
        self.drbdadm(
            "apply a volume's configuration live",
            &format!("adjusting {resource} failed"),
            &["adjust", resource],
        )
        .await
    }

    async fn reconnect(&self, resource: &str, discard: bool) -> Result<()> {
        // A StandAlone side may refuse the disconnect ("not connected") —
        // that is the state we are here to leave, not an error.
        let _ = self
            .drbdadm(
                "disconnect a volume before reconnecting",
                &format!("disconnecting {resource} failed"),
                &["disconnect", resource],
            )
            .await;
        if discard {
            self.drbdadm(
                "reconnect a volume, discarding this node's writes",
                &format!("reconnecting {resource} (discarding) failed"),
                &["connect", "--discard-my-data", resource],
            )
            .await
        } else {
            self.drbdadm(
                "reconnect a volume",
                &format!("reconnecting {resource} failed"),
                &["connect", resource],
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The resource file carries the replication secret, so it rides stdin
    /// with a locked-down mode — never an argument.
    #[tokio::test]
    async fn the_resource_file_rides_stdin_with_a_locked_mode() {
        let exec = lumen_sys::exec::MockExec::working();
        let backend = CliBackend::new(exec.clone());
        backend
            .write_resource(
                "alpha-v0",
                "resource \"alpha-v0\" { shared-secret \"s3cret\"; }",
            )
            .await
            .unwrap();

        let ran = exec.ran().await;
        assert_eq!(ran[0].program, INSTALL);
        assert_eq!(
            ran[0].args,
            vec!["-D", "-m", "0600", "/dev/stdin", "/etc/drbd.d/alpha-v0.res"]
        );
        assert!(ran[0].stdin.as_deref().unwrap().contains("s3cret"));
        assert!(!ran[0].args.iter().any(|a| a.contains("s3cret")));
    }

    #[tokio::test]
    async fn the_lifecycle_verbs_map_to_drbdadm_one_to_one() {
        let exec = lumen_sys::exec::MockExec::working();
        let backend = CliBackend::new(exec.clone());
        backend.create_metadata("alpha-v0", 1).await.unwrap();
        backend.up("alpha-v0").await.unwrap();
        backend.skip_initial_sync("alpha-v0").await.unwrap();
        backend.resize("alpha-v0").await.unwrap();
        backend.down("alpha-v0").await.unwrap();
        backend.remove_resource_file("alpha-v0").await.unwrap();

        assert!(
            exec.ran_with(
                DRBDADM,
                &["create-md", "--force", "--max-peers=1", "alpha-v0"]
            )
            .await
        );
        assert!(exec.ran_with(DRBDADM, &["up", "alpha-v0"]).await);
        assert!(
            exec.ran_with(
                DRBDADM,
                &["new-current-uuid", "--clear-bitmap", "alpha-v0/0"]
            )
            .await
        );
        assert!(exec.ran_with(DRBDADM, &["resize", "alpha-v0"]).await);
        assert!(exec.ran_with(DRBDADM, &["down", "alpha-v0"]).await);
        assert!(exec.ran_with(RM, &["-f", "/etc/drbd.d/alpha-v0.res"]).await);
    }

    #[tokio::test]
    async fn a_failed_transient_unit_reports_the_tools_own_sentence() {
        let exec = lumen_sys::exec::MockExec::working();
        exec.fail_next(1, "'alpha-v0' not defined in your config")
            .await;
        let backend = CliBackend::new(exec);
        let err = backend.up("alpha-v0").await.unwrap_err();
        assert!(err.to_string().contains("not defined"), "{err}");
    }
}
