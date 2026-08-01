//! End-to-end tests for the Updates page, over the real router with the
//! in-memory package manager injected.
//!
//! Nothing here touches the packages on the machine running the tests: the
//! backend is `lumen_update::MockUpdates`, which models what is waiting and
//! what a transaction would do to it and installs nothing. `make test`
//! therefore cannot upgrade anybody's laptop.
//!
//! The property these tests are really about is the one the domain exists for
//! — an ordinary update never moves the kernel — asserted here at the layer an
//! operator actually presses.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use lumen_controlplane::config::Config;
use lumen_controlplane::realm::{AuthFailure, Realm, RealmKind, RealmRegistry};
use lumen_controlplane::{app, AppState};
use lumen_net::NetworkService;
use lumen_sys::backend::mock::MockPower;
use lumen_sys::exec::MockExec;
use lumen_sys::SysService;
use lumen_update::{MockUpdates, Update, UpdateService};
use lumen_virt::VirtService;
use lumen_zfs::backend::mock::MockBackend as MockZfsBackend;
use lumen_zfs::StorageService;

const TICKET_SECRET: &[u8] = b"test-secret-test-secret-test-secret!";

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
    updates: Arc<MockUpdates>,
    cookie: String,
    _dir: TempDir,
}

/// The four packages a node might have waiting: one of Lumen's own, one from
/// the distribution, and the kernel with the storage module built against it.
fn waiting() -> Vec<Update> {
    vec![
        Update::new("lumen-controlplane", "0.4.0-1.el10", "lumen"),
        Update::new("libvirt", "11.0.0-2.el10", "appstream"),
        Update::new("kernel-core", "6.12.0-212.el10", "baseos"),
        Update::new("kmod-zfs-2.3", "2.3.4-1.el10", "zfs-2.3-kmod"),
    ]
}

