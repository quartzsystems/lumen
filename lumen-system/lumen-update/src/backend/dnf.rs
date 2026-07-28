//! The node's own package manager, reached through its command line.
//!
//! The same trade `lumen-zfs` made against `libzfs` and `lumen-cluster` made
//! against the cluster libraries, for the same reasons: the command line is
//! the supported interface, it is what an operator would type, and it keeps
//! bindgen and libclang out of the appliance toolchain. It carries the same
//! rule too — **typed argument arrays, never an interpolated shell string**.
//! There is nothing to escape because there is no string to escape into.
//!
//! ## Everything goes through the privileged runner, including the reads
//!
//! `lumen-controlplane.service` runs with `ProtectSystem=strict`. Refreshing
//! repository metadata writes `/var/cache/libdnf5`, taking the package
//! manager's lock writes `/var/lib/dnf`, and a transaction writes the whole
//! system — none of which the daemon may do. So every invocation is handed to
//! systemd as a transient unit, exactly as `useradd` and `zpool create` are.
//! See `lumen_sys::exec` and docs/system.md.
//!
//! **The runner needs a long deadline.** `lumen_sys::exec` defaults to two
//! minutes, which is right for `useradd` and nowhere near enough for a
//! transaction that downloads a kernel. [`DnfBackend`] is constructed with an
//! [`Exec`] of its own — see [`crate::service::UpdateService`]'s note — and a
//! transaction that outlives even that is reported as a timeout rather than
//! left pending forever.
//!
//! ## What the exit status does and does not tell you
//!
//! `check-update` answers **100** when there are updates and **0** when there
//! are none, so a nonzero status is not a failure here.
//!
//! `--assumeno` exits nonzero after resolving *successfully*, which is why the
//! dry run reads the printed transaction summary rather than the status. That
//! is not a trick invented here: iso/build-live-iso.sh's offline-resolution
//! gate does the same thing against the same package manager, and it is what
//! proves every ISO can install its target set.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use lumen_sys::exec::{Exec, Request};

use crate::backend::unavailable::running_release;
use crate::backend::UpdateBackend;
use crate::error::{Result, UpdateError};
use crate::model::{ApplyPlan, ApplyReport, KernelState, Resolution, Update, UpdateKind};

/// Absolute paths, because `execve` resolves them and no shell is involved.
const DNF: &str = "/usr/bin/dnf";
const RPM: &str = "/usr/bin/rpm";

/// The architectures a `name.arch` column can legitimately end with. Used to
/// tell a package row from a header, a progress line, or a wrapped message —
/// all of which the package manager prints on the same stream.
const ARCHES: &[&str] = &["x86_64", "noarch", "aarch64", "i686", "src"];

/// How much of a transaction's output is kept.
///
/// A kernel upgrade prints thousands of lines and every interesting one — what
/// failed, what was installed, what the scriptlets said — is at the end. The
/// whole thing goes to the journal regardless; this is what the console is
/// handed.
const LOG_TAIL_BYTES: usize = 64 * 1024;

pub struct DnfBackend {
    exec: Arc<dyn Exec>,
}

impl DnfBackend {
    pub fn new(exec: Arc<dyn Exec>) -> Self {
        Self { exec }
    }

    async fn run(&self, description: &str, args: Vec<String>) -> Result<lumen_sys::Outcome> {
        let request = Request::new(description, DNF).args(args);
        tracing::debug!("update: {}", request.display());
        self.exec.run(request).await.map_err(UpdateError::backend)
    }
}

