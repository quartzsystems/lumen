//! End-to-end tests for updating every node in the environment, over the real
//! router with a scripted peer channel standing in for the other members.
//!
//! Nothing here touches the packages on the machine running the tests: this
//! node's backend is `lumen_update::MockUpdates`, and the peers are
//! [`FakePeers`], which models what each member has waiting and what happens
//! when it is asked to install it.
//!
//! The properties these tests are about are the ones the walk exists for:
//!
//! - it visits members **one at a time**, and this node **last**;
//! - it **stops** at the first member that fails, rather than turning one node
//!   that could not update into all of them;
//! - a member whose control plane restarts mid-transaction — which is what
//!   installing Lumen's own packages does — is a **success**, decided from
//!   what its package database says afterwards rather than from a progress
//!   feed that died with the process;
//! - the kernel is never installed this way.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use lumen_cluster::backend::mock::{membership_of, MockBackend as ClusterMockBackend};
use lumen_cluster::{ClusterError, ClusterService, EnvironmentNode};
use lumen_controlplane::cluster_updates::NodeUpdates;
use lumen_controlplane::config::Config;
use lumen_controlplane::inventory::{InventoryPeers, NodeInventory};
use lumen_controlplane::realm::{AuthFailure, Realm, RealmKind, RealmRegistry};
use lumen_controlplane::updates::{TransactionKind, UpdateProgress};
use lumen_controlplane::{app, AppState};
use lumen_net::NetworkService;
use lumen_sys::backend::mock::MockPower;
use lumen_sys::exec::MockExec;
use lumen_sys::SysService;
use lumen_update::{
    Counts, KernelState, MockUpdates, PlatformPlan, RebootState, Update, UpdateService, UpdateView,
};
use lumen_virt::VirtService;
use lumen_zfs::backend::mock::MockBackend as MockZfsBackend;
use lumen_zfs::StorageService;

const TICKET_SECRET: &[u8] = b"test-secret-test-secret-test-secret!";

/// This node. Sorts last among the three names on purpose — so a test that
/// sees it visited last has not merely watched an alphabetical sort do the
/// right thing by accident.
const LOCAL: &str = "alpha-3";

// --- the scripted members ----------------------------------------------------

/// What one peer does when it is told to install.
#[derive(Debug, Clone)]
enum Outcome {
    /// Installs what it had waiting and reports it, the ordinary case.
    Installs,
    /// The package manager fails, and says why.
    Fails(String),
    /// Installs, and its control plane restarts before it can report —
    /// exactly what `%systemd_postun_with_restart` does when the transaction
    /// includes `lumen-controlplane`. It stops answering for two calls, then
    /// comes back with no transaction to report at all.
    RestartsAfterInstalling {
        /// Whether packages are still waiting when it comes back. `true` is
        /// the transaction having genuinely not finished, which must not be
        /// read as success just because the feed is gone.
        leaves_waiting: bool,
    },
    /// Never answers anything.
    Gone,
}

struct FakeMember {
    name: String,
    waiting: Mutex<Vec<Update>>,
    /// The kernel and the modules built against it, and whether that member's
    /// package manager says they can move together.
    platform: Mutex<Vec<Update>>,
    platform_resolves: bool,
    /// Whether it is running a kernel it no longer has — set by installing the
    /// platform set, cleared by restarting.
    stale_kernel: AtomicBool,
    progress: Mutex<Option<UpdateProgress>>,
    outcome: Outcome,
    /// Calls that will fail before this member answers again.
    down_for: AtomicUsize,
    applies: AtomicUsize,
    checks: AtomicUsize,
    // --- the rolling half ---
    /// Machines that will not move off it when it is drained.
    stranded: Vec<lumen_controlplane::maintenance::Stranded>,
    drain: Mutex<Option<lumen_controlplane::maintenance::MaintenanceProgress>>,
    in_service: AtomicBool,
    drains: AtomicUsize,
    restarts: AtomicUsize,
    returns: AtomicUsize,
    /// The drain refuses outright, as a member whose cluster cannot spare it
    /// would.
    refuses_drain: Option<String>,
}

impl FakeMember {
    fn new(name: &str, waiting: Vec<Update>, outcome: Outcome) -> Self {
        FakeMember {
            name: name.to_string(),
            waiting: Mutex::new(waiting),
            platform: Mutex::new(Vec::new()),
            platform_resolves: true,
            stale_kernel: AtomicBool::new(false),
            progress: Mutex::new(None),
            outcome,
            down_for: AtomicUsize::new(0),
            applies: AtomicUsize::new(0),
            checks: AtomicUsize::new(0),
            stranded: Vec::new(),
            drain: Mutex::new(None),
            in_service: AtomicBool::new(true),
            drains: AtomicUsize::new(0),
            restarts: AtomicUsize::new(0),
            returns: AtomicUsize::new(0),
            refuses_drain: None,
        }
    }

    /// A member with a kernel waiting, which is what makes a rolling update
    /// restart it.
    fn with_platform(mut self, resolves: bool) -> Self {
        self.platform = Mutex::new(vec![
            Update::new("kernel-core", "6.12.0-212.el10", "baseos"),
            Update::new("kmod-zfs-2.3", "2.3.4-1.el10", "zfs-2.3-kmod"),
        ]);
        self.platform_resolves = resolves;
        self
    }

    /// A machine that will not move when this member is drained.
    fn stranding(mut self, vmid: u32, name: &str, reason: &str) -> Self {
        self.stranded
            .push(lumen_controlplane::maintenance::Stranded {
                vmid,
                name: name.to_string(),
                reason: reason.to_string(),
            });
        self
    }

    fn refusing_drain(mut self, reason: &str) -> Self {
        self.refuses_drain = Some(reason.to_string());
        self
    }

