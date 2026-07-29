//! The one entry point the control plane calls.
//!
//! Two verbs and a reading: check what is waiting, install some of it, and ask
//! whether the node is running the kernel it has. Everything the console shows
//! comes out of [`UpdateView`], and everything it can press goes in through
//! [`UpdateService::apply`].

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backend::UpdateBackend;
use crate::error::{Result, UpdateError};
use crate::model::{ApplyPlan, ApplyReport, PlatformPlan, RebootState, Update, UpdateKind};

/// How many of each kind are waiting — the numbers the console puts on a badge
/// without reading the table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// Lumen's own packages.
    pub lumen: usize,
    /// Everything else in userland.
    pub other: usize,
    /// Of the above, how many an advisory marks as security fixes.
    pub security: usize,
    /// Platform packages waiting, whether or not they can move.
    pub platform: usize,
}

/// Everything the Updates page renders.
///
/// Read back as well as written: a cluster-wide read asks every member this
/// question and assembles the answers side by side, so the coordinator has to
/// be able to parse a peer's view off the wire. The JSON is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateView {
    pub node: String,
    /// When the repositories were last asked, in unix seconds. `None` before
    /// the first check of this daemon's life.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    /// The ordinary updates: everything that is not the platform set. These
    /// are what the plain button installs.
    pub updates: Vec<Update>,
    /// The kernel and its kABI-tracking modules, and whether they can move.
    pub platform: PlatformPlan,
    pub reboot: RebootState,
    pub counts: Counts,
    /// Why the last check failed, when it did. Carried rather than returned as
    /// an error so the page still renders the reboot state and the previous
    /// answer — a node that cannot reach its repositories is a node whose
    /// operator especially wants to see the rest of the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UpdateView {
    /// Nothing known yet: what the page shows before the first check finishes.
    fn empty(node: &str, reboot: RebootState) -> Self {
        Self {
            node: node.to_string(),
            checked_at: None,
            updates: Vec::new(),
            platform: PlatformPlan::none(),
            reboot,
            counts: Counts::default(),
            error: None,
        }
    }

    /// Anything at all waiting, of either sort.
    pub fn pending(&self) -> bool {
        !self.updates.is_empty() || self.platform.pending()
    }
}

/// What the console asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyRequest {
    /// Install the platform set — kernel, kernel modules, ZFS, DRBD — instead
    /// of the ordinary updates. The two are never combined: they are different
    /// decisions with different consequences, and a single button that
    /// sometimes moved the kernel would be the exact failure this domain
    /// exists to prevent.
    pub platform: bool,
    /// Required for the platform set. Named the way every other
    /// acknowledgement in this appliance is: as the sentence the operator is
    /// agreeing to.
    pub i_understand_the_kernel_moves: bool,
}

pub struct UpdateService {
    backend: Arc<dyn UpdateBackend>,
    node: String,
    /// The last answer, so the console can render a badge on every page load
    /// without refreshing repository metadata over the network each time.
    /// In memory only — see the crate note on there being no database.
    last: Mutex<Option<UpdateView>>,
}

