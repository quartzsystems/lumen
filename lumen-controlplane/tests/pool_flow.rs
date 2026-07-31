//! The LumenFS pool over the real router: the observed view, the snapshot
//! verbs with their rollback guard, and the peer route that runs a closed
//! verb against a **real daemon** on this node's loopback — every other
//! domain on its in-memory backend, nothing touched on the machine running
//! these.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use lumen_controlplane::config::Config;
use lumen_controlplane::pool::PoolPresence;
use lumen_controlplane::realm::{AuthFailure, Realm, RealmKind, RealmRegistry};
use lumen_controlplane::security;
use lumen_controlplane::{app, AppState};
use lumen_drbd::{VmDiskRequest, VmVolumes};
use lumen_net::NetworkService;
use lumen_pool::{MockFleet, PoolFleet, PoolService};
use lumen_virt::VirtService;
use lumen_zfs::StorageService;

const TICKET_SECRET: &[u8] = b"test-secret-test-secret-test-secret!";
const HERE: &str = "alpha-1";
const THERE: &str = "alpha-2";

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
    async fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthFailure> {
        if username == "root" && password == "correct-horse" {
            Ok(())
        } else {
            Err(AuthFailure::Denied)
        }
    }
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lumen-pool-flow-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A router whose pool is whatever the test hands in; everything else is
/// the standard in-memory appliance.
fn router_with(tag: &str, pool: PoolPresence) -> (axum::Router, TempDir) {
    let mut config = Config::from_env();
    config.webui_dir = std::env::temp_dir().join("lumen-webui-none");
    config.no_tls = true;
    config.session_ttl_secs = 3600;
    let state_dir = TempDir::new(tag);
    let network = Arc::new(NetworkService::new(
        Arc::new(lumen_net::backend::mock::MockBackend::appliance()),
        &state_dir.0.join("net"),
        60,
    ));
    let storage = Arc::new(StorageService::new(Arc::new(
        lumen_zfs::backend::mock::MockBackend::appliance(),
    )));
    let virt = Arc::new(VirtService::new(
        Arc::new(lumen_virt::backend::mock::MockBackend::appliance()),
        storage.clone(),
        network.clone(),
        Arc::new(lumen_drbd::MockVmVolumes::standalone()),
    ));
    let sys = Arc::new(lumen_sys::SysService::new(
        Arc::new(lumen_sys::backend::mock::MockPower::appliance()),
        Arc::new(lumen_sys::exec::MockExec::new()),
    ));
    let cluster = Arc::new(
        ClusterService::new(
            Arc::new(lumen_cluster::backend::mock::MockBackend::environment()),
            Arc::new(lumen_cluster::MockPeers::new()),
            network.clone(),
            &state_dir.0,
            "test",
        )
        .with_node(HERE),
    );
    let drbd = Arc::new(lumen_drbd::DrbdService::new(
        Arc::new(lumen_drbd::backend::mock::MockBackend::appliance()),
        Arc::new(lumen_drbd::MockVolumePeers::new()),
        cluster.clone(),
        storage.clone(),
    ));
    let router = app(Arc::new(AppState {
        config,
        jwt_secret: security::session_secret(TICKET_SECRET.to_vec()),
        tls: None,
        realms: RealmRegistry::new().register(Box::new(MockRealm)),
        sys,
        network,
        storage,
        virt,
        cluster,
        peers: Arc::new(lumen_controlplane::inventory::NoPeers),
        drbd,
        pool,
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
        updates: Arc::new(lumen_update::UpdateService::new(
            Arc::new(lumen_update::MockUpdates::new()),
            "test-node",
        )),
        drain: Default::default(),
        update_job: Default::default(),
        roll: Default::default(),
    }));
    (router, state_dir)
}

use lumen_cluster::ClusterService;

/// A pool of two over the mock fleet, plus the fleet for arranging states
/// the routes must react to. The control address points at nothing: these
/// tests are about the routes, and the peer-verb test below is the one that
/// brings a real daemon.
fn pooled() -> (Arc<MockFleet>, PoolPresence) {
    let fleet = Arc::new(MockFleet::pooled(&[HERE, THERE]));
    let service = Arc::new(PoolService::new(fleet.clone(), "alpha"));
    (
        fleet,
        PoolPresence::Present {
            service,
            control: "127.0.0.1:1".parse().unwrap(),
        },
    )
}

async fn sign_in(router: &axum::Router) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::post("/api/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"username":"root","password":"correct-horse"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("login sets the session cookie")
        .to_str()
        .unwrap();
    cookie.split(';').next().unwrap().to_string()
}

