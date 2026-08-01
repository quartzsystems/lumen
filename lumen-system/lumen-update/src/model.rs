//! What an update is, which kind it belongs to, and what a transaction did.

use serde::{Deserialize, Serialize};

/// The package-name prefixes that make up the **platform set**: the kernel and
/// everything pinned against its ABI.
///
/// These are the packages iso/pins.env moves as one — bump one, re-verify
/// the set — and the ones an ordinary update must never touch. The list is
/// prefixes rather than exact names because the kernel alone arrives as
/// `kernel`, `kernel-core`, `kernel-modules`, `kernel-modules-core`, and
/// `kernel-tools`, and the kmod packages carry their series in the name
/// (`kmod-zfs-2.3`).
///
/// `zfs` also catches `zfs-dracut`, which is what puts the pool import into
/// the initramfs — a userland package by packaging but part of the boot path
/// in practice, so it moves with the module rather than against it.
pub const PLATFORM_PREFIXES: &[&str] = &["kernel", "kmod-", "zfs"];

/// Whether a package belongs to the platform set.
///
/// Matched on the package name only. The repository it came from is not
/// consulted deliberately: a kernel is a kernel whether it arrived from
/// AlmaLinux, from a mirror, or from something an operator added by hand, and
/// a rule that keyed on the repository would stop protecting the node the
/// moment somebody configured a new one.
pub fn is_platform(name: &str) -> bool {
    PLATFORM_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Which of four things an update is, from the console's point of view.
///
/// The kinds are about *what an operator decides*, not about where the package
/// came from: "Lumen's own" and "the platform" are separate decisions with
/// separate buttons, security is a reason to stop putting one off, and the
/// rest is the long tail nobody reads individually.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateKind {
    /// Lumen's own packages — the control plane, the console, the branding.
    Lumen,
    /// Kernel, kernel modules, ZFS. See [`PLATFORM_PREFIXES`].
    Platform,
    /// Everything else the distribution ships.
    Other,
}

impl UpdateKind {
    /// Which kind a package name belongs to. The platform check comes first:
    /// a `kmod-` package that somehow shipped from the Lumen repository is
    /// still a kernel module and still moves with the kernel.
    pub fn of(name: &str) -> Self {
        if is_platform(name) {
            UpdateKind::Platform
        } else if name.starts_with("lumen-") {
            UpdateKind::Lumen
        } else {
            UpdateKind::Other
        }
    }
}

/// One package with a newer build waiting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    pub name: String,
    pub arch: String,
    /// The epoch-version-release that would be installed.
    pub version: String,
    /// What is installed now. `None` when the package manager named a package
    /// that is not installed at all — an obsoletion, usually — which is worth
    /// showing rather than hiding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
    /// The repository it would come from, as configured on this node.
    pub repo: String,
    pub kind: UpdateKind,
    /// The advisory that covers it, when the repository publishes advisory
    /// metadata and one applies. Absent is not "not a security fix" — it is
    /// "nothing said so"; see [`crate::backend::UpdateBackend::check`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisory: Option<String>,
    /// Whether that advisory is a security advisory.
    #[serde(default)]
    pub security: bool,
}

impl Update {
    /// A plain row for a test or an error message.
    pub fn new(name: &str, version: &str, repo: &str) -> Self {
        Self {
            name: name.to_string(),
            arch: "x86_64".to_string(),
            version: version.to_string(),
            installed: None,
            repo: repo.to_string(),
            kind: UpdateKind::of(name),
            advisory: None,
            security: false,
        }
    }
}

/// What the package manager said when asked to resolve a set without applying
/// it.
///
/// This is the platform gate's whole mechanism: not a version comparison Lumen
/// keeps, but the solver's own answer about its own repositories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    /// Whether it could be resolved as one transaction.
    pub ok: bool,
    /// The solver's words. On failure this is what the console shows — the
    /// message naming the unresolvable dependency is far more useful than
    /// anything this crate could write about it.
    pub detail: String,
}