    fn view(&self) -> UpdateView {
        let waiting = self.waiting.lock().unwrap().clone();
        let platform = self.platform.lock().unwrap().clone();
        let stale = self.stale_kernel.load(Ordering::SeqCst);
        UpdateView {
            node: self.name.clone(),
            checked_at: Some(1_800_000_000),
            counts: Counts {
                lumen: 0,
                other: waiting.len(),
                security: 0,
                platform: platform.len(),
            },
            updates: waiting,
            platform: if platform.is_empty() {
                PlatformPlan::none()
            } else {
                PlatformPlan {
                    updates: platform,
                    resolves: self.platform_resolves,
                    detail: (!self.platform_resolves).then(|| {
                        "nothing provides kernel-uname-r = 6.12.0-212.el10.x86_64".to_string()
                    }),
                }
            },
            reboot: RebootState::from_kernel(KernelState {
                running: "6.12.0-211.el10.x86_64".into(),
                newest: Some(
                    if stale {
                        "6.12.0-212.el10.x86_64"
                    } else {
                        "6.12.0-211.el10.x86_64"
                    }
                    .into(),
                ),
            }),
            error: None,
        }
    }

    fn answer(&self) -> Result<NodeUpdates, ClusterError> {
        if matches!(self.outcome, Outcome::Gone) {
            return Err(ClusterError::Conflict(format!(
                "{}: could not connect",
                self.name
            )));
        }
        if self.down_for.load(Ordering::SeqCst) > 0 {
            self.down_for.fetch_sub(1, Ordering::SeqCst);
            return Err(ClusterError::Conflict(format!(
                "{}: connection refused",
                self.name
            )));
        }
        Ok(NodeUpdates {
            view: self.view(),
            progress: self.progress.lock().unwrap().clone(),
        })
    }
}

/// A peer channel that answers from a scripted set of members.
///
/// It stands in for the HTTP one at the same seam the real code uses, so what
/// the tests exercise is the walk's own logic — the order, the waiting, the
/// stopping — rather than a socket.
struct FakePeers {
    members: Vec<Arc<FakeMember>>,
    /// Distinct `started_at` values, so the walk can tell the transaction it
    /// started from one that was already running there.
    clock: AtomicUsize,
}

impl FakePeers {
    fn new(members: Vec<Arc<FakeMember>>) -> Self {
        FakePeers {
            members,
            clock: AtomicUsize::new(1_800_000_100),
        }
    }

    fn member(&self, node: &EnvironmentNode) -> Result<&Arc<FakeMember>, ClusterError> {
        self.members
            .iter()
            .find(|member| member.name == node.name)
            .ok_or_else(|| ClusterError::NotFound(format!("no such member {}", node.name)))
    }
}

#[async_trait]
impl InventoryPeers for FakePeers {
    async fn fetch(&self, _node: &EnvironmentNode) -> Result<NodeInventory, ClusterError> {
        unreachable!("the update tests never ask for an inventory")
    }

    async fn create_pool(
        &self,
        _node: &EnvironmentNode,
        _request: &lumen_zfs::PoolCreate,
    ) -> Result<(), ClusterError> {
        unreachable!("the update tests never build a pool")
    }

    async fn wipe_disk(
        &self,
        _node: &EnvironmentNode,
        _disk: &str,
    ) -> Result<lumen_zfs::BlockDevice, ClusterError> {
        unreachable!("the update tests never clear a disk")
    }

    async fn updates(&self, node: &EnvironmentNode) -> Result<NodeUpdates, ClusterError> {
        self.member(node)?.answer()
    }

    async fn check_updates(&self, node: &EnvironmentNode) -> Result<NodeUpdates, ClusterError> {
        let member = self.member(node)?;
        let answer = member.answer();
        if answer.is_ok() {
            member.checks.fetch_add(1, Ordering::SeqCst);
        }
        answer
    }

    async fn apply_updates(
        &self,
        node: &EnvironmentNode,
        platform: bool,
        by: &str,
    ) -> Result<UpdateProgress, ClusterError> {
        let member = self.member(node)?;
        if platform {
            assert!(
                member.platform_resolves,
                "a platform set that will not resolve must never be handed to a member"
            );
            assert!(
                !member.in_service.load(Ordering::SeqCst),
                "the kernel must not be installed on a member that is still in service"
            );
        }
        if matches!(member.outcome, Outcome::Gone) {
            return Err(ClusterError::Conflict(format!(
                "{}: could not connect",
                member.name
            )));
        }
        member.applies.fetch_add(1, Ordering::SeqCst);

        let started_at = self.clock.fetch_add(1, Ordering::SeqCst) as u64;
        let running = UpdateProgress {
            node: member.name.clone(),
            kind: if platform {
                TransactionKind::Platform
            } else {
                TransactionKind::Ordinary
            },
            phase: lumen_cluster::join::WorkflowPhase::Running,
            started_at,
            finished_at: None,
            by: by.to_string(),
            changed: Vec::new(),
            log: None,
            error: None,
            reboot: None,
        };

        let set = if platform {
            &member.platform
        } else {
            &member.waiting
        };
        let installed: Vec<String> = set
            .lock()
            .unwrap()
            .iter()
            .map(|update| update.name.clone())
            .collect();

        match &member.outcome {
            Outcome::Installs => {
                set.lock().unwrap().clear();
                // A platform transaction leaves a kernel installed and not
                // running, which is the whole reason the node is restarted.
                if platform {
                    member.stale_kernel.store(true, Ordering::SeqCst);
                }
                *member.progress.lock().unwrap() = Some(UpdateProgress {
                    phase: lumen_cluster::join::WorkflowPhase::Complete,
                    finished_at: Some(started_at + 30),
                    changed: installed,
                    reboot: Some(member.view().reboot),
                    ..running.clone()
                });
            }
            Outcome::Fails(reason) => {
                *member.progress.lock().unwrap() = Some(UpdateProgress {
                    phase: lumen_cluster::join::WorkflowPhase::Failed,
                    finished_at: Some(started_at + 5),
                    error: Some(reason.clone()),
                    ..running.clone()
                });
            }
            Outcome::RestartsAfterInstalling { leaves_waiting } => {
                if !leaves_waiting {
                    set.lock().unwrap().clear();
                }
                // The record died with the process that held it, and the
                // member stops answering while it comes back up.
                *member.progress.lock().unwrap() = None;
                member.down_for.store(2, Ordering::SeqCst);
            }
            Outcome::Gone => unreachable!("handled above"),
        }

        Ok(running)
    }