impl UpdateService {
    pub fn new(backend: Arc<dyn UpdateBackend>, node: impl Into<String>) -> Self {
        Self {
            backend,
            node: node.into(),
            last: Mutex::new(None),
        }
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    /// The last answer, or a view that says nothing has been asked yet.
    ///
    /// Never blocks on the network. The reboot state is read fresh even so,
    /// because it costs one `uname` and a package-database query and it is the
    /// one fact on the page an operator may be acting on right now.
    pub async fn view(&self) -> Result<UpdateView> {
        let reboot = self.reboot_state().await;
        let mut view = match self.last.lock().expect("update view lock").clone() {
            Some(view) => view,
            None => UpdateView::empty(&self.node, reboot.clone()),
        };
        view.reboot = reboot;
        Ok(view)
    }

    /// Ask the repositories, and remember the answer.
    ///
    /// A failure is returned *and* recorded: the caller decides whether an
    /// explicit "Check now" should surface it as an error, while the stored
    /// view keeps it so a later page load still explains the stale timestamp.
    pub async fn check(&self) -> Result<UpdateView> {
        let reboot = self.reboot_state().await;
        let updates = match self.backend.check().await {
            Ok(updates) => updates,
            Err(err) => {
                let mut view = UpdateView::empty(&self.node, reboot);
                view.error = Some(err.to_string());
                self.remember(view);
                return Err(err);
            }
        };

        let (platform, ordinary): (Vec<Update>, Vec<Update>) = updates
            .into_iter()
            .partition(|update| update.kind == UpdateKind::Platform);

        // The gate: ask the solver whether the platform set can move as one
        // transaction. Only when there is something to ask about — an empty
        // set trivially resolves and asking would be a pointless second call.
        let plan = if platform.is_empty() {
            PlatformPlan::none()
        } else {
            let names: Vec<String> = platform.iter().map(|u| u.name.clone()).collect();
            match self.backend.resolve(&names).await {
                Ok(resolution) => PlatformPlan {
                    updates: platform,
                    resolves: resolution.ok,
                    detail: (!resolution.ok).then_some(resolution.detail),
                },
                // A dry run that could not be performed is not a dry run that
                // succeeded. Blocked, with the reason, is the safe direction:
                // the cost of being wrong here is an unbootable node.
                Err(err) => PlatformPlan {
                    updates: platform,
                    resolves: false,
                    detail: Some(format!(
                        "Could not work out whether these can be installed together: {err}"
                    )),
                },
            }
        };

        let view = UpdateView {
            node: self.node.clone(),
            checked_at: Some(now()),
            counts: counts(&ordinary, &plan),
            updates: ordinary,
            platform: plan,
            reboot,
            error: None,
        };
        self.remember(view.clone());
        Ok(view)
    }

    /// Install updates.
    ///
    /// Always re-checks first. The guards below are only worth having if they
    /// run against what is true now rather than against whatever the console
    /// was last shown — a page left open overnight must not be able to
    /// authorize a transaction nobody looked at.
    pub async fn apply(&self, request: ApplyRequest) -> Result<ApplyReport> {
        let view = self.check().await?;

        let plan = if request.platform {
            if !view.platform.pending() {
                return Err(UpdateError::NotFound(
                    "There is no kernel or storage-module update waiting.".to_string(),
                ));
            }
            if !view.platform.resolves {
                return Err(UpdateError::conflict(format!(
                    "These cannot be installed together yet, so Lumen will not install any of \
                     them. The kernel and the storage modules built against it have to move as \
                     one set — installing the kernel alone is what leaves a node unable to \
                     import its pool at the next restart. The package manager said: {}",
                    view.platform
                        .detail
                        .as_deref()
                        .unwrap_or("no reason was given")
                )));
            }
            if !request.i_understand_the_kernel_moves {
                return Err(UpdateError::conflict(
                    "Installing these replaces the kernel and the storage modules built against \
                     it. The node keeps running the old kernel until it is restarted. Acknowledge \
                     \"i_understand_the_kernel_moves\" to go ahead."
                        .to_string(),
                ));
            }
            ApplyPlan::platform(&view.platform.updates)
        } else {
            if view.updates.is_empty() {
                return Err(UpdateError::NotFound(
                    "There are no updates waiting to be installed.".to_string(),
                ));
            }
            ApplyPlan::ordinary()
        };

        let report = self.backend.apply(&plan).await?;

        // Re-read rather than assume. What is waiting now, and whether the
        // node is running the kernel it has, are both questions the package
        // database answers — and the transaction may have done more or less
        // than was asked.
        if let Err(err) = self.check().await {
            tracing::warn!("the update finished but the node could not be re-checked: {err}");
        }
        Ok(report)
    }

    /// Whether this node is running the kernel it has installed.
    ///
    /// Never fails: a package database that cannot be read produces "nothing
    /// known", which claims no restart is outstanding rather than inventing
    /// one. The running release itself comes from `/proc` and is always there.
    async fn reboot_state(&self) -> RebootState {
        match self.backend.kernel().await {
            Ok(kernel) => RebootState::from_kernel(kernel),
            Err(err) => {
                tracing::warn!("could not read this node's kernel state: {err}");
                RebootState::from_kernel(crate::model::KernelState {
                    running: String::new(),
                    newest: None,
                })
            }
        }
    }

    fn remember(&self, view: UpdateView) {
        *self.last.lock().expect("update view lock") = Some(view);
    }
}

fn counts(ordinary: &[Update], platform: &PlatformPlan) -> Counts {
    Counts {
        lumen: ordinary
            .iter()
            .filter(|u| u.kind == UpdateKind::Lumen)
            .count(),
        other: ordinary
            .iter()
            .filter(|u| u.kind == UpdateKind::Other)
            .count(),
        security: ordinary.iter().filter(|u| u.security).count()
            + platform.updates.iter().filter(|u| u.security).count(),
        platform: platform.updates.len(),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockUpdates;

    fn waiting() -> Vec<Update> {
        vec![
            Update::new("lumen-controlplane", "0.4.0-1.el10", "lumen"),
            Update::new("libvirt", "11.0.0-2.el10", "appstream"),
            Update::new("kernel-core", "6.12.0-212.el10", "baseos"),
            Update::new("kmod-zfs-2.3", "2.3.4-1.el10", "zfs-2.3-kmod"),
        ]
    }

    fn service(mock: MockUpdates) -> (Arc<MockUpdates>, UpdateService) {
        let mock = Arc::new(mock);
        let service = UpdateService::new(mock.clone(), "node-a");
        (mock, service)
    }

    #[tokio::test]
    async fn a_check_separates_the_platform_set_from_everything_else() {
        let (_, service) = service(MockUpdates::new().with_updates(waiting()));
        let view = service.check().await.unwrap();

        let ordinary: Vec<&str> = view.updates.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(ordinary, vec!["lumen-controlplane", "libvirt"]);
        let platform: Vec<&str> = view
            .platform
            .updates
            .iter()
            .map(|u| u.name.as_str())
            .collect();
        assert_eq!(platform, vec!["kernel-core", "kmod-zfs-2.3"]);

        assert_eq!(view.counts.lumen, 1);
        assert_eq!(view.counts.other, 1);
        assert_eq!(view.counts.platform, 2);
        assert!(view.platform.offerable());
        assert!(view.checked_at.is_some());
    }

    /// The property the whole domain exists for: an ordinary update never
    /// hands the package manager a transaction that could move the kernel.
    #[tokio::test]
    async fn an_ordinary_update_excludes_the_platform_set() {
        let (mock, service) = service(MockUpdates::new().with_updates(waiting()));
        service
            .apply(ApplyRequest::default())
            .await
            .expect("ordinary updates install");

        let plans = mock.applied();
        assert_eq!(plans.len(), 1);
        assert!(plans[0].packages.is_empty());
        assert!(plans[0].exclude.contains(&"kernel*".to_string()));
        assert!(plans[0].exclude.contains(&"kmod-*".to_string()));

        // And the kernel is still waiting afterwards.
        let view = service.view().await.unwrap();
        assert_eq!(view.counts.platform, 2);
        assert!(view.updates.is_empty());
    }

    #[tokio::test]
    async fn the_platform_set_needs_an_acknowledgement() {
        let (mock, service) = service(MockUpdates::new().with_updates(waiting()));
        let err = service
            .apply(ApplyRequest {
                platform: true,
                i_understand_the_kernel_moves: false,
            })
            .await
            .unwrap_err();

        assert!(matches!(err, UpdateError::Conflict(_)), "{err:?}");
        assert!(err.to_string().contains("restarted"), "{err}");
        assert!(mock.applied().is_empty(), "nothing may have been run");
    }

    /// The gate proper: a kernel whose modules have not caught up is refused
    /// outright, and the refusal carries the solver's own words.
    #[tokio::test]
    async fn a_platform_set_that_does_not_resolve_is_refused_even_when_acknowledged() {
        let (mock, service) = service(
            MockUpdates::new()
                .with_updates(waiting())
                .blocking_resolution("nothing provides kernel-uname-r = 6.12.0-212.el10.x86_64"),
        );

        let view = service.check().await.unwrap();
        assert!(view.platform.pending());
        assert!(!view.platform.resolves);
        assert!(!view.platform.offerable(), "the console must not offer it");

        let err = service
            .apply(ApplyRequest {
                platform: true,
                i_understand_the_kernel_moves: true,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nothing provides"), "{err}");
        assert!(mock.applied().is_empty(), "nothing may have been run");
    }

    /// A dry run that could not be performed is treated as a refusal, not as
    /// permission. The cost of being wrong the other way is an unbootable node.
    #[tokio::test]
    async fn a_resolution_that_could_not_be_performed_blocks_the_platform_set() {
        struct Broken(MockUpdates);

        #[async_trait::async_trait]
        impl UpdateBackend for Broken {
            async fn check(&self) -> Result<Vec<Update>> {
                self.0.check().await
            }
            async fn resolve(&self, _: &[String]) -> Result<crate::model::Resolution> {
                Err(UpdateError::backend(anyhow::anyhow!("the solver crashed")))
            }
            async fn apply(&self, plan: &ApplyPlan) -> Result<ApplyReport> {
                self.0.apply(plan).await
            }
            async fn kernel(&self) -> Result<crate::model::KernelState> {
                self.0.kernel().await
            }
        }

        let backend = Arc::new(Broken(MockUpdates::new().with_updates(waiting())));
        let service = UpdateService::new(backend, "node-a");
        let view = service.check().await.unwrap();
        assert!(!view.platform.resolves);
        assert!(view.platform.detail.unwrap().contains("the solver crashed"));
    }

    #[tokio::test]
    async fn installing_the_platform_set_leaves_a_restart_outstanding() {
        let (mock, service) = service(
            MockUpdates::new()
                .with_updates(waiting())
                .landing_on_kernel("6.12.0-212.el10.x86_64"),
        );

        let before = service.check().await.unwrap();
        assert!(!before.reboot.required);

        service
            .apply(ApplyRequest {
                platform: true,
                i_understand_the_kernel_moves: true,
            })
            .await
            .expect("the platform set installs");

        // Named exactly, not globbed.
        assert_eq!(
            mock.applied()[0].packages,
            vec!["kernel-core", "kmod-zfs-2.3"]
        );

        let after = service.view().await.unwrap();
        assert!(after.reboot.required);
        assert!(after.reboot.reason.unwrap().contains("6.12.0-212"));
        // Nothing here restarted anything.
        assert_eq!(after.reboot.kernel.running, "6.12.0-211.7.3.el10_2.x86_64");
    }

    #[tokio::test]
    async fn there_is_nothing_to_do_when_nothing_is_waiting() {
        let (mock, service) = service(MockUpdates::new());
        let err = service.apply(ApplyRequest::default()).await.unwrap_err();
        assert!(matches!(err, UpdateError::NotFound(_)), "{err:?}");
        assert!(mock.applied().is_empty());

        let err = service
            .apply(ApplyRequest {
                platform: true,
                i_understand_the_kernel_moves: true,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, UpdateError::NotFound(_)), "{err:?}");
    }

    /// The apply guards run against a fresh check, not against whatever the
    /// console was last shown.
    #[tokio::test]
    async fn applying_re_checks_before_it_decides() {
        let (mock, service) = service(MockUpdates::new().with_updates(waiting()));
        service.check().await.unwrap();
        assert_eq!(mock.checks(), 1);
        service.apply(ApplyRequest::default()).await.unwrap();
        // One for the guard, one for the re-read afterwards.
        assert_eq!(mock.checks(), 3);
    }

    #[tokio::test]
    async fn a_failed_check_is_reported_and_remembered() {
        struct Offline;

        #[async_trait::async_trait]
        impl UpdateBackend for Offline {
            async fn check(&self) -> Result<Vec<Update>> {
                Err(UpdateError::backend(anyhow::anyhow!(
                    "cannot reach lumen.quartz.systems"
                )))
            }
            async fn resolve(&self, _: &[String]) -> Result<crate::model::Resolution> {
                unreachable!("never reached when the check failed")
            }
            async fn apply(&self, _: &ApplyPlan) -> Result<ApplyReport> {
                unreachable!("never reached when the check failed")
            }
            async fn kernel(&self) -> Result<crate::model::KernelState> {
                Ok(crate::model::KernelState {
                    running: "6.12.0-211.7.3.el10_2.x86_64".into(),
                    newest: Some("6.12.0-212.el10.x86_64".into()),
                })
            }
        }

        let service = UpdateService::new(Arc::new(Offline), "node-a");
        let err = service.check().await.unwrap_err();
        assert!(err.to_string().contains("lumen.quartz.systems"));

        // The page still renders, still carries the reason, and still reports
        // the outstanding restart it read from the node itself.
        let view = service.view().await.unwrap();
        assert!(view.error.unwrap().contains("lumen.quartz.systems"));
        assert!(view.reboot.required);
    }
}
