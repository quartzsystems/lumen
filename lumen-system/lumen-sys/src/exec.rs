//! Running a command that has to happen outside this daemon's sandbox.
//!
//! `lumen-controlplane.service` runs with `ProtectSystem=strict`, which makes
//! the whole file system hierarchy read-only apart from `/dev`, `/proc`,
//! `/sys`, and the directories the unit names. That is what makes
//! the daemon safe to expose on a network, and it is deliberately not relaxed:
//! docs/compute.md works through why machine and volume operations need no
//! relaxation at all.
//!
//! Two operations genuinely do:
//!
//! - **Creating a local account** writes `/etc/passwd`, `/etc/shadow`, and
//!   `/etc/group`, and `useradd` makes lock files next to them — so it needs
//!   `/etc` writable, not four individual files.
//! - **`zpool create`, `import`, `export`, and `destroy`** write
//!   `/etc/zfs/zpool.cache`. Every other storage operation reaches the kernel
//!   through `/dev/zfs` and needs nothing.
//!
//! ## Ask systemd to do it, rather than being allowed to do it
//!
//! `ReadWritePaths=/etc` would make both work in about four characters, and it
//! would also hand a network-facing daemon write access to `sudoers`, every
//! PAM stack, and every unit file on the box. That is a much larger thing to
//! give away than the two commands that need it.
//!
//! Instead the command is handed to **systemd**, which starts it as a
//! transient unit of its own — a child of PID 1, outside this daemon's
//! namespace, with none of this daemon's restrictions and none of its
//! privileges either. The sandbox stays exactly as it is.
//!
//! That is the same arrangement every other domain already has. Networking
//! does not configure interfaces; it asks NetworkManager. Machines are not
//! started by this daemon; the hypervisor starts them. In each case the
//! privileged work happens in another process with its own API, which is
//! precisely why none of the hardening has to move. `lumen-execd` was the
//! placeholder for a third such process — it turns out systemd already is one.
//!
//! ## `systemd-run`, not the bus
//!
//! `StartTransientUnit` over the system bus would avoid a process, at the cost
//! of hand-building an `a(sv)` property list, subscribing to `JobRemoved`,
//! reading `ExecMainStatus` back off the unit, and garbage-collecting it
//! afterwards — roughly two hundred lines to reimplement what `systemd-run
//! --wait --pipe --collect` already does correctly, including propagating the
//! exit status.
//!
//! This is the same trade `lumen-zfs` made against `libzfs`, and it carries
//! the same rule: **typed argument arrays, never an interpolated shell
//! string**. There is nothing to escape because there is no string to escape
//! into — every invocation is an array handed to `execve`.
//!
//! ## Secrets go over the pipe, never in the argument list
//!
//! A password on a command line is visible to every process on the box through
//! `/proc`, and systemd records a unit's command in the journal besides. So
//! `chpasswd` reads its input from standard input, which `--pipe` connects
//! straight through to the transient unit. Nothing in [`Request::args`] is ever
//! secret, and [`Request::stdin`] never reaches a log.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::timeout;

/// How long a privileged command may take before it is given up on.
///
/// Nothing delegated here legitimately takes minutes: `useradd` writes three
/// files, and `zpool create` labels its disks and returns. Without a bound, a
/// command that blocks — a disk that will not settle, a bus that took the job
/// and never answered — leaves the HTTP request that asked for it pending for
/// as long as the browser is willing to wait, and the console sits on
/// "Creating…" with no error, no progress, and nothing to do but reload the
/// page. A sentence after two minutes is worse than a fast answer and far
/// better than silence.
const DEADLINE: Duration = Duration::from_secs(120);