    async fn enter_maintenance(
        &self,
        node: &EnvironmentNode,
        _by: &str,
    ) -> Result<lumen_controlplane::maintenance::MaintenanceProgress, ClusterError> {
        let member = self.member(node)?;
        if let Some(reason) = member.refuses_drain.as_deref() {
            return Err(ClusterError::Conflict(reason.to_string()));
        }
        member.drains.fetch_add(1, Ordering::SeqCst);
        member.in_service.store(false, Ordering::SeqCst);

        // Out of service at once; the machines move while the caller polls.
        let progress = lumen_controlplane::maintenance::MaintenanceProgress {
            node: member.name.clone(),
            cluster: "alpha".into(),
            phase: lumen_cluster::join::WorkflowPhase::Running,
            error: None,
            steps: Vec::new(),
            stranded: Vec::new(),
        };
        *member.drain.lock().unwrap() =
            Some(lumen_controlplane::maintenance::MaintenanceProgress {
                phase: lumen_cluster::join::WorkflowPhase::Complete,
                stranded: member.stranded.clone(),
                ..progress.clone()
            });
        Ok(progress)
    }

    async fn drain_progress(
        &self,
        node: &EnvironmentNode,
    ) -> Result<Option<lumen_controlplane::maintenance::MaintenanceProgress>, ClusterError> {
        Ok(self.member(node)?.drain.lock().unwrap().clone())
    }