#[async_trait]
impl UpdateBackend for DnfBackend {
    async fn check(&self) -> Result<Vec<Update>> {
        // --refresh is the point of an explicit check: an operator pressing
        // "Check now" wants the repositories asked, not the cache re-read.
        //
        // --assumeyes answers exactly one question, and check-update installs
        // nothing, so it cannot answer anything more dangerous: the first time a
        // node reads a repository with repo_gpgcheck, dnf asks whether to import
        // that repository's key. Nothing here is a terminal, so unanswered means
        // the repository fails to load, and skip_if_unavailable then drops it
        // without a word — a node with updates waiting reports none, which is
        // the worst possible way for this to fail. The key being imported is the
        // one already installed at the path lumen.repo names, by a package the
        // operator chose to install; consenting to it again is not a decision.
        let outcome = self
            .run(
                "Lumen: check for updates",
                vec![
                    "--quiet".into(),
                    "--assumeyes".into(),
                    "--refresh".into(),
                    "check-update".into(),
                ],
            )
            .await?;

        // 0 = nothing waiting, 100 = something waiting. Anything else failed.
        match outcome.status {
            0 => return Ok(Vec::new()),
            100 => {}
            _ => {
                return Err(UpdateError::backend(anyhow::anyhow!(
                    "could not check for updates: {}",
                    outcome.failure()
                )))
            }
        }

        let mut updates = parse_check_update(&outcome.stdout);

        // What is installed now, in one call rather than one per package. A
        // failure here costs the "from" column and nothing else, so it is not
        // worth failing the check over.
        match self.installed().await {
            Ok(installed) => {
                for update in &mut updates {
                    update.installed = installed.get(&update.name).cloned();
                }
            }
            Err(err) => tracing::warn!("could not read the installed versions: {err}"),
        }

        // Advisories, likewise best-effort: the repositories may publish none,
        // and a node that cannot read them is not a node that should be told
        // it has no security updates. See `parse_updateinfo`.
        match self
            .run(
                "Lumen: read update advisories",
                vec![
                    "--quiet".into(),
                    "updateinfo".into(),
                    "list".into(),
                    "--updates".into(),
                ],
            )
            .await
        {
            Ok(outcome) if outcome.ok() => {
                let names: Vec<String> = updates.iter().map(|u| u.name.clone()).collect();
                let advisories = parse_updateinfo(&outcome.stdout, &names);
                for update in &mut updates {
                    if let Some((id, security)) = advisories.get(&update.name) {
                        update.advisory = Some(id.clone());
                        update.security = *security;
                    }
                }
            }
            Ok(outcome) => tracing::warn!("no advisory metadata: {}", outcome.failure()),
            Err(err) => tracing::warn!("could not read advisory metadata: {err}"),
        }

        Ok(updates)
    }

    async fn resolve(&self, packages: &[String]) -> Result<Resolution> {
        if packages.is_empty() {
            return Ok(Resolution {
                ok: true,
                detail: "Nothing to resolve.".into(),
            });
        }
        let mut args = vec!["--assumeno".to_string(), "upgrade".to_string()];
        args.extend(packages.iter().cloned());
        let outcome = self
            .run(
                "Lumen: test whether updates can be installed together",
                args,
            )
            .await?;

        // The status is meaningless under --assumeno; the printed summary is
        // the success signal. See the module note.
        let combined = format!("{}\n{}", outcome.stdout, outcome.stderr);
        if combined.contains("Transaction Summary") {
            Ok(Resolution {
                ok: true,
                detail: "These can be installed together.".into(),
            })
        } else {
            Ok(Resolution {
                ok: false,
                detail: solver_complaint(&combined),
            })
        }
    }

    async fn apply(&self, plan: &ApplyPlan) -> Result<ApplyReport> {
        let mut args = vec!["-y".to_string(), "upgrade".to_string()];
        for glob in &plan.exclude {
            args.push(format!("--exclude={glob}"));
        }
        args.extend(plan.packages.iter().cloned());

        let outcome = self.run("Lumen: install updates", args).await?;
        let combined = format!("{}\n{}", outcome.stdout, outcome.stderr);
        if !outcome.ok() {
            return Err(UpdateError::backend(anyhow::anyhow!(
                "the update did not finish: {}",
                outcome.failure()
            )));
        }
        Ok(ApplyReport {
            changed: parse_changed(&outcome.stdout),
            log: tail(&combined, LOG_TAIL_BYTES),
        })
    }

    async fn kernel(&self) -> Result<KernelState> {
        let running = running_release();
        // Install time, not version order: sorting release strings correctly
        // means reimplementing rpmvercmp, and the newest kernel on an
        // appliance is always the one installed last.
        let request = Request::new("Lumen: read the installed kernels", RPM).args([
            "-q",
            "--qf",
            "%{INSTALLTIME}\\t%{VERSION}-%{RELEASE}.%{ARCH}\\n",
            "kernel-core",
        ]);
        let outcome = self.exec.run(request).await.map_err(UpdateError::backend)?;
        let newest = outcome
            .ok()
            .then(|| newest_kernel(&outcome.stdout))
            .flatten();
        Ok(KernelState { running, newest })
    }
}

impl DnfBackend {
    /// Every installed package and its version, in one call.
    async fn installed(&self) -> Result<HashMap<String, String>> {
        let request = Request::new("Lumen: read the installed versions", RPM).args([
            "-qa",
            "--qf",
            "%{NAME}\\t%{VERSION}-%{RELEASE}\\n",
        ]);
        let outcome = self.exec.run(request).await.map_err(UpdateError::backend)?;
        if !outcome.ok() {
            return Err(UpdateError::backend(anyhow::anyhow!(
                "could not read the package database: {}",
                outcome.failure()
            )));
        }
        Ok(outcome
            .stdout
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(name, version)| (name.to_string(), version.trim().to_string()))
            .collect())
    }
}