/// What ran, and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    /// The most useful sentence the command produced.
    ///
    /// `shadow-utils` and `zpool` both report failures on standard error, one
    /// line at a time, and the last line is the specific one — the earlier
    /// lines are usually context leading up to it. When there is nothing at
    /// all, the exit status is still something to report rather than a silent
    /// failure.
    pub fn failure(&self) -> String {
        let said = self
            .stderr
            .lines()
            .chain(self.stdout.lines())
            .map(str::trim)
            .rfind(|line| !line.is_empty());
        match said {
            Some(line) => line.to_string(),
            None => format!("the command failed with status {}", self.status),
        }
    }
}

/// One privileged command.
#[derive(Debug, Clone)]
pub struct Request {
    /// What this is for, in a few words. It becomes the transient unit's
    /// description, so it is what an operator sees in `systemctl` and in the
    /// journal — write it for them.
    pub description: String,
    /// The program, absolute. Resolved by `execve`, not by a shell, so it is
    /// not subject to `PATH`.
    pub program: String,
    /// Its arguments, one element per argument. **Never a secret** — see the
    /// module note.
    pub args: Vec<String>,
    /// Written to the command's standard input and then closed. This is where
    /// a password goes.
    pub stdin: Option<String>,
}

impl Request {
    pub fn new(description: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            program: program.into(),
            args: Vec::new(),
            stdin: None,
        }
    }

    pub fn arg(mut self, value: impl Into<String>) -> Self {
        self.args.push(value.into());
        self
    }

    pub fn args<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(values.into_iter().map(Into::into));
        self
    }

    pub fn stdin(mut self, value: impl Into<String>) -> Self {
        self.stdin = Some(value.into());
        self
    }

    /// The command as it would be written down, for a log or an error. The
    /// standard input is deliberately not part of it.
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The seam between "this needs to happen outside the sandbox" and how.
#[async_trait]
pub trait Exec: Send + Sync {
    async fn run(&self, request: Request) -> anyhow::Result<Outcome>;
}

/// Hand it to systemd.
pub struct SystemdRun {
    /// Overridable so a test can point at something that is not `systemd-run`.
    program: String,
    /// How long to wait before giving up. A field rather than the constant so
    /// a test does not have to take two minutes to prove the bound exists.
    deadline: Duration,
}

impl Default for SystemdRun {
    fn default() -> Self {
        Self {
            program: "systemd-run".to_string(),
            deadline: DEADLINE,
        }
    }
}

impl SystemdRun {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    /// Wait a different length of time. For tests.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// The invocation, as an argument array.
    ///
    /// Split out so it can be asserted on: these flags are the whole of the
    /// contract with systemd, and getting one of them wrong is the difference
    /// between waiting for a result and firing something into the background.
    fn argv(&self, request: &Request) -> Vec<String> {
        let mut argv: Vec<String> = vec![
            // No "Running as unit …" on standard error; the only thing on it
            // should be what the command itself said.
            "--quiet".into(),
            // Unload the unit afterwards even when it failed, so a node does
            // not collect one dead unit per mistyped account name.
            "--collect".into(),
            // Block until it is over, and exit with its status. Without this
            // the call returns the moment the job is queued and every failure
            // becomes silence.
            "--wait".into(),
            // Connect our pipes to it, which is both how the output comes back
            // and how a password reaches `chpasswd` without touching argv.
            "--pipe".into(),
            "--service-type=oneshot".into(),
            // Messages are read by people and matched by tests; neither wants
            // them translated by whatever locale the node happens to have.
            "--setenv=LC_ALL=C".into(),
            format!("--description=Lumen: {}", request.description),
            "--".into(),
        ];
        argv.push(request.program.clone());
        argv.extend(request.args.iter().cloned());
        argv
    }

    /// One attempt: start `systemd-run`, feed it its input, and collect what it
    /// said.
    async fn attempt(&self, request: &Request) -> anyhow::Result<Outcome> {
        let mut child = Command::new(&self.program)
            .args(self.argv(request))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| {
                anyhow::anyhow!(
                    "could not run {} — this appliance asks systemd to perform privileged \
                     operations, and it is not answering: {err}",
                    self.program
                )
            })?;