    async fn exit_maintenance(
        &self,
        node: &EnvironmentNode,
        _by: &str,
    ) -> Result<(), ClusterError> {
        let member = self.member(node)?;
        member.returns.fetch_add(1, Ordering::SeqCst);
        member.in_service.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn restart(&self, node: &EnvironmentNode) -> Result<(), ClusterError> {
        let member = self.member(node)?;
        assert!(
            !member.in_service.load(Ordering::SeqCst),
            "a member must be out of service before it is restarted"
        );
        member.restarts.fetch_add(1, Ordering::SeqCst);
        // It goes away, comes back on the kernel it installed, and has no
        // transaction to report — a fresh process.
        member.stale_kernel.store(false, Ordering::SeqCst);
        *member.progress.lock().unwrap() = None;
        member.down_for.store(2, Ordering::SeqCst);
        Ok(())
    }
}

// --- the harness -------------------------------------------------------------

struct MockRealm;

#[async_trait]
impl Realm for MockRealm {
    fn id(&self) -> &str {
        "lumen"
    }
    fn name(&self) -> &str {
        "Lumen"
    }
    fn kind(&self) -> RealmKind {
        RealmKind::Builtin
    }
    async fn authenticate(&self, _username: &str, password: &str) -> Result<(), AuthFailure> {
        if password == "correct-horse" {
            Ok(())
        } else {
            Err(AuthFailure::Denied)
        }
    }
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    router: axum::Router,
    /// This node's own package manager.
    local: Arc<MockUpdates>,
    members: Vec<Arc<FakeMember>>,
    cookie: String,
    _dir: TempDir,
}

fn waiting() -> Vec<Update> {
    vec![
        Update::new("lumen-controlplane", "0.4.0-1.el10", "lumen"),
        Update::new("libvirt", "11.0.0-2.el10", "appstream"),
    ]
}

/// An environment of three: two scripted peers and this node.
async fn harness(tag: &str, peers: Vec<Arc<FakeMember>>, local: MockUpdates) -> Harness {
    let dir = TempDir(std::env::temp_dir().join(format!(
        "lumen-cp-cupd-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    )));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();

    let mut config = Config::from_env();
    config.webui_dir = std::env::temp_dir().join("lumen-webui-none");
    config.no_tls = true;
    config.session_ttl_secs = 3600;
    config.update_check_secs = 0;

    let mut names: Vec<(&str, Option<&str>)> = peers
        .iter()
        .map(|member| (member.name.as_str(), Some("alpha")))
        .collect();
    names.push((LOCAL, Some("alpha")));

    let exec = Arc::new(MockExec::new());
    let sys = Arc::new(SysService::new(
        Arc::new(MockPower::appliance()),
        exec.clone(),
    ));
    let storage = Arc::new(StorageService::new(Arc::new(MockZfsBackend::appliance())));
    let network = Arc::new(NetworkService::new(
        Arc::new(lumen_net::backend::mock::MockBackend::appliance()),
        &dir.0.join("net"),
        60,
    ));
    let virt = Arc::new(VirtService::new(
        Arc::new(lumen_virt::backend::mock::MockBackend::appliance()),
        storage.clone(),
        network.clone(),
        Arc::new(lumen_drbd::MockVmVolumes::standalone()),
    ));
    // A formed cluster with every member online, so the rolling update's
    // "has it rejoined?" check has a real answer to read rather than an
    // unreachable cluster it would have to time out on.
    let cluster_backend = ClusterMockBackend::appliance();
    cluster_backend.register_cluster(lumen_cluster::backend::mock::formed_cluster(
        "alpha",
        &names.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
    ));
    let cluster = Arc::new(
        ClusterService::new(
            Arc::new(cluster_backend),
            Arc::new(lumen_cluster::MockPeers::new()),
            network.clone(),
            &dir.0,
            "test",
        )
        .with_node(LOCAL)
        .with_environment(&membership_of(&names)),
    );
    let drbd = Arc::new(lumen_drbd::DrbdService::new(
        Arc::new(lumen_drbd::backend::mock::MockBackend::appliance()),
        Arc::new(lumen_drbd::MockVolumePeers::new()),
        cluster.clone(),
        storage.clone(),
    ));

    let local = Arc::new(local);
    let router = app(Arc::new(AppState {
        config,
        jwt_secret: lumen_controlplane::security::session_secret(TICKET_SECRET.to_vec()),
        tls: None,
        realms: RealmRegistry::new().register(Box::new(MockRealm)),
        sys,
        network,
        storage,
        virt,
        cluster,
        peers: Arc::new(FakePeers::new(peers.clone())),
        drbd,
        pool: lumen_controlplane::pool::PoolPresence::Absent,
        updates: Arc::new(UpdateService::new(local.clone(), LOCAL)),
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
        drain: Default::default(),
        update_job: Default::default(),
        roll: Default::default(),
        pool_deploy: std::sync::Arc::new(lumen_pool::PoolDeploy::new(
            lumen_sys::exec::MockExec::working(),
        )),
        pool_peers: std::sync::Arc::new(lumen_controlplane::inventory::NoPeers),
        pool_job: Default::default(),
    }));

    let response = router
        .clone()
        .oneshot(
            Request::post("/api/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"username":"alice","password":"correct-horse"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .expect("a session cookie")
        .to_string();

    Harness {
        router,
        local,
        members: peers,
        cookie,
        _dir: dir,
    }
}

impl Harness {
    async fn send(&self, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    async fn get(&self, path: &str) -> (StatusCode, serde_json::Value) {
        self.send(
            Request::get(path)
                .header(header::COOKIE, &self.cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post(&self, path: &str, body: &str) -> (StatusCode, serde_json::Value) {
        self.send(
            Request::post(path)
                .header(header::COOKIE, &self.cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    /// Wait for the walk to finish. The scripted members answer at once, but
    /// the walk is a task with a poll interval, so the test has to let it run.
    async fn settled(&self) -> serde_json::Value {
        for _ in 0..600 {
            let (_, progress) = self.get("/api/environment/updates/progress").await;
            if progress
                .get("phase")
                .and_then(|phase| phase.as_str())
                .is_some_and(|phase| phase != "running")
            {
                return progress;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the walk never finished");
    }

    fn member(&self, name: &str) -> &Arc<FakeMember> {
        self.members
            .iter()
            .find(|member| member.name == name)
            .expect("a scripted member")
    }
}

fn steps(progress: &serde_json::Value) -> Vec<(String, String)> {
    progress["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            (
                step["node"].as_str().unwrap().to_string(),
                step["state"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

// --- the read ----------------------------------------------------------------

#[tokio::test]
async fn the_cluster_view_carries_every_member_and_rolls_them_up() {
    let h = harness(
        "view",
        vec![
            Arc::new(FakeMember::new("alpha-1", waiting(), Outcome::Installs)),
            Arc::new(FakeMember::new("alpha-2", Vec::new(), Outcome::Installs)),
        ],
        MockUpdates::new().with_updates(waiting()),
    )
    .await;

    // This node has to have checked once for its own row to carry anything;
    // the read deliberately never asks the repositories itself.
    h.post("/api/system/updates/check", "{}").await;

    let (status, view) = h.get("/api/environment/updates").await;
    assert_eq!(status, StatusCode::OK);

    let members: Vec<&str> = view["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member["node"].as_str().unwrap())
        .collect();
    assert_eq!(members, vec!["alpha-1", "alpha-2", LOCAL]);

    // Exactly one row is this node's, and it is the only one the console may
    // treat as local.
    let local_rows = view["members"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|member| member["local"] == true)
        .count();
    assert_eq!(local_rows, 1);

    // Two members waiting on two updates each; the third has nothing.
    assert_eq!(view["counts"]["members"], 3);
    assert_eq!(view["counts"]["reachable"], 3);
    assert_eq!(view["counts"]["members_with_updates"], 2);
    assert_eq!(view["counts"]["updates"], 4);
    assert_eq!(view["counts"]["restarts_required"], 0);
}

/// One member that has gone away degrades one row. The console renders the
/// others rather than blanking the table.
#[tokio::test]
async fn an_unreachable_member_costs_one_row_and_no_more() {
    let h = harness(
        "unreachable",
        vec![
            Arc::new(FakeMember::new("alpha-1", waiting(), Outcome::Installs)),
            Arc::new(FakeMember::new("alpha-2", waiting(), Outcome::Gone)),
        ],
        MockUpdates::new(),
    )
    .await;

    let (_, view) = h.get("/api/environment/updates").await;
    let rows = view["members"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "the member is listed, not dropped");

    let gone = rows.iter().find(|row| row["node"] == "alpha-2").unwrap();
    assert_eq!(gone["reachable"], false);
    assert!(gone["error"]
        .as_str()
        .unwrap()
        .contains("could not connect"));
    assert!(gone.get("updates").is_none());

    // And the members that did answer are counted.
    assert_eq!(view["counts"]["members"], 3);
    assert_eq!(view["counts"]["reachable"], 2);
}

#[tokio::test]
async fn checking_the_environment_asks_every_member() {
    let h = harness(
        "check",
        vec![
            Arc::new(FakeMember::new("alpha-1", waiting(), Outcome::Installs)),
            Arc::new(FakeMember::new("alpha-2", waiting(), Outcome::Installs)),
        ],
        MockUpdates::new().with_updates(waiting()),
    )
    .await;

    let (status, view) = h.post("/api/environment/updates/check", "{}").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.member("alpha-1").checks.load(Ordering::SeqCst), 1);
    assert_eq!(h.member("alpha-2").checks.load(Ordering::SeqCst), 1);
    assert_eq!(h.local.checks(), 1, "this node asks its own repositories");
    assert_eq!(view["counts"]["updates"], 6);
}

// --- the walk ----------------------------------------------------------------

/// The shape of the whole feature: every member updated, one at a time, with
/// this node last.
#[tokio::test]
async fn the_walk_updates_every_member_and_leaves_this_node_until_last() {
    let h = harness(
        "walk",
        vec![
            Arc::new(FakeMember::new("alpha-1", waiting(), Outcome::Installs)),
            Arc::new(FakeMember::new("alpha-2", waiting(), Outcome::Installs)),
        ],
        MockUpdates::new().with_updates(waiting()),
    )
    .await;

    let (status, accepted) = h.post("/api/environment/updates/apply", "{}").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(accepted["kind"], "ordinary");
    assert_eq!(accepted["by"], "alice@lumen");

    // The whole of the work is named before anything is installed, in the
    // order it will be done — and this node is at the end of it.
    assert_eq!(
        steps(&accepted)
            .iter()
            .map(|(node, _)| node.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha-1", "alpha-2", LOCAL]
    );

    let progress = h.settled().await;
    assert_eq!(progress["phase"], "complete", "{progress}");
    assert!(progress["error"].is_null());
    for (node, state) in steps(&progress) {
        assert_eq!(state, "done", "{node} should have finished");
    }

    // Each member was asked exactly once, and this node's package manager ran
    // a transaction of its own.
    assert_eq!(h.member("alpha-1").applies.load(Ordering::SeqCst), 1);
    assert_eq!(h.member("alpha-2").applies.load(Ordering::SeqCst), 1);
    assert_eq!(h.local.applied().len(), 1);

    // What this node ran was the ordinary transaction: it excluded the whole
    // platform set, which is the property the update domain exists for.
    let plan = &h.local.applied()[0];
    assert!(plan.packages.is_empty());
    assert!(plan.exclude.contains(&"kernel*".to_string()));
    assert!(plan.exclude.contains(&"kmod-*".to_string()));

    // And the walk reports what each member changed.
    let first = &progress["steps"][0];
    let changed: Vec<&str> = first["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect();
    assert_eq!(changed, vec!["lumen-controlplane", "libvirt"]);
    assert_eq!(first["restart_required"], false);
}

/// A member with nothing waiting is not an error and does not stop the walk —
/// it is the ordinary state of a retry, and of a cluster that was updated
/// yesterday.
#[tokio::test]
async fn a_member_that_is_already_current_is_stepped_over() {
    let h = harness(
        "current",
        vec![
            Arc::new(FakeMember::new("alpha-1", Vec::new(), Outcome::Installs)),
            Arc::new(FakeMember::new("alpha-2", waiting(), Outcome::Installs)),
        ],
        MockUpdates::new(),
    )
    .await;

    h.post("/api/environment/updates/apply", "{}").await;
    let progress = h.settled().await;
    assert_eq!(progress["phase"], "complete", "{progress}");

    let first = &progress["steps"][0];
    assert_eq!(first["node"], "alpha-1");
    assert_eq!(first["state"], "done");
    assert!(first["detail"]
        .as_str()
        .unwrap()
        .contains("Already up to date"));
    assert_eq!(
        h.member("alpha-1").applies.load(Ordering::SeqCst),
        0,
        "a member with nothing waiting must not be handed a transaction"
    );

    // The member behind it was still updated, and so was this node — which
    // had nothing waiting either.
    assert_eq!(h.member("alpha-2").applies.load(Ordering::SeqCst), 1);
    assert!(h.local.applied().is_empty());
    assert_eq!(progress["steps"][2]["state"], "done");
}

/// The stopping rule. A failure on one member does not become a failure on
/// every member after it.
#[tokio::test]
async fn the_walk_stops_at_the_first_member_that_fails() {
    let h = harness(
        "stop",
        vec![
            Arc::new(FakeMember::new("alpha-1", waiting(), Outcome::Installs)),
            Arc::new(FakeMember::new(
                "alpha-2",
                waiting(),
                Outcome::Fails("the mirror closed the connection".to_string()),
            )),
        ],
        MockUpdates::new().with_updates(waiting()),
    )
    .await;

    h.post("/api/environment/updates/apply", "{}").await;
    let progress = h.settled().await;
    assert_eq!(progress["phase"], "failed", "{progress}");

    assert_eq!(
        steps(&progress),
        vec![
            ("alpha-1".to_string(), "done".to_string()),
            ("alpha-2".to_string(), "failed".to_string()),
            // Never attempted, and still saying so.
            (LOCAL.to_string(), "pending".to_string()),
        ]
    );

    // The error names the member, quotes the package manager, and says what
    // did happen before it.
    let error = progress["error"].as_str().unwrap();
    assert!(error.contains("alpha-2"), "{error}");
    assert!(
        error.contains("the mirror closed the connection"),
        "{error}"
    );
    assert!(error.contains("alpha-1"), "{error}");

    // This node was left alone, which is the whole point of stopping.
    assert!(h.local.applied().is_empty());
}

/// The case the walk is really built around: installing Lumen's own packages
/// restarts the control plane holding the progress feed. The member comes back
/// reporting no transaction at all, and that is a success — decided from what
/// its package database says, not from the feed that died.
#[tokio::test]
async fn a_member_whose_control_plane_restarts_mid_transaction_counts_as_updated() {
    let h = harness(
        "restart",
        vec![
            Arc::new(FakeMember::new(
                "alpha-1",
                waiting(),
                Outcome::RestartsAfterInstalling {
                    leaves_waiting: false,
                },
            )),
            Arc::new(FakeMember::new("alpha-2", waiting(), Outcome::Installs)),
        ],
        MockUpdates::new(),
    )
    .await;

    h.post("/api/environment/updates/apply", "{}").await;
    let progress = h.settled().await;
    assert_eq!(progress["phase"], "complete", "{progress}");
    assert_eq!(progress["steps"][0]["node"], "alpha-1");
    assert_eq!(progress["steps"][0]["state"], "done");

    // The walk carried on to the member behind it.
    assert_eq!(progress["steps"][1]["state"], "done");
    assert_eq!(h.member("alpha-2").applies.load(Ordering::SeqCst), 1);
}

/// The other half of that rule: a missing feed is not permission to assume
/// success. A member that restarted with packages still waiting has not
/// finished, and the walk stops and names them.
#[tokio::test]
async fn a_restart_that_left_updates_waiting_is_a_failure() {
    let h = harness(
        "restart-dirty",
        vec![
            Arc::new(FakeMember::new(
                "alpha-1",
                waiting(),
                Outcome::RestartsAfterInstalling {
                    leaves_waiting: true,
                },
            )),
            Arc::new(FakeMember::new("alpha-2", waiting(), Outcome::Installs)),
        ],
        MockUpdates::new(),
    )
    .await;

    h.post("/api/environment/updates/apply", "{}").await;
    let progress = h.settled().await;
    assert_eq!(progress["phase"], "failed", "{progress}");
    assert_eq!(progress["steps"][0]["state"], "failed");

    let detail = progress["steps"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("restarted"), "{detail}");
    assert!(detail.contains("lumen-controlplane"), "{detail}");

    // Nothing after it was attempted.
    assert_eq!(h.member("alpha-2").applies.load(Ordering::SeqCst), 0);
    assert_eq!(progress["steps"][1]["state"], "pending");
}

/// A member that cannot be reached at all fails its step rather than being
/// silently skipped — an update that did not happen must not read as one that
/// did.
#[tokio::test]
async fn a_member_that_cannot_be_reached_stops_the_walk() {
    let h = harness(
        "walk-unreachable",
        vec![
            Arc::new(FakeMember::new("alpha-1", waiting(), Outcome::Gone)),
            Arc::new(FakeMember::new("alpha-2", waiting(), Outcome::Installs)),
        ],
        MockUpdates::new(),
    )
    .await;

    h.post("/api/environment/updates/apply", "{}").await;
    let progress = h.settled().await;
    assert_eq!(progress["phase"], "failed", "{progress}");
    assert_eq!(progress["steps"][0]["state"], "failed");
    assert_eq!(h.member("alpha-2").applies.load(Ordering::SeqCst), 0);
}

// --- the rolling walk --------------------------------------------------------

const ROLLING: &str = r#"{"rolling":true,"i_understand_each_node_restarts":true}"#;

/// The whole of Tier 2: a member with a kernel waiting is emptied, brought up
/// to date, restarted, waited for, and put back into service — in that order,
/// and before the next member is touched.
#[tokio::test]
async fn a_rolling_update_drains_installs_restarts_and_returns_each_member() {
    let h = harness(
        "rolling",
        vec![
            Arc::new(FakeMember::new("alpha-1", waiting(), Outcome::Installs).with_platform(true)),
            Arc::new(FakeMember::new("alpha-2", waiting(), Outcome::Installs).with_platform(true)),
        ],
        MockUpdates::new(),
    )
    .await;

    let (status, accepted) = h.post("/api/environment/updates/apply", ROLLING).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["kind"], "rolling");

    let progress = h.settled().await;
    assert_eq!(progress["phase"], "complete", "{progress}");

    for name in ["alpha-1", "alpha-2"] {
        let member = h.member(name);
        assert_eq!(
            member.drains.load(Ordering::SeqCst),
            1,
            "{name} drained once"
        );
        // Two transactions: the ordinary set, then the platform set.
        assert_eq!(member.applies.load(Ordering::SeqCst), 2, "{name} installed");
        assert_eq!(
            member.restarts.load(Ordering::SeqCst),
            1,
            "{name} restarted"
        );
        assert_eq!(
            member.returns.load(Ordering::SeqCst),
            1,
            "{name} back in service"
        );
        assert!(
            member.in_service.load(Ordering::SeqCst),
            "{name} must not be left out of service"
        );
        assert!(
            member.waiting.lock().unwrap().is_empty() && member.platform.lock().unwrap().is_empty(),
            "{name} should have nothing left waiting"
        );
        assert!(
            !member.stale_kernel.load(Ordering::SeqCst),
            "{name} should be running the kernel it installed"
        );
    }

    // Each member's step says so, and the restart is accounted for.
    for index in 0..2 {
        assert_eq!(progress["steps"][index]["state"], "done");
        assert_eq!(progress["steps"][index]["stage"], "finished");
        assert_eq!(progress["steps"][index]["restart_required"], false);
    }
}

/// The limit, stated rather than hidden: the node driving the update is the
/// one node it will not restart.
#[tokio::test]
async fn a_rolling_update_leaves_the_coordinating_node_to_the_operator() {
    let h = harness(
        "rolling-local",
        vec![Arc::new(
            FakeMember::new("alpha-1", Vec::new(), Outcome::Installs).with_platform(true),
        )],
        // This node has a kernel waiting too, which is what makes the case.
        MockUpdates::new()
            .with_updates(vec![Update::new(
                "kernel-core",
                "6.12.0-212.el10",
                "baseos",
            )])
            .landing_on_kernel("6.12.0-212.el10.x86_64"),
    )
    .await;

    h.post("/api/environment/updates/apply", ROLLING).await;
    let progress = h.settled().await;

    // The peer was rolled in full.
    assert_eq!(progress["steps"][0]["state"], "done");
    assert_eq!(h.member("alpha-1").restarts.load(Ordering::SeqCst), 1);

    // This node was not — and says so plainly rather than being marked done.
    let local = &progress["steps"][1];
    assert_eq!(local["node"], LOCAL);
    assert_eq!(local["state"], "pending");
    let detail = local["detail"].as_str().unwrap();
    assert!(detail.contains("running the update"), "{detail}");
    assert!(detail.contains("another member"), "{detail}");

    // And the walk carries the same sentence where the console can find it
    // without digging through the steps.
    assert!(progress["left_to_you"]
        .as_str()
        .unwrap()
        .contains("Maintenance"));
    // The walk itself did everything it could, so it is complete, not failed.
    assert_eq!(progress["phase"], "complete", "{progress}");
    assert!(h.local.applied().is_empty(), "nothing was installed here");
}

/// The gate the whole update domain exists for, reached at its most dangerous
/// point: a rolling update is the one thing that would install an unresolvable
/// kernel on every node in turn.
#[tokio::test]
async fn a_rolling_update_refuses_a_member_whose_kernel_will_not_resolve() {
    let h = harness(
        "rolling-blocked",
        vec![
            Arc::new(FakeMember::new("alpha-1", waiting(), Outcome::Installs).with_platform(false)),
            Arc::new(FakeMember::new("alpha-2", waiting(), Outcome::Installs).with_platform(true)),
        ],
        MockUpdates::new(),
    )
    .await;

    h.post("/api/environment/updates/apply", ROLLING).await;
    let progress = h.settled().await;
    assert_eq!(progress["phase"], "failed", "{progress}");

    let detail = progress["steps"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("nothing provides"), "{detail}");
    assert!(detail.contains("import its pool"), "{detail}");

    // Nothing was installed on it, and it was never drained or restarted.
    let blocked = h.member("alpha-1");
    assert_eq!(blocked.applies.load(Ordering::SeqCst), 0);
    assert_eq!(blocked.drains.load(Ordering::SeqCst), 0);
    assert_eq!(blocked.restarts.load(Ordering::SeqCst), 0);
    // And nothing after it was attempted.
    assert_eq!(h.member("alpha-2").drains.load(Ordering::SeqCst), 0);
}

/// A node that could not be emptied is not a node to restart. The machine that
/// would not move is named, the node goes back into service, and the update
/// stops rather than doing the same thing to every member.
#[tokio::test]
async fn a_member_that_will_not_empty_is_not_restarted() {
    let h = harness(
        "rolling-stranded",
        vec![
            Arc::new(
                FakeMember::new("alpha-1", Vec::new(), Outcome::Installs)
                    .with_platform(true)
                    .stranding(101, "billing-db", "It has an ISO attached."),
            ),
            Arc::new(FakeMember::new("alpha-2", Vec::new(), Outcome::Installs).with_platform(true)),
        ],
        MockUpdates::new(),
    )
    .await;

    h.post("/api/environment/updates/apply", ROLLING).await;
    let progress = h.settled().await;
    assert_eq!(progress["phase"], "failed", "{progress}");

    let stuck = h.member("alpha-1");
    assert_eq!(stuck.drains.load(Ordering::SeqCst), 1);
    assert_eq!(
        stuck.restarts.load(Ordering::SeqCst),
        0,
        "a node with a machine still on it must never be restarted"
    );
    assert_eq!(stuck.applies.load(Ordering::SeqCst), 0, "nothing installed");
    assert!(
        stuck.in_service.load(Ordering::SeqCst),
        "it must be put back into service rather than left out of it"
    );

    let step = &progress["steps"][0];
    assert_eq!(step["state"], "failed");
    assert_eq!(step["stranded"][0]["name"], "billing-db");
    let detail = step["detail"].as_str().unwrap();
    assert!(detail.contains("billing-db"), "{detail}");
    assert!(detail.contains("back into service"), "{detail}");

    assert_eq!(h.member("alpha-2").drains.load(Ordering::SeqCst), 0);
}

/// A member that only has userland updates is not taken down for them. A
/// rolling update restarts the nodes that need restarting, not every node.
#[tokio::test]
async fn a_rolling_update_does_not_drain_a_member_that_needs_no_restart() {
    let h = harness(
        "rolling-in-place",
        vec![
            // Userland only.
            Arc::new(FakeMember::new("alpha-1", waiting(), Outcome::Installs)),
            // A kernel waiting.
            Arc::new(FakeMember::new("alpha-2", Vec::new(), Outcome::Installs).with_platform(true)),
        ],
        MockUpdates::new(),
    )
    .await;

    h.post("/api/environment/updates/apply", ROLLING).await;
    let progress = h.settled().await;
    assert_eq!(progress["phase"], "complete", "{progress}");

    let in_place = h.member("alpha-1");
    assert_eq!(in_place.applies.load(Ordering::SeqCst), 1);
    assert_eq!(
        in_place.drains.load(Ordering::SeqCst),
        0,
        "taking a node down to install userland packages is a cost with no purpose"
    );
    assert_eq!(in_place.restarts.load(Ordering::SeqCst), 0);

    let rolled = h.member("alpha-2");
    assert_eq!(rolled.drains.load(Ordering::SeqCst), 1);
    assert_eq!(rolled.restarts.load(Ordering::SeqCst), 1);
}

/// A node that was left running the old kernel by an earlier install is
/// restarted even though nothing new is waiting — that outstanding restart is
/// exactly what a rolling update is for.
#[tokio::test]
async fn a_member_already_owed_a_restart_is_rolled_even_with_nothing_waiting() {
    let member = Arc::new(FakeMember::new("alpha-1", Vec::new(), Outcome::Installs));
    member.stale_kernel.store(true, Ordering::SeqCst);
    let h = harness("rolling-owed", vec![member], MockUpdates::new()).await;

    h.post("/api/environment/updates/apply", ROLLING).await;
    let progress = h.settled().await;
    assert_eq!(progress["phase"], "complete", "{progress}");

    let owed = h.member("alpha-1");
    assert_eq!(owed.drains.load(Ordering::SeqCst), 1);
    assert_eq!(owed.restarts.load(Ordering::SeqCst), 1);
    assert_eq!(owed.returns.load(Ordering::SeqCst), 1);
    assert_eq!(
        owed.applies.load(Ordering::SeqCst),
        0,
        "there was nothing to install — only a restart to take"
    );
}

/// A drain the member itself refuses — its cluster cannot spare it — stops the
/// update on the member's own judgement rather than the coordinator's.
#[tokio::test]
async fn a_member_that_refuses_to_be_drained_stops_the_rolling_update() {
    let h = harness(
        "rolling-refused",
        vec![
            Arc::new(
                FakeMember::new("alpha-1", Vec::new(), Outcome::Installs)
                    .with_platform(true)
                    .refusing_drain("\"alpha\" would lose quorum without this node"),
            ),
            Arc::new(FakeMember::new("alpha-2", Vec::new(), Outcome::Installs).with_platform(true)),
        ],
        MockUpdates::new(),
    )
    .await;

    h.post("/api/environment/updates/apply", ROLLING).await;
    let progress = h.settled().await;
    assert_eq!(progress["phase"], "failed", "{progress}");

    let detail = progress["steps"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("lose quorum"), "{detail}");
    assert_eq!(h.member("alpha-1").restarts.load(Ordering::SeqCst), 0);
    assert_eq!(h.member("alpha-2").drains.load(Ordering::SeqCst), 0);
}

// --- the refusals ------------------------------------------------------------

/// A rolling update restarts every node in the cluster. It does not happen
/// because somebody sent `{"rolling":true}` by itself.
#[tokio::test]
async fn a_rolling_update_needs_the_acknowledgement() {
    let h = harness(
        "rolling-ack",
        vec![Arc::new(
            FakeMember::new("alpha-1", waiting(), Outcome::Installs).with_platform(true),
        )],
        MockUpdates::new(),
    )
    .await;

    let (status, body) = h
        .post("/api/environment/updates/apply", r#"{"rolling":true}"#)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("restarts every node"), "{error}");
    assert_eq!(h.member("alpha-1").drains.load(Ordering::SeqCst), 0);
}

/// The kernel does not move across a cluster this way. The refusal points at
/// what does.
#[tokio::test]
async fn the_platform_set_is_refused_across_the_environment() {
    let h = harness(
        "platform",
        vec![Arc::new(FakeMember::new(
            "alpha-1",
            waiting(),
            Outcome::Installs,
        ))],
        MockUpdates::new().with_updates(waiting()),
    )
    .await;

    let (status, body) = h
        .post("/api/environment/updates/apply", r#"{"platform":true}"#)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("rolling update"), "{error}");
    assert!(h.local.applied().is_empty());
    assert_eq!(h.member("alpha-1").applies.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_second_walk_is_refused_while_one_is_running() {
    let h = harness(
        "second",
        vec![Arc::new(FakeMember::new(
            "alpha-1",
            waiting(),
            // Never answers, so the first walk is still waiting on it when the
            // second request arrives.
            Outcome::RestartsAfterInstalling {
                leaves_waiting: false,
            },
        ))],
        MockUpdates::new(),
    )
    .await;

    let (status, _) = h.post("/api/environment/updates/apply", "{}").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, body) = h.post("/api/environment/updates/apply", "{}").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("already updating the environment"),
        "{body}"
    );

    h.settled().await;
}

#[tokio::test]
async fn every_environment_update_route_needs_a_session() {
    let h = harness(
        "auth",
        vec![Arc::new(FakeMember::new(
            "alpha-1",
            waiting(),
            Outcome::Installs,
        ))],
        MockUpdates::new(),
    )
    .await;

    for (method, path) in [
        ("GET", "/api/environment/updates"),
        ("GET", "/api/environment/updates/progress"),
        ("POST", "/api/environment/updates/check"),
        ("POST", "/api/environment/updates/apply"),
    ] {
        let request = if method == "GET" {
            Request::get(path).body(Body::empty()).unwrap()
        } else {
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap()
        };
        let (status, _) = h.send(request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
    assert_eq!(h.member("alpha-1").applies.load(Ordering::SeqCst), 0);
}

/// An unknown field is a typo in something that installs software across a
/// whole cluster. Rejected, not ignored.
#[tokio::test]
async fn an_unknown_field_is_refused() {
    let h = harness(
        "strict",
        vec![Arc::new(FakeMember::new(
            "alpha-1",
            waiting(),
            Outcome::Installs,
        ))],
        MockUpdates::new(),
    )
    .await;

    let (status, _) = h
        .post("/api/environment/updates/apply", r#"{"platfrom":true}"#)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(h.member("alpha-1").applies.load(Ordering::SeqCst), 0);
}

/// A single appliance that never joined an environment still answers, with
/// itself alone — so the console renders one table either way rather than
/// carrying a second code path for the un-clustered case.
#[tokio::test]
async fn a_node_with_no_environment_answers_with_itself() {
    let dir = TempDir(std::env::temp_dir().join(format!(
        "lumen-cp-cupd-alone-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    )));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();

    let mut config = Config::from_env();
    config.webui_dir = std::env::temp_dir().join("lumen-webui-none");
    config.no_tls = true;
    config.session_ttl_secs = 3600;
    config.update_check_secs = 0;

    let storage = Arc::new(StorageService::new(Arc::new(MockZfsBackend::appliance())));
    let network = Arc::new(NetworkService::new(
        Arc::new(lumen_net::backend::mock::MockBackend::appliance()),
        &dir.0.join("net"),
        60,
    ));
    let cluster = Arc::new(ClusterService::new(
        Arc::new(ClusterMockBackend::appliance()),
        Arc::new(lumen_cluster::MockPeers::new()),
        network.clone(),
        &dir.0,
        "test",
    ));
    let virt = Arc::new(VirtService::new(
        Arc::new(lumen_virt::backend::mock::MockBackend::appliance()),
        storage.clone(),
        network.clone(),
        Arc::new(lumen_drbd::MockVmVolumes::standalone()),
    ));
    let drbd = Arc::new(lumen_drbd::DrbdService::new(
        Arc::new(lumen_drbd::backend::mock::MockBackend::appliance()),
        Arc::new(lumen_drbd::MockVolumePeers::new()),
        cluster.clone(),
        storage.clone(),
    ));
    let local = Arc::new(MockUpdates::new().with_updates(waiting()));
    let router = app(Arc::new(AppState {
        config,
        jwt_secret: lumen_controlplane::security::session_secret(TICKET_SECRET.to_vec()),
        tls: None,
        realms: RealmRegistry::new().register(Box::new(MockRealm)),
        sys: Arc::new(SysService::new(
            Arc::new(MockPower::appliance()),
            Arc::new(MockExec::new()),
        )),
        network,
        storage,
        virt,
        cluster,
        peers: Arc::new(lumen_controlplane::inventory::NoPeers),
        drbd,
        pool: lumen_controlplane::pool::PoolPresence::Absent,
        updates: Arc::new(UpdateService::new(local.clone(), "lumen")),
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
        drain: Default::default(),
        update_job: Default::default(),
        roll: Default::default(),
        pool_deploy: std::sync::Arc::new(lumen_pool::PoolDeploy::new(
            lumen_sys::exec::MockExec::working(),
        )),
        pool_peers: std::sync::Arc::new(lumen_controlplane::inventory::NoPeers),
        pool_job: Default::default(),
    }));

    let response = router
        .clone()
        .oneshot(
            Request::post("/api/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"username":"alice","password":"correct-horse"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap()
        .to_string();

    let h = Harness {
        router,
        local,
        members: Vec::new(),
        cookie,
        _dir: dir,
    };

    let (status, view) = h.post("/api/environment/updates/check", "{}").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["members"].as_array().unwrap().len(), 1);
    assert_eq!(view["members"][0]["local"], true);
    assert_eq!(view["counts"]["updates"], 2);

    // And a walk over an environment of one is just this node.
    let (status, accepted) = h.post("/api/environment/updates/apply", "{}").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(accepted["steps"].as_array().unwrap().len(), 1);
    let progress = h.settled().await;
    assert_eq!(progress["phase"], "complete", "{progress}");
    assert_eq!(h.local.applied().len(), 1);
}