/// The platform set's state: what is waiting, and whether it can move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformPlan {
    /// The platform packages with a newer build available.
    pub updates: Vec<Update>,
    /// Whether the package manager can resolve them together. Meaningless when
    /// `updates` is empty, and then reported as `true` — there is nothing to
    /// fail to resolve.
    pub resolves: bool,
    /// Why not, when it cannot. The solver's own message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl PlatformPlan {
    /// Nothing waiting: the ordinary state of a node between point releases.
    pub fn none() -> Self {
        Self {
            updates: Vec::new(),
            resolves: true,
            detail: None,
        }
    }

    pub fn pending(&self) -> bool {
        !self.updates.is_empty()
    }

    /// Whether the console may offer the button. Something to do, and a solver
    /// that says it can be done.
    pub fn offerable(&self) -> bool {
        self.pending() && self.resolves
    }
}

/// The running kernel against the newest installed one.
///
/// Read from `uname` and the package database rather than from a
/// package-manager plugin: `needs-restarting` lives in a separate package that
/// an appliance has no other reason to carry, and a node that lacked it would
/// silently report "no reboot needed" — the dangerous direction to be wrong in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelState {
    /// What `uname -r` says.
    pub running: String,
    /// The newest kernel the package database has, in `uname -r` form. `None`
    /// when the package database could not be read, and then no claim is made
    /// either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest: Option<String>,
}

impl KernelState {
    /// A kernel is installed that is not the one running.
    pub fn stale(&self) -> bool {
        self.newest
            .as_deref()
            .is_some_and(|newest| newest != self.running)
    }
}

/// Whether this node should be restarted, and the sentence explaining why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebootState {
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub kernel: KernelState,
}

impl RebootState {
    /// The state a kernel reading implies. The only condition that makes this
    /// appliance genuinely need a restart is a kernel it is not running:
    /// userland packages are restarted by the package manager's own scriptlets
    /// and by `systemctl daemon-reload`, and telling an operator to reboot for
    /// them would train them to ignore the notice that matters.
    pub fn from_kernel(kernel: KernelState) -> Self {
        let required = kernel.stale();
        let reason = required.then(|| {
            format!(
                "This node is running {} and has {} installed. The new kernel — and the storage \
                 modules built against it — take effect on the next restart.",
                kernel.running,
                kernel.newest.as_deref().unwrap_or("a newer kernel"),
            )
        });
        Self {
            required,
            reason,
            kernel,
        }
    }
}

/// What to apply, as the domain hands it to the package manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyPlan {
    /// Package names to upgrade. Empty means "everything that is not
    /// excluded", which is the ordinary case.
    pub packages: Vec<String>,
    /// Package name globs that must not move. The ordinary transaction
    /// excludes the whole platform set; the platform transaction excludes
    /// nothing, because it *is* the platform set.
    pub exclude: Vec<String>,
}

impl ApplyPlan {
    /// Everything except the platform set: the Tuesday-afternoon update.
    pub fn ordinary() -> Self {
        Self {
            packages: Vec::new(),
            exclude: PLATFORM_PREFIXES
                .iter()
                .map(|prefix| format!("{prefix}*"))
                .collect(),
        }
    }

    /// The platform set, by the names actually waiting. Named explicitly
    /// rather than by glob so the transaction is exactly what the operator was
    /// shown and agreed to — a glob would quietly widen between the page
    /// rendering and the button being pressed.
    pub fn platform(updates: &[Update]) -> Self {
        Self {
            packages: updates.iter().map(|u| u.name.clone()).collect(),
            exclude: Vec::new(),
        }
    }
}