        // Written and then dropped, so the command sees end-of-input rather
        // than waiting for more. A command that does not read its input is not
        // an error — `useradd` does not — so a broken pipe here is ignored.
        if let Some(input) = request.stdin.as_ref() {
            if let Some(mut pipe) = child.stdin.take() {
                let _ = pipe.write_all(input.as_bytes()).await;
                let _ = pipe.shutdown().await;
            }
        } else {
            drop(child.stdin.take());
        }

        // Bounded, because the alternative is an HTTP request that never
        // answers. Dropping the future drops the child, and `kill_on_drop`
        // ends `systemd-run` — but *not* the transient unit, which is a child
        // of PID 1 and outlives the process that asked for it. The message
        // says so rather than implying the command was called off, because
        // "create the storage pool" may well still be in progress.
        let output = match timeout(self.deadline, child.wait_with_output()).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "gave up waiting for {} after {} seconds. It was handed to systemd as a \
                     transient unit, which is still running it — `systemctl list-units 'run-*'` \
                     shows it and `journalctl -u lumen-controlplane` has the command that was \
                     sent. Nothing was undone.",
                    request.description,
                    self.deadline.as_secs()
                ));
            }
        };
        Ok(Outcome {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// systemd-run complaining that it could not reach PID 1, as opposed to the
/// command complaining about the job.
///
/// `--pipe` hands the command systemd-run's own standard error, so both arrive
/// on one stream with nothing but the wording to tell them apart. These are the
/// shapes systemd-run uses when it never got as far as starting anything, and
/// the distinction is worth drawing: "Failed to start transient service unit:
/// Connection reset by peer" reported as though `zpool` had said it sends an
/// operator to go and look at their disks, when the thing to look at is the
/// path between this daemon and systemd.
fn systemd_refused(stderr: &str) -> Option<&str> {
    const SHAPES: [&str; 5] = [
        "Failed to start transient",
        "Failed to enqueue",
        "Failed to connect to bus",
        "Failed to create bus connection",
        "Failed to set up transient service",
    ];
    stderr
        .lines()
        .map(str::trim)
        .find(|line| SHAPES.iter().any(|shape| line.starts_with(shape)))
}

#[async_trait]
impl Exec for SystemdRun {
    async fn run(&self, request: Request) -> anyhow::Result<Outcome> {
        tracing::info!(command = %request.display(), "running outside the sandbox");

        let mut outcome = self.attempt(&request).await?;

        // PID 1 drops every connection on its private socket when it
        // re-executes, and `systemctl daemon-reload` is enough to do it — so an
        // operation that happened to land in that window failed for a reason
        // that was already over before the message was printed. One retry. The
        // second failure is a real one and is reported as such, and the command
        // has not run at this point, so there is nothing to have run twice.
        if !outcome.ok() && systemd_refused(&outcome.stderr).is_some() {
            tracing::warn!(
                command = %request.display(),
                reason = %outcome.failure(),
                "systemd would not start the unit; trying once more"
            );
            outcome = self.attempt(&request).await?;
        }

        if !outcome.ok() {
            if let Some(refusal) = systemd_refused(&outcome.stderr) {
                return Err(anyhow::anyhow!(
                    "could not {}: systemd would not start it — {refusal}. This appliance hands \
                     the few commands its sandbox forbids to systemd, so this is a failure to \
                     reach PID 1 rather than a failure of the command itself. `systemctl status \
                     lumen-controlplane` and `journalctl -u lumen-controlplane` say more.",
                    request.description
                ));
            }
            tracing::warn!(
                command = %request.display(),
                status = outcome.status,
                reason = %outcome.failure(),
                "privileged command failed"
            );
        }
        Ok(outcome)
    }
}

/// Records what it was asked to run and answers however the test says.
///
/// Compiled unconditionally and exported rather than hidden behind
/// `#[cfg(test)]`, for the reason every other mock in this tree is: a
/// `cfg(test)` item is invisible to another crate's integration tests, and the
/// storage domain's and the control plane's both need this one.
///
/// ## It can also do the work
///
/// Given [`MockExec::backed_by`], it applies `useradd`, `usermod`, `userdel`,
/// and `chpasswd` to a set of account files. That turns it from a mock into a
/// *fake*, and it is worth the hundred lines: the service creates an account
/// and then reads it back out of the files, so without this the round trip
/// that matters most is the one thing tests could not cover.
#[derive(Default)]
pub struct MockExec {
    ran: Mutex<Vec<Request>>,
    /// Answered in order; anything past the end succeeds silently. A test that
    /// cares about a specific failure queues it.
    answers: Mutex<Vec<Outcome>>,
    /// When set, the account commands actually change these files. A plain
    /// field rather than a lock: it is set once by the builder and only ever
    /// read afterwards.
    accounts: Option<crate::state::AccountFiles>,
}

impl MockExec {
    pub fn new() -> Self {
        Self::default()
    }

    /// An exec that succeeds at everything, which is what most tests want.
    pub fn working() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Apply the account commands to these files, rather than only recording
    /// them.
    pub fn backed_by(mut self, files: crate::state::AccountFiles) -> Self {
        self.accounts = Some(files);
        self
    }

    /// Make the next command fail with this on standard error.
    pub async fn fail_next(&self, status: i32, stderr: &str) {
        self.answers.lock().await.push(Outcome {
            status,
            stdout: String::new(),
            stderr: stderr.to_string(),
        });
    }

    /// Make the next command succeed and say this — for the commands a
    /// caller runs to *read* something rather than to change it.
    pub async fn answer_next(&self, stdout: &str) {
        self.answers.lock().await.push(Outcome {
            status: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        });
    }

    /// Everything that has been run, in order.
    pub async fn ran(&self) -> Vec<Request> {
        self.ran.lock().await.clone()
    }

    /// Whether a command with this program and these arguments was run.
    pub async fn ran_with(&self, program: &str, args: &[&str]) -> bool {
        self.ran
            .lock()
            .await
            .iter()
            .any(|request| request.program == program && request.args == args)
    }

    /// The input given to the last command that was given any — so a test can
    /// assert a password went over the pipe rather than into the arguments.
    pub async fn last_stdin(&self) -> Option<String> {
        self.ran
            .lock()
            .await
            .iter()
            .rev()
            .find_map(|request| request.stdin.clone())
    }
}

#[async_trait]
impl Exec for MockExec {
    async fn run(&self, request: Request) -> anyhow::Result<Outcome> {
        let outcome = {
            let mut answers = self.answers.lock().await;
            if answers.is_empty() {
                Outcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            } else {
                answers.remove(0)
            }
        };

        // A command that was going to fail never gets to change anything,
        // which is what makes the rollback path testable.
        if outcome.ok() {
            if let Some(files) = self.accounts.as_ref() {
                apply(files, &request);
            }
        }
        self.ran.lock().await.push(request);
        Ok(outcome)
    }
}

/// Do to the account files what the command would have done to the real ones.
///
/// Deliberately small: it understands the four commands `SysService` issues and
/// the flags it issues them with, and ignores everything else. This is not a
/// re-implementation of `shadow-utils` — it is enough for a service that writes
/// and then reads back to see what it wrote.
fn apply(files: &crate::state::AccountFiles, request: &Request) {
    let read = |path: &std::path::Path| std::fs::read_to_string(path).unwrap_or_default();
    let program = request
        .program
        .rsplit('/')
        .next()
        .unwrap_or(&request.program);
    let args: Vec<&str> = request.args.iter().map(String::as_str).collect();

    // The value after a flag, when the flag is there.
    let after = |flag: &str| -> Option<&str> {
        args.iter()
            .position(|a| *a == flag)
            .and_then(|i| args.get(i + 1))
            .copied()
    };
    let name = args.last().copied().unwrap_or_default();

    match program {
        "useradd" => {
            let mut passwd = read(&files.passwd);
            // The next free identifier at or above the human floor, which is
            // what useradd itself does.
            let uid = passwd
                .lines()
                .filter_map(|line| line.split(':').nth(2)?.parse::<u32>().ok())
                .filter(|uid| {
                    (crate::model::FIRST_HUMAN_UID..=crate::model::LAST_HUMAN_UID).contains(uid)
                })
                .max()
                .map(|max| max + 1)
                .unwrap_or(crate::model::FIRST_HUMAN_UID);
            let shell = after("-s").unwrap_or(crate::model::DEFAULT_SHELL);
            let gecos = after("-c").unwrap_or("");
            passwd.push_str(&format!(
                "{name}:x:{uid}:{uid}:{gecos}:/home/{name}:{shell}\n"
            ));
            let _ = std::fs::write(&files.passwd, passwd);

            let mut group = read(&files.group);
            group.push_str(&format!("{name}:x:{uid}:\n"));
            if let Some(extra) = after("-G") {
                group = add_to_groups(&group, name, extra);
            }
            let _ = std::fs::write(&files.group, group);

            // No password yet — `!!` is exactly what useradd leaves behind,
            // and it is why the service sets one immediately afterwards.
            let mut shadow = read(&files.shadow);
            shadow.push_str(&format!("{name}:!!:20000:0:99999:7:::\n"));
            let _ = std::fs::write(&files.shadow, shadow);
        }
        "userdel" => {
            let drop_lines = |text: String| -> String {
                text.lines()
                    .filter(|line| line.split(':').next() != Some(name))
                    .map(|line| format!("{line}\n"))
                    .collect()
            };
            let _ = std::fs::write(&files.passwd, drop_lines(read(&files.passwd)));
            let _ = std::fs::write(&files.shadow, drop_lines(read(&files.shadow)));
            let _ = std::fs::write(
                &files.group,
                remove_from_groups(&drop_lines(read(&files.group)), name),
            );
        }
        "chpasswd" => {
            let Some((who, _password)) = request
                .stdin
                .as_deref()
                .and_then(|input| input.trim().split_once(':'))
            else {
                return;
            };
            let _ = std::fs::write(
                &files.shadow,
                rewrite_shadow(&read(&files.shadow), who, |_| "$6$mock$hash".to_string()),
            );
        }
        "usermod" => {
            if args.contains(&"-L") {
                let _ = std::fs::write(
                    &files.shadow,
                    rewrite_shadow(&read(&files.shadow), name, |hash| {
                        if hash.starts_with('!') {
                            hash.to_string()
                        } else {
                            format!("!{hash}")
                        }
                    }),
                );
            }
            if args.contains(&"-U") {
                let _ = std::fs::write(
                    &files.shadow,
                    rewrite_shadow(&read(&files.shadow), name, |hash| {
                        hash.trim_start_matches('!').to_string()
                    }),
                );
            }
            if let Some(groups) = after("-G") {
                let base = read(&files.group);
                // `-a -G` adds; a bare `-G` replaces the supplementary list.
                let cleared = if args.contains(&"-a") {
                    base
                } else {
                    remove_from_groups(&base, name)
                };
                let _ = std::fs::write(&files.group, add_to_groups(&cleared, name, groups));
            }
            let mut passwd = read(&files.passwd);
            if let Some(gecos) = after("-c") {
                passwd = rewrite_passwd(&passwd, name, 4, gecos);
            }
            if let Some(shell) = after("-s") {
                passwd = rewrite_passwd(&passwd, name, 6, shell);
            }
            let _ = std::fs::write(&files.passwd, passwd);
        }
        _ => {}
    }
}

fn rewrite_passwd(passwd: &str, name: &str, field: usize, value: &str) -> String {
    passwd
        .lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split(':').collect();
            if fields.first() == Some(&name) && fields.len() > field {
                fields[field] = value;
                format!("{}\n", fields.join(":"))
            } else {
                format!("{line}\n")
            }
        })
        .collect()
}