/// The three-column `check-update` listing, minus everything else printed on
/// the same stream.
///
/// A row is `name.arch  version  repository`. Anything that is not exactly
/// three fields, or whose first field does not end in a known architecture, is
/// a header, a progress line, or a wrapped sentence — and skipping those is
/// safer than trying to enumerate them, because the set changes between
/// package-manager releases and a misparsed header would become a fictitious
/// package on the console.
fn parse_check_update(stdout: &str) -> Vec<Update> {
    let mut updates = Vec::new();
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 3 {
            continue;
        }
        // Split at the LAST dot: package names contain them (`kmod-zfs-2.3`),
        // and the architecture never does.
        let Some((name, arch)) = fields[0].rsplit_once('.') else {
            continue;
        };
        if !ARCHES.contains(&arch) || name.is_empty() {
            continue;
        }
        updates.push(Update {
            name: name.to_string(),
            arch: arch.to_string(),
            version: fields[1].to_string(),
            installed: None,
            repo: fields[2].to_string(),
            kind: UpdateKind::of(name),
            advisory: None,
            security: false,
        });
    }
    updates
}

/// Advisory identifiers, matched back onto package names.
///
/// The listing's third column is a full NEVRA, and taking a package name back
/// out of one means knowing where the version starts — which is guesswork in
/// general. It is not guesswork here, because the set of names in play is
/// already known: the longest known name that the NEVRA starts with, followed
/// by a hyphen, is the package. Anything that matches nothing is skipped.
///
/// Entirely best-effort. A repository that publishes no advisory metadata is
/// ordinary, and the console says "nothing said so" rather than "no security
/// updates" precisely because of that.
fn parse_updateinfo(stdout: &str, names: &[String]) -> HashMap<String, (String, bool)> {
    let mut found = HashMap::new();
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        let (id, kind, nevra) = (fields[0], fields[1], fields[fields.len() - 1]);
        // An advisory identifier always carries a colon (`ALSA-2026:1234`).
        if !id.contains(':') {
            continue;
        }
        let matched = names
            .iter()
            .filter(|name| nevra.starts_with(&format!("{name}-")))
            .max_by_key(|name| name.len());
        if let Some(name) = matched {
            let security = kind.to_ascii_lowercase().contains("sec");
            found.insert(name.clone(), (id.to_string(), security));
        }
    }
    found
}

/// The package name inside a full `name-version-release.arch`.
///
/// Three separators back from the end, and the first two are the only ones
/// whose position is knowable: the architecture after the last dot, the release
/// after the last hyphen, the version after the one before it. Everything left
/// is the name — which is how a name may itself contain both hyphens and dots
/// (`kmod-zfs-2.3`) without the split going wrong.
fn nevra_name(nevra: &str) -> &str {
    let without_arch = nevra
        .rsplit_once('.')
        .map(|(rest, _)| rest)
        .unwrap_or(nevra);
    let without_release = without_arch
        .rsplit_once('-')
        .map(|(rest, _)| rest)
        .unwrap_or(without_arch);
    without_release
        .rsplit_once('-')
        .map(|(name, _)| name)
        .unwrap_or(without_release)
}

/// The packages a transaction reported changing.
///
/// Best-effort and deliberately not load-bearing: what actually happened to
/// this node is established by re-reading the package database, not by parsing
/// a progress listing. This is for the console's summary line.
fn parse_changed(stdout: &str) -> Vec<String> {
    let mut changed = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed
            .strip_prefix("Upgrading ")
            .or_else(|| trimmed.strip_prefix("Installing "))
            .or_else(|| trimmed.strip_prefix("Removing "))
        else {
            continue;
        };
        if let Some(nevra) = rest.split_whitespace().next() {
            changed.push(nevra_name(nevra).to_string());
        }
    }
    changed.sort();
    changed.dedup();
    changed
}

/// The most useful thing a failed resolution said.
///
/// The solver puts the specific complaint — "nothing provides …", "cannot
/// install the best update candidate" — near the end, after the repository
/// chatter. The last few non-empty lines carry it.
fn solver_complaint(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return "The package manager gave no reason.".to_string();
    }
    let start = lines.len().saturating_sub(6);
    lines[start..].join(" ")
}

/// The kernel installed most recently, from `INSTALLTIME<tab>release` rows.
fn newest_kernel(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter_map(|(when, release)| {
            when.trim()
                .parse::<u64>()
                .ok()
                .map(|when| (when, release.trim().to_string()))
        })
        .max_by_key(|(when, _)| *when)
        .map(|(_, release)| release)
}