async fn request(
    router: &axum::Router,
    method: Method,
    path: &str,
    cookie: Option<&str>,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    if let Some(bearer) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        // An extractor rejection (the 422 for a verb outside the closed
        // set) answers plain text, not the JSON envelope; carry it as a
        // string rather than refusing to look at it.
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
        })
    };
    (status, value)
}

#[tokio::test]
async fn a_standalone_node_answers_no_pool_and_no_error() {
    let (router, _dir) = router_with("absent", PoolPresence::Absent);
    let cookie = sign_in(&router).await;
    let (status, body) = request(
        &router,
        Method::GET,
        "/api/storage/pool",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pool"], serde_json::Value::Null);
    assert_eq!(body["error"], serde_json::Value::Null);

    // And the verbs say why they refuse, rather than 500ing on nothing.
    let (status, body) = request(
        &router,
        Method::POST,
        "/api/storage/pool/disks/vm-7-disk-0/snapshots",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "snapshot": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("no LumenFS pool"));
}

#[tokio::test]
async fn a_broken_drop_in_is_an_error_not_an_absent_pool() {
    // The failure the middle shape prevents: a half-written fsd.conf
    // reading as "nothing to show" instead of "your deployment is broken".
    let (router, _dir) = router_with(
        "broken",
        PoolPresence::Broken("no LUMEN_FSD_BRICK: this names the pool's brick".into()),
    );
    let cookie = sign_in(&router).await;
    let (status, body) = request(
        &router,
        Method::GET,
        "/api/storage/pool",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pool"], serde_json::Value::Null);
    assert!(
        body["error"].as_str().unwrap().contains("LUMEN_FSD_BRICK"),
        "the reason should reach the page: {body}"
    );
}

#[tokio::test]
async fn the_pool_page_is_one_read_and_the_snapshot_verbs_change_what_it_says() {
    let (fleet, pool) = pooled();
    let service = pool.service().unwrap().clone();
    let (router, _dir) = router_with("page", pool);
    let cookie = sign_in(&router).await;

    // A machine's disk, made through the seam the compute domain uses.
    service
        .create_disk(&VmDiskRequest {
            name: "vm-7-disk-3".into(),
            size_bytes: 512 << 20,
            members: Vec::new(),
        })
        .await
        .unwrap();

    let (status, body) = request(
        &router,
        Method::GET,
        "/api/storage/pool",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let view = &body["pool"];
    assert_eq!(view["name"], "alpha");
    assert_eq!(view["health"], "Healthy");
    assert_eq!(view["members"].as_array().unwrap().len(), 2);
    let disk = view["vdisks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["device"] == "/dev/ublkb1795")
        .expect("the created disk should be in the view");
    assert_eq!(disk["disk"]["vmid"], 7);
    assert_eq!(disk["exported_on"], serde_json::json!([HERE]));
    assert_eq!(disk["snapshots"], serde_json::json!([]));

    // Take a snapshot through the route; the next read carries it.
    let (status, body) = request(
        &router,
        Method::POST,
        "/api/storage/pool/disks/vm-7-disk-3/snapshots",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "snapshot": 1_700_000_100 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = request(
        &router,
        Method::GET,
        "/api/storage/pool",
        Some(&cookie),
        None,
        None,
    )
    .await;
    let snapshots = &body["pool"]["vdisks"][0]["snapshots"];
    assert_eq!(snapshots[0]["snapshot"], 1_700_000_100);

    // Delete it; the next read agrees.
    let (status, body) = request(
        &router,
        Method::DELETE,
        "/api/storage/pool/disks/vm-7-disk-3/snapshots/1700000100",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = request(
        &router,
        Method::GET,
        "/api/storage/pool",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(
        body["pool"]["vdisks"][0]["snapshots"],
        serde_json::json!([])
    );
    let _ = fleet;
}

#[tokio::test]
async fn a_rollback_needs_the_acknowledgement_and_a_served_disk_still_refuses() {
    let (fleet, pool) = pooled();
    let service = pool.service().unwrap().clone();
    let (router, _dir) = router_with("rollback", pool);
    let cookie = sign_in(&router).await;
    service
        .create_disk(&VmDiskRequest {
            name: "vm-7-disk-0".into(),
            size_bytes: 1 << 30,
            members: Vec::new(),
        })
        .await
        .unwrap();
    service
        .snapshot("/dev/ublkb1792", 1_700_000_100)
        .await
        .unwrap();

    // Without the acknowledgement: refused before anything is asked.
    let (status, body) = request(
        &router,
        Method::POST,
        "/api/storage/pool/disks/vm-7-disk-0/rollback",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "snapshot": 1_700_000_100 })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("i_understand_this_may_lose_data"),
        "{body}"
    );

    // Acknowledged but served: the pool-wide guard names the member.
    let (status, body) = request(
        &router,
        Method::POST,
        "/api/storage/pool/disks/vm-7-disk-0/rollback",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "snapshot": 1_700_000_100,
            "i_understand_this_may_lose_data": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains(HERE), "{body}");

    // Served nowhere: the same request goes through.
    fleet.unexport(HERE, 7 * 256).await.unwrap();
    let (status, body) = request(
        &router,
        Method::POST,
        "/api/storage/pool/disks/vm-7-disk-0/rollback",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "snapshot": 1_700_000_100,
            "i_understand_this_may_lose_data": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn the_disk_name_is_the_only_address_the_routes_accept() {
    let (_fleet, pool) = pooled();
    let (router, _dir) = router_with("names", pool);
    let cookie = sign_in(&router).await;
    // Machine 0 does not exist, and a bare word is nobody's disk: refused
    // by name, before the pool is asked anything.
    for wrong in ["vm-0-disk-0", "scratch", "vm-7-disk-256"] {
        let (status, body) = request(
            &router,
            Method::POST,
            &format!("/api/storage/pool/disks/{wrong}/snapshots"),
            Some(&cookie),
            None,
            Some(serde_json::json!({ "snapshot": 1 })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{wrong}: {body}");
    }
}

#[tokio::test]
async fn the_pool_routes_need_a_session_like_every_operator_surface() {
    let (_fleet, pool) = pooled();
    let (router, _dir) = router_with("auth", pool);
    let (status, _) = request(&router, Method::GET, "/api/storage/pool", None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// --- the peer route, against a real daemon -----------------------------------

/// Serve a real daemon's control surface on an ephemeral loopback port —
/// the same posture the shipped unit gives it, minus the fixed number.
fn real_daemon_control(dir: &std::path::Path) -> std::net::SocketAddr {
    let brick = dir.join("peer-verb.brick");
    lumen_fsd::format_brick(
        &brick,
        Some(64 << 20),
        0,
        true,
        Vec::new(),
        Some(8 << 20),
        [0xC0; 16],
        [0xC1; 16],
    )
    .unwrap();
    let daemon = lumen_fsd::Daemon::start(lumen_fsd::Config {
        node: 0,
        bricks: vec![brick],
        listen: Some("127.0.0.1:0".parse().unwrap()),
        dial: None,
    })
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // `serve` borrows the daemon for as long as it serves, which is forever;
    // leaking is the cheapest correct answer in a test process.
    let daemon: &'static lumen_fsd::Daemon = Box::leak(Box::new(daemon));
    std::thread::spawn(move || lumen_fsd::control::serve(listener, daemon));
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_verb_reaches_this_nodes_real_daemon_and_nothing_else_does() {
    let dir = TempDir::new("peer-verb");
    let control = real_daemon_control(&dir.0);
    let (fleet, _unused) = pooled();
    let service = Arc::new(PoolService::new(fleet, "alpha"));
    let (router, _dir) = router_with("peer", PoolPresence::Present { service, control });

    // A peer ticket is the authentication; the verb is a closed enum the
    // route deserializes before anything runs.
    let ticket = security::issue_peer_ticket(TICKET_SECRET, THERE).unwrap();
    let (status, body) = request(
        &router,
        Method::POST,
        "/api/peer/pool/verb",
        None,
        Some(&ticket),
        Some(serde_json::json!({ "verb": "status" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["answer"], "status", "{body}");
    // The daemon's own account, through the route: a lone listener is
    // suspended, and says so.
    assert_eq!(body["value"]["node"], 0);
    assert_eq!(body["value"]["replication"], "Suspended");

    // A browser session is not a peer: the console's cookie must not open
    // the peer surface.
    let cookie = sign_in(&router).await;
    let (status, _) = request(
        &router,
        Method::POST,
        "/api/peer/pool/verb",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "verb": "status" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // And a verb outside the closed set never runs anything.
    let (status, _) = request(
        &router,
        Method::POST,
        "/api/peer/pool/verb",
        None,
        Some(&ticket),
        Some(serde_json::json!({ "verb": "shell", "command": "rm -rf /" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_peer_verb_on_a_poolless_node_is_refused_with_the_reason() {
    let (router, _dir) = router_with("peer-absent", PoolPresence::Absent);
    let ticket = security::issue_peer_ticket(TICKET_SECRET, THERE).unwrap();
    let (status, body) = request(
        &router,
        Method::POST,
        "/api/peer/pool/verb",
        None,
        Some(&ticket),
        Some(serde_json::json!({ "verb": "status" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("no pool daemon"),
        "{body}"
    );
}