fn rewrite_shadow(shadow: &str, name: &str, change: impl Fn(&str) -> String) -> String {
    shadow
        .lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split(':').collect();
            if fields.first() == Some(&name) && fields.len() > 1 {
                let replaced = change(fields[1]);
                fields[1] = &replaced;
                return format!("{}\n", fields.join(":"));
            }
            format!("{line}\n")
        })
        .collect()
}

fn add_to_groups(group: &str, name: &str, wanted: &str) -> String {
    let wanted: Vec<&str> = wanted.split(',').filter(|g| !g.is_empty()).collect();
    group
        .lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split(':').collect();
            if fields.len() < 4 || !wanted.contains(&fields[0]) {
                return format!("{line}\n");
            }
            let mut members: Vec<&str> = fields[3].split(',').filter(|m| !m.is_empty()).collect();
            if !members.contains(&name) {
                members.push(name);
            }
            let joined = members.join(",");
            fields[3] = &joined;
            format!("{}\n", fields.join(":"))
        })
        .collect()
}

fn remove_from_groups(group: &str, name: &str) -> String {
    group
        .lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split(':').collect();
            if fields.len() < 4 {
                return format!("{line}\n");
            }
            let kept: Vec<&str> = fields[3]
                .split(',')
                .filter(|m| !m.is_empty() && *m != name)
                .collect();
            let joined = kept.join(",");
            fields[3] = &joined;
            format!("{}\n", fields.join(":"))
        })
        .collect()
}