/// The last `limit` bytes, cut at a line boundary so the console never shows
/// half a line.
fn tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let cut = text.len() - limit;
    let from = text[cut..]
        .find('\n')
        .map(|offset| cut + offset + 1)
        .unwrap_or(cut);
    text[from..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_listing_becomes_updates_and_headers_do_not() {
        let stdout = "\
Updating and loading repositories:
Repositories loaded.

lumen-controlplane.x86_64          0.4.0-1.el10        lumen
kmod-zfs-2.3.x86_64                2.3.4-1.el10        zfs-2.3-kmod
NetworkManager.x86_64              1:1.52.0-1.el10     baseos
this line has too many fields to be a package row entirely
";
        let updates = parse_check_update(stdout);
        assert_eq!(updates.len(), 3);
        assert_eq!(updates[0].name, "lumen-controlplane");
        assert_eq!(updates[0].kind, UpdateKind::Lumen);
        // The name carries dots of its own; only the last one is the arch.
        assert_eq!(updates[1].name, "kmod-zfs-2.3");
        assert_eq!(updates[1].arch, "x86_64");
        assert_eq!(updates[1].kind, UpdateKind::Platform);
        assert_eq!(updates[2].version, "1:1.52.0-1.el10");
        assert_eq!(updates[2].kind, UpdateKind::Other);
    }

    #[test]
    fn nothing_waiting_parses_to_nothing() {
        assert!(parse_check_update("").is_empty());
        assert!(parse_check_update("Repositories loaded.\n").is_empty());
    }

    #[test]
    fn advisories_attach_to_the_longest_matching_name() {
        let names = vec!["kmod-zfs".to_string(), "kmod-zfs-2.3".to_string()];
        let stdout = "\
ALSA-2026:1234 Security/Sec.  kmod-zfs-2.3-2.3.4-1.el10.x86_64
ALBA-2026:9999 bugfix         something-else-1.0-1.el10.noarch
not an advisory line at all
";
        let found = parse_updateinfo(stdout, &names);
        // The longer name matches, not the prefix that also would.
        let (id, security) = found.get("kmod-zfs-2.3").expect("matched");
        assert_eq!(id, "ALSA-2026:1234");
        assert!(security);
        assert!(!found.contains_key("kmod-zfs"));
    }

    #[test]
    fn the_newest_kernel_is_the_one_installed_last() {
        let stdout = "\
1750000000\t6.12.0-211.7.3.el10_2.x86_64
1760000000\t6.12.0-212.el10.x86_64
";
        assert_eq!(
            newest_kernel(stdout).as_deref(),
            Some("6.12.0-212.el10.x86_64")
        );
        assert_eq!(newest_kernel(""), None);
        assert_eq!(newest_kernel("package kernel-core is not installed"), None);
    }

    /// The split that a name containing both hyphens and dots would break if
    /// it were done by searching forwards instead of backwards.
    #[test]
    fn a_name_is_taken_back_out_of_a_full_package_identifier() {
        assert_eq!(
            nevra_name("lumen-controlplane-0.4.0-1.el10.x86_64"),
            "lumen-controlplane"
        );
        assert_eq!(
            nevra_name("kmod-zfs-2.3-2.3.4-1.el10.x86_64"),
            "kmod-zfs-2.3"
        );
        assert_eq!(
            nevra_name("kernel-core-6.12.0-212.el10.x86_64"),
            "kernel-core"
        );
    }

    #[test]
    fn changed_packages_come_off_the_transaction_listing() {
        let stdout = "\
Upgrading lumen-controlplane-0.4.0-1.el10.x86_64
Installing kmod-zfs-2.3-2.3.4-1.el10.x86_64
Upgrading lumen-controlplane-0.4.0-1.el10.x86_64
Complete!
";
        assert_eq!(
            parse_changed(stdout),
            vec!["kmod-zfs-2.3", "lumen-controlplane"]
        );
    }

    #[test]
    fn a_long_log_is_cut_at_a_line_boundary() {
        let text = "first\n".repeat(1000);
        let cut = tail(&text, 50);
        assert!(cut.len() <= 50);
        assert!(cut.starts_with("first"), "{cut:?}");
        // Short input is returned whole.
        assert_eq!(tail("short\n", 4096), "short\n");
    }

    #[test]
    fn a_failed_resolution_reports_the_solvers_own_words() {
        let output = "\
Updating and loading repositories:
Repositories loaded.
Problem: cannot install the best update candidate
  - nothing provides kernel-uname-r = 6.12.0-212.el10.x86_64 needed by kmod-zfs
";
        let complaint = solver_complaint(output);
        assert!(complaint.contains("nothing provides"), "{complaint}");
        assert_eq!(solver_complaint(""), "The package manager gave no reason.");
    }
}