async fn harness(tag: &str, mock: MockUpdates) -> Harness {
    let dir = TempDir(std::env::temp_dir().join(format!(
        "lumen-cp-upd-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    )));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();

    let mut config = Config::from_env();
    config.webui_dir = std::env::temp_dir().join("lumen-webui-none");
    config.no_tls = true;
    config.session_ttl_secs = 3600;
    // No periodic check in the tests: the loop lives in main, but pinning the
    // value here documents that nothing in this file depends on a timer.
    config.update_check_secs = 0;

    let exec = Arc::new(MockExec::new());
    let sys = Arc::new(SysService::new(
        Arc::new(MockPower::appliance()),
        exec.clone(),
    ));
    let zfs = Arc::new(MockZfsBackend::appliance());
    let storage = Arc::new(StorageService::new(zfs));
    let network = Arc::new(NetworkService::new(
        Arc::new(lumen_net::backend::mock::MockBackend::appliance()),
        &dir.0.join("net"),
        60,
    ));
    let virt = Arc::new(VirtService::new(
        Arc::new(lumen_virt::backend::mock::MockBackend::appliance()),
        storage.clone(),
        network.clone(),
        Arc::new(lumen_pool::MockVmVolumes::standalone()),
    ));
    let cluster = Arc::new(lumen_cluster::ClusterService::new(
        Arc::new(lumen_cluster::backend::mock::MockBackend::appliance()),
        Arc::new(lumen_cluster::MockPeers::new()),
        network.clone(),
        &dir.0,
        "test",
    ));

    let updates = Arc::new(mock);
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
        peers: Arc::new(lumen_controlplane::inventory::NoPeers),
        pool: lumen_controlplane::pool::PoolPresence::Absent,
        updates: Arc::new(UpdateService::new(updates.clone(), "lumen")),
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
        updates,
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

    /// Wait for the spawned transaction to finish. The mock returns at once,
    /// but the job is a task either way, so the test has to let it run.
    async fn settled(&self) -> serde_json::Value {
        for _ in 0..100 {
            let (_, progress) = self.get("/api/system/updates/progress").await;
            if progress
                .get("phase")
                .and_then(|phase| phase.as_str())
                .is_some_and(|phase| phase != "running")
            {
                return progress;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the update never finished");
    }
}

#[tokio::test]
async fn the_page_renders_before_anything_has_been_checked() {
    let h = harness("fresh", MockUpdates::new().with_updates(waiting())).await;

    let (status, view) = h.get("/api/system/updates").await;
    assert_eq!(status, StatusCode::OK);
    // Nothing asked yet, so nothing claimed — but the node's own reboot state
    // is still there, because it is read from the node rather than a
    // repository.
    assert!(view.get("checked_at").is_none());
    assert_eq!(view["updates"].as_array().unwrap().len(), 0);
    assert_eq!(view["reboot"]["required"], false);
    assert_eq!(
        h.updates.checks(),
        0,
        "reading must not ask the repositories"
    );

    // And the transaction feed is an answer, not a 404.
    let (status, progress) = h.get("/api/system/updates/progress").await;
    assert_eq!(status, StatusCode::OK);
    assert!(progress.is_null());
}

#[tokio::test]
async fn a_check_separates_the_platform_set_from_everything_else() {
    let h = harness("check", MockUpdates::new().with_updates(waiting())).await;

    let (status, view) = h.post("/api/system/updates/check", "{}").await;
    assert_eq!(status, StatusCode::OK);

    let ordinary: Vec<&str> = view["updates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap())
        .collect();
    assert_eq!(ordinary, vec!["lumen-controlplane", "libvirt"]);
    assert_eq!(view["counts"]["lumen"], 1);
    assert_eq!(view["counts"]["platform"], 2);
    assert_eq!(view["platform"]["resolves"], true);
    assert!(view["checked_at"].is_u64());
}

/// The property the whole feature exists for, at the layer an operator
/// presses: the ordinary button cannot move the kernel.
#[tokio::test]
async fn an_ordinary_update_leaves_the_kernel_where_it_is() {
    let h = harness("ordinary", MockUpdates::new().with_updates(waiting())).await;

    let (status, accepted) = h.post("/api/system/updates/apply", "{}").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(accepted["kind"], "ordinary");
    assert_eq!(accepted["phase"], "running");
    // Who asked is recorded on the job, so a transaction found running by a
    // second operator says whose it is.
    assert_eq!(accepted["by"], "alice");

    let progress = h.settled().await;
    assert_eq!(progress["phase"], "complete");
    let changed: Vec<&str> = progress["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert_eq!(changed, vec!["lumen-controlplane", "libvirt"]);
    assert_eq!(progress["reboot"]["required"], false);

    // The transaction the package manager was actually handed excluded every
    // platform prefix, and the kernel is still waiting afterwards.
    let plans = h.updates.applied();
    assert_eq!(plans.len(), 1);
    assert!(plans[0].exclude.contains(&"kernel*".to_string()));
    assert!(plans[0].exclude.contains(&"kmod-*".to_string()));

    let (_, view) = h.get("/api/system/updates").await;
    assert_eq!(view["counts"]["platform"], 2);
    assert_eq!(view["updates"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn the_platform_set_will_not_move_without_the_acknowledgement() {
    let h = harness("ack", MockUpdates::new().with_updates(waiting())).await;

    let (status, body) = h
        .post("/api/system/updates/apply", r#"{"platform":true}"#)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("restarted"),
        "{body}"
    );
    assert!(h.updates.applied().is_empty(), "nothing may have been run");
}

/// A kernel whose modules have not caught up is refused outright — even with
/// the acknowledgement — and the refusal carries the solver's own words.
#[tokio::test]
async fn a_platform_set_that_does_not_resolve_is_refused() {
    let h = harness(
        "blocked",
        MockUpdates::new()
            .with_updates(waiting())
            .blocking_resolution("nothing provides kernel-uname-r = 6.12.0-212.el10.x86_64"),
    )
    .await;

    let (_, view) = h.post("/api/system/updates/check", "{}").await;
    assert_eq!(view["platform"]["resolves"], false);
    assert!(view["platform"]["detail"]
        .as_str()
        .unwrap()
        .contains("nothing provides"));

    let (status, body) = h
        .post(
            "/api/system/updates/apply",
            r#"{"platform":true,"i_understand_the_kernel_moves":true}"#,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("nothing provides"),
        "{body}"
    );
    assert!(h.updates.applied().is_empty(), "nothing may have been run");
}

#[tokio::test]
async fn installing_the_platform_set_leaves_a_restart_outstanding() {
    let h = harness(
        "platform",
        MockUpdates::new()
            .with_updates(waiting())
            .landing_on_kernel("6.12.0-212.el10.x86_64"),
    )
    .await;

    let (status, accepted) = h
        .post(
            "/api/system/updates/apply",
            r#"{"platform":true,"i_understand_the_kernel_moves":true}"#,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(accepted["kind"], "platform");

    let progress = h.settled().await;
    assert_eq!(progress["phase"], "complete");
    assert_eq!(progress["reboot"]["required"], true);
    assert!(progress["reboot"]["reason"]
        .as_str()
        .unwrap()
        .contains("6.12.0-212"));

    // Named exactly, never globbed: what ran is what the console showed.
    let plans = h.updates.applied();
    assert_eq!(plans[0].packages, vec!["kernel-core", "kmod-zfs-2.3"]);
    assert!(plans[0].exclude.is_empty());

    // And nothing restarted the node — that is the power route's decision,
    // where the cluster quorum guard lives.
    let (_, power) = h.get("/api/system/power").await;
    assert!(power["scheduled"].is_null());
}

#[tokio::test]
async fn there_is_nothing_to_install_when_nothing_is_waiting() {
    let h = harness("empty", MockUpdates::new()).await;

    let (status, body) = h.post("/api/system/updates/apply", "{}").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"].as_str().unwrap().contains("no updates"),
        "{body}"
    );

    let (status, body) = h
        .post(
            "/api/system/updates/apply",
            r#"{"platform":true,"i_understand_the_kernel_moves":true}"#,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("kernel"), "{body}");
}

#[tokio::test]
async fn a_failed_transaction_is_reported_rather_than_swallowed() {
    let h = harness(
        "failure",
        MockUpdates::new()
            .with_updates(waiting())
            .failing_apply("the mirror closed the connection"),
    )
    .await;

    let (status, _) = h.post("/api/system/updates/apply", "{}").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let progress = h.settled().await;
    assert_eq!(progress["phase"], "failed");
    assert!(progress["error"]
        .as_str()
        .unwrap()
        .contains("the mirror closed the connection"));
}

#[tokio::test]
async fn every_update_route_needs_a_session() {
    let h = harness("auth", MockUpdates::new().with_updates(waiting())).await;

    for (method, path) in [
        ("GET", "/api/system/updates"),
        ("GET", "/api/system/updates/progress"),
        ("POST", "/api/system/updates/check"),
        ("POST", "/api/system/updates/apply"),
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
    assert_eq!(h.updates.checks(), 0);
}

/// An unknown field is a typo in something that installs software. It is
/// rejected rather than ignored, the same way every other write route in this
/// appliance rejects one.
#[tokio::test]
async fn an_unknown_field_is_refused() {
    let h = harness("strict", MockUpdates::new().with_updates(waiting())).await;
    let (status, _) = h
        .post("/api/system/updates/apply", r#"{"platfrom":true}"#)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(h.updates.applied().is_empty());
}