/// There is no way to run anything, and every attempt says why.
///
/// Used when the node has no systemd to ask — which on this appliance means
/// something is very wrong, but a control plane that still answers is more
/// useful than one that does not start.
pub struct UnavailableExec {
    reason: String,
}

impl UnavailableExec {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[async_trait]
impl Exec for UnavailableExec {
    async fn run(&self, request: Request) -> anyhow::Result<Outcome> {
        Err(anyhow::anyhow!(
            "cannot {}: {}",
            request.description,
            self.reason
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flags_are_the_contract_with_systemd() {
        let argv = SystemdRun::new().argv(
            &Request::new("create the account \"alice\"", "/usr/sbin/useradd")
                .args(["-m", "alice"]),
        );

        // --wait is the one that matters most: without it the call returns as
        // soon as the job is queued and every failure becomes silence.
        assert!(argv.contains(&"--wait".to_string()));
        // --pipe is how the output comes back and how a password reaches the
        // command without ever being an argument.
        assert!(argv.contains(&"--pipe".to_string()));
        // --collect stops a node accumulating a dead unit per failed attempt.
        assert!(argv.contains(&"--collect".to_string()));
        assert!(argv.contains(&"--quiet".to_string()));

        // Everything after -- is the command itself, unchanged and unsplit.
        let separator = argv.iter().position(|a| a == "--").expect("a separator");
        assert_eq!(&argv[separator + 1..], ["/usr/sbin/useradd", "-m", "alice"]);
    }

    /// There is no string for an argument to escape out of, so a name that
    /// would be a disaster in a shell is simply an argument.
    #[test]
    fn an_argument_is_an_argument_and_never_part_of_a_sentence() {
        let argv = SystemdRun::new().argv(&Request::new("x", "/usr/sbin/useradd").args([
            "-m",
            "; reboot #",
            "$(whoami)",
        ]));
        let separator = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            &argv[separator + 1..],
            ["/usr/sbin/useradd", "-m", "; reboot #", "$(whoami)"]
        );
    }

    #[test]
    fn a_secret_is_never_something_you_could_read_out_of_ps() {
        let request = Request::new("set a password", "/usr/sbin/chpasswd").stdin("alice:hunter2");
        assert!(!request.display().contains("hunter2"));
        assert!(!SystemdRun::new()
            .argv(&request)
            .iter()
            .any(|arg| arg.contains("hunter2")));
    }

    #[test]
    fn a_failure_reports_the_specific_line_rather_than_the_lead_up() {
        let outcome = Outcome {
            status: 9,
            stdout: String::new(),
            stderr: "useradd: warning: the home directory already exists\n\
                     useradd: user 'alice' already exists\n"
                .into(),
        };
        assert_eq!(outcome.failure(), "useradd: user 'alice' already exists");

        // A command that failed silently still says something.
        let quiet = Outcome {
            status: 4,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(quiet.failure(), "the command failed with status 4");
    }

    /// `--pipe` puts systemd-run's complaints and the command's on one stream,
    /// and an operator sent to check their disks over a bus failure has been
    /// sent to the wrong place.
    #[test]
    fn systemds_own_refusal_is_not_mistaken_for_the_commands() {
        assert_eq!(
            systemd_refused("Failed to start transient service unit: Connection reset by peer\n"),
            Some("Failed to start transient service unit: Connection reset by peer")
        );
        assert!(systemd_refused("Failed to connect to bus: No such file or directory").is_some());

        // What `zpool` and `useradd` say is theirs, and stays theirs.
        assert_eq!(systemd_refused("cannot create 'tank': no such pool"), None);
        assert_eq!(
            systemd_refused("useradd: user 'alice' already exists"),
            None
        );
        assert_eq!(systemd_refused(""), None);
    }

    /// A command that never returns has to become a sentence, not a request
    /// that hangs until the operator reloads the page — the real cases being a
    /// disk that will not settle and a bus that took the job and never
    /// answered.
    ///
    /// A script rather than `sleep` itself: the stand-in is invoked in
    /// `systemd-run`'s place, so it is handed `--quiet --collect --wait …`
    /// first, and every real program exits on those rather than blocking. This
    /// one ignores its arguments, which is the whole reason it exists.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_command_that_never_returns_is_given_up_on() {
        use std::os::unix::fs::PermissionsExt;

        let script = std::env::temp_dir().join(format!("lumen-sys-blocks-{}", std::process::id()));
        std::fs::write(&script, "#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let exec = SystemdRun::with_program(script.to_string_lossy().into_owned())
            .with_deadline(Duration::from_millis(150));
        let err = exec
            .run(Request::new(
                "create the storage pool \"tank\"",
                "/usr/sbin/zpool",
            ))
            .await
            .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("gave up waiting"), "{message}");
        // Named, so the operator knows which operation is unaccounted for.
        assert!(message.contains("create the storage pool"), "{message}");
        // And told where it went, because killing systemd-run does not stop
        // the transient unit it started.
        assert!(message.contains("run-*"), "{message}");

        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn the_mock_records_what_it_was_asked_to_do() {
        let exec = MockExec::new();
        exec.run(Request::new("x", "/usr/sbin/useradd").args(["-m", "alice"]))
            .await
            .unwrap();
        assert!(exec.ran_with("/usr/sbin/useradd", &["-m", "alice"]).await);

        exec.fail_next(1, "useradd: user 'bob' already exists")
            .await;
        let outcome = exec
            .run(Request::new("x", "/usr/sbin/useradd").arg("bob"))
            .await
            .unwrap();
        assert!(!outcome.ok());
        assert!(outcome.failure().contains("already exists"));
    }

    #[tokio::test]
    async fn an_unavailable_exec_says_what_could_not_be_done() {
        let exec = UnavailableExec::new("systemd is not answering");
        let err = exec
            .run(Request::new(
                "create the account \"alice\"",
                "/usr/sbin/useradd",
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("create the account"), "{err}");
        assert!(err.to_string().contains("not answering"), "{err}");
    }
}