/// What a transaction did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    /// The packages the package manager reported as changed.
    pub changed: Vec<String>,
    /// Its output, for the console's log pane and for the journal. Trimmed to
    /// the tail by the caller — a full transaction log is thousands of lines
    /// and none of the interesting ones are at the start.
    pub log: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_platform_set_is_matched_by_prefix() {
        for name in [
            "kernel",
            "kernel-core",
            "kernel-modules-core",
            "kmod-zfs-2.3",
            "zfs",
            "zfs-dracut",
        ] {
            assert!(is_platform(name), "{name} should be platform");
            assert_eq!(UpdateKind::of(name), UpdateKind::Platform);
        }
        for name in ["libvirt", "NetworkManager", "openssl-libs", "lumen-release"] {
            assert!(!is_platform(name), "{name} should not be platform");
        }
    }

    #[test]
    fn lumens_own_packages_are_their_own_kind() {
        assert_eq!(UpdateKind::of("lumen-controlplane"), UpdateKind::Lumen);
        assert_eq!(UpdateKind::of("lumen-release"), UpdateKind::Lumen);
        assert_eq!(UpdateKind::of("libvirt"), UpdateKind::Other);
    }

    /// The one classification that could go wrong in a way that matters: a
    /// kernel module published in Lumen's own repository is still a kernel
    /// module, and must not be swept into an ordinary update.
    #[test]
    fn a_kernel_module_is_platform_even_if_it_were_ours() {
        assert_eq!(UpdateKind::of("kmod-zfs-2.3"), UpdateKind::Platform);
        assert!(ApplyPlan::ordinary()
            .exclude
            .iter()
            .any(|glob| glob == "kmod-*"));
    }

    #[test]
    fn an_ordinary_transaction_excludes_every_platform_prefix() {
        let plan = ApplyPlan::ordinary();
        assert!(plan.packages.is_empty(), "ordinary means everything else");
        assert_eq!(plan.exclude.len(), PLATFORM_PREFIXES.len());
        for prefix in PLATFORM_PREFIXES {
            assert!(plan.exclude.contains(&format!("{prefix}*")));
        }
    }

    /// The platform transaction names its packages rather than globbing, so
    /// what runs is what the operator was shown.
    #[test]
    fn a_platform_transaction_names_exactly_what_was_shown() {
        let updates = vec![
            Update::new("kernel-core", "6.12.0-212.el10", "baseos"),
            Update::new("kmod-zfs-2.3", "2.3.4-1.el10", "zfs-2.3-kmod"),
        ];
        let plan = ApplyPlan::platform(&updates);
        assert_eq!(plan.packages, vec!["kernel-core", "kmod-zfs-2.3"]);
        assert!(plan.exclude.is_empty());
    }

    #[test]
    fn a_reboot_is_needed_only_when_the_running_kernel_is_not_the_newest() {
        let same = KernelState {
            running: "6.12.0-211.7.3.el10_2.x86_64".into(),
            newest: Some("6.12.0-211.7.3.el10_2.x86_64".into()),
        };
        assert!(!RebootState::from_kernel(same).required);

        let stale = KernelState {
            running: "6.12.0-211.7.3.el10_2.x86_64".into(),
            newest: Some("6.12.0-212.el10.x86_64".into()),
        };
        let state = RebootState::from_kernel(stale);
        assert!(state.required);
        assert!(state.reason.unwrap().contains("6.12.0-212.el10.x86_64"));

        // Nothing known is not evidence of a pending reboot.
        let unknown = KernelState {
            running: "6.12.0-211.7.3.el10_2.x86_64".into(),
            newest: None,
        };
        assert!(!RebootState::from_kernel(unknown).required);
    }

    #[test]
    fn a_platform_plan_is_offerable_only_when_it_resolves() {
        assert!(!PlatformPlan::none().offerable());
        let waiting = PlatformPlan {
            updates: vec![Update::new("kernel-core", "6.12.0-212.el10", "baseos")],
            resolves: true,
            detail: None,
        };
        assert!(waiting.offerable());
        let blocked = PlatformPlan {
            resolves: false,
            detail: Some("nothing provides kmod-zfs for kernel 6.12.0-212".into()),
            ..waiting
        };
        assert!(blocked.pending());
        assert!(!blocked.offerable());
    }
}
