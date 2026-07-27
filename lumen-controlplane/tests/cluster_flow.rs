//! End-to-end tests for the environment: reads, the join-token handshake,
//! cluster create with its per-step progress, and destroy — over the real
//! router with every domain on its in-memory backend. No corosync, no
//! Pacemaker, no network, nothing touched on the machine running them.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use lumen_cluster::backend::mock::{environment_membership, membership_of, MockBackend};
use lumen_cluster::{ClusterService, EnvironmentMembership, JoinToken, MockPeers};
use lumen_controlplane::config::Config;
use lumen_controlplane::realm::{AuthFailure, Realm, RealmKind, RealmRegistry};
use lumen_controlplane::security;
use lumen_controlplane::{app, AppState};
use lumen_net::NetworkService;
use lumen_virt::VirtService;
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
    async fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthFailure> {
        if username == "root" && password == "correct-horse" {
            Ok(())
        } else {
            Err(AuthFailure::Denied)
        }
    }
}

/// A scratch directory per harness, cleaned by the OS eventually; unique so
/// concurrent tests never share environment state.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lumen-cluster-flow-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

struct Harness {
    router: axum::Router,
    peers: Arc<MockPeers>,
    _state_dir: TempDir,
}

/// A router around the given cluster backend and peers; every other domain
/// is its appliance-shaped mock. The tests claim to be `alpha-1`.
fn harness(
    tag: &str,
    backend: MockBackend,
    peers: MockPeers,
    membership: Option<&EnvironmentMembership>,
) -> Harness {
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
    ));
    let sys = Arc::new(lumen_sys::SysService::new(
        Arc::new(lumen_sys::backend::mock::MockPower::appliance()),
        Arc::new(lumen_sys::exec::MockExec::new()),
    ));
    let peers = Arc::new(peers);
    let mut cluster = ClusterService::new(
        Arc::new(backend),
        peers.clone(),
        network.clone(),
        &state_dir.0,
        "test",
    )
    .with_node("alpha-1")
    .with_form_poll(Duration::from_millis(5));
    if let Some(membership) = membership {
        cluster = cluster.with_environment(membership);
    }
    let state = AppState {
        config,
        jwt_secret: security::session_secret(TICKET_SECRET.to_vec()),
        tls: None,
        realms: RealmRegistry::new().register(Box::new(MockRealm)),
        sys,
        network,
        storage,
        virt,
        cluster: Arc::new(cluster),
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
    };
    Harness {
        router: app(Arc::new(state)),
        peers,
        _state_dir: state_dir,
    }
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
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

// --- reads ------------------------------------------------------------------

#[tokio::test]
async fn every_environment_route_requires_a_session() {
    let harness = harness(
        "auth",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&environment_membership()),
    );
    for (method, path) in [
        (Method::GET, "/api/environment"),
        (Method::GET, "/api/environment/clusters/alpha"),
        (Method::POST, "/api/environment/tokens"),
        (Method::POST, "/api/environment/join"),
        (Method::POST, "/api/environment/preflight"),
        (Method::POST, "/api/environment/clusters"),
        (Method::GET, "/api/environment/clusters/pending"),
        (Method::DELETE, "/api/environment/clusters/alpha"),
        (Method::DELETE, "/api/environment/nodes/spare-1"),
        (
            Method::POST,
            "/api/environment/clusters/alpha/fence/alpha-2/test",
        ),
        (
            Method::POST,
            "/api/environment/clusters/alpha/nodes/alpha-2/confirm-dead",
        ),
    ] {
        let (status, _) = request(&harness.router, method.clone(), path, None, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
}

#[tokio::test]
async fn a_standalone_node_answers_with_itself_as_the_one_unassigned_node() {
    let harness = harness(
        "standalone",
        MockBackend::appliance(),
        MockPeers::new(),
        None,
    );
    let cookie = sign_in(&harness.router).await;
    let (status, body) = request(
        &harness.router,
        Method::GET,
        "/api/environment",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("environment").is_none(), "{body}");
    assert_eq!(body["clusters"].as_array().unwrap().len(), 0);
    let unassigned = body["unassigned"].as_array().unwrap();
    assert_eq!(unassigned.len(), 1);
    assert_eq!(unassigned[0]["node"], "alpha-1");
    assert_eq!(unassigned[0]["local"], true);
}

#[tokio::test]
async fn the_environment_is_grouped_by_cluster_then_by_node() {
    let harness = harness(
        "grouped",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&environment_membership()),
    );
    let cookie = sign_in(&harness.router).await;
    let (status, body) = request(
        &harness.router,
        Method::GET,
        "/api/environment",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(body["environment"]["nodes"], 6);
    let clusters = body["clusters"].as_array().unwrap();
    assert_eq!(clusters.len(), 2);
    assert_eq!(clusters[0]["name"], "alpha");
    assert_eq!(clusters[0]["regime"], "two_node");
    assert_eq!(clusters[0]["health"], "ok");
    assert_eq!(clusters[0]["nodes"][0]["node"], "alpha-1");
    assert_eq!(clusters[0]["nodes"][0]["local"], true);
    assert_eq!(clusters[1]["name"], "beta");
    assert_eq!(clusters[1]["regime"], "quorum");

    let unassigned = body["unassigned"].as_array().unwrap();
    assert_eq!(unassigned.len(), 1);
    assert_eq!(unassigned[0]["node"], "spare-1");
}

#[tokio::test]
async fn an_unreachable_cluster_is_listed_with_its_reason_not_dropped() {
    let harness = harness(
        "unreachable",
        MockBackend::environment().with_unreachable_cluster("beta"),
        MockPeers::new(),
        Some(&environment_membership()),
    );
    let cookie = sign_in(&harness.router).await;
    let (_, body) = request(
        &harness.router,
        Method::GET,
        "/api/environment",
        Some(&cookie),
        None,
        None,
    )
    .await;
    let beta = &body["clusters"][1];
    assert_eq!(beta["health"], "unknown");
    assert!(beta["error"].as_str().is_some(), "{beta}");
    assert_eq!(beta["nodes"].as_array().unwrap().len(), 3);
}

// --- the join handshake -----------------------------------------------------

#[tokio::test]
async fn the_first_token_bootstraps_and_a_join_spends_it() {
    let harness = harness(
        "bootstrap",
        MockBackend::appliance(),
        MockPeers::new(),
        None,
    );
    let cookie = sign_in(&harness.router).await;

    // Minting bootstraps: the environment appears, with this node in it.
    let (status, minted) = request(
        &harness.router,
        Method::POST,
        "/api/environment/tokens",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "address": "192.168.10.1:8443" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    assert_eq!(minted["bootstrapped"], true);
    let token_text = minted["token"].as_str().unwrap();
    let token = JoinToken::decode(token_text).unwrap();
    assert_eq!(token.issuer, "192.168.10.1:8443");

    let (_, body) = request(
        &harness.router,
        Method::GET,
        "/api/environment",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(body["environment"]["nodes"], 1);

    // The issuer's peer endpoint admits the token holder — no ticket, the
    // token is the authentication.
    let join_request = serde_json::json!({
        "token_id": token.id,
        "secret": token.secret,
        "node": "alpha-2",
        "address": "192.168.10.2:8443",
        "controlplane_version": "test",
    });
    let (status, grant) = request(
        &harness.router,
        Method::POST,
        "/api/peer/join",
        None,
        None,
        Some(join_request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{grant}");
    assert!(grant["ca_pem"]
        .as_str()
        .unwrap()
        .contains("BEGIN CERTIFICATE"));
    assert!(grant["node_cert_pem"]
        .as_str()
        .unwrap()
        .contains("BEGIN CERTIFICATE"));
    assert_eq!(grant["membership"]["nodes"].as_array().unwrap().len(), 2);
    assert!(!grant["session_secret"].as_str().unwrap().is_empty());

    // One-time: the same token again is refused with a sentence.
    let (status, refused) = request(
        &harness.router,
        Method::POST,
        "/api/peer/join",
        None,
        None,
        Some(join_request),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(refused["error"].as_str().unwrap().contains("one-time"));
}

// --- peer authentication ----------------------------------------------------

#[tokio::test]
async fn peer_routes_take_peer_tickets_and_nothing_else() {
    let harness = harness(
        "peer-auth",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&environment_membership()),
    );
    let cookie = sign_in(&harness.router).await;

    // No credential at all: 401.
    let (status, _) = request(
        &harness.router,
        Method::POST,
        "/api/peer/preflight",
        None,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // An operator's session cookie is not a peer.
    let (status, _) = request(
        &harness.router,
        Method::POST,
        "/api/peer/preflight",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A peer ticket works on peer routes…
    let ticket = security::issue_peer_ticket(TICKET_SECRET, "alpha-2").unwrap();
    let (status, report) = request(
        &harness.router,
        Method::POST,
        "/api/peer/preflight",
        None,
        Some(&ticket),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["node"], "alpha-1");

    // …and is not a session anywhere else, even riding the cookie header.
    let (status, _) = request(
        &harness.router,
        Method::GET,
        "/api/environment",
        Some(&format!("lumen_auth={ticket}")),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// --- cluster create ---------------------------------------------------------

fn create_body(nodes: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "name": "alpha",
        "preferred_node": if nodes.len() == 2 { serde_json::json!(nodes[0]) } else { serde_json::Value::Null },
        "core": { "subnet": "10.10.0.0/24", "mtu": 9000 },
        "management": { "subnet": "192.168.10.0/24" },
        "members": nodes.iter().enumerate().map(|(i, node)| serde_json::json!({
            "node": node,
            "core_interface": "nic1",
            "core_address": format!("10.10.0.{}", i + 1),
            "management_interface": "nic0",
            "management_address": format!("192.168.10.{}", i + 1),
            "bmc_address": format!("10.20.0.{}", i + 1),
            "bmc_username": "ADMIN",
            "bmc_password": "fence-pw",
        })).collect::<Vec<_>>(),
    })
}

async fn poll_until_finished(router: &axum::Router, cookie: &str) -> serde_json::Value {
    for _ in 0..400 {
        let (status, progress) = request(
            router,
            Method::GET,
            "/api/environment/clusters/pending",
            Some(cookie),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        if progress["phase"] != "running" {
            return progress;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the create never finished");
}

#[tokio::test]
async fn a_cluster_create_reports_per_node_per_step_progress_and_completes() {
    let backend = Arc::new(MockBackend::appliance());
    // The harness needs the backend inside the service too; build peers
    // around the same instance so a completed start makes the cluster
    // observable.
    let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
    let peers = MockPeers::new()
        .with_backend(backend.clone())
        .with_healthy_node("alpha-1", "test", &["nic0", "nic1"])
        .with_healthy_node("alpha-2", "test", &["nic0", "nic1"]);

    // Rebuild the harness by hand: the shared-backend shape is this test's.
    let mut config = Config::from_env();
    config.webui_dir = std::env::temp_dir().join("lumen-webui-none");
    config.no_tls = true;
    config.session_ttl_secs = 3600;
    let state_dir = TempDir::new("create");
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
    ));
    let sys = Arc::new(lumen_sys::SysService::new(
        Arc::new(lumen_sys::backend::mock::MockPower::appliance()),
        Arc::new(lumen_sys::exec::MockExec::new()),
    ));
    let cluster = Arc::new(
        ClusterService::new(
            backend,
            Arc::new(peers),
            network.clone(),
            &state_dir.0,
            "test",
        )
        .with_node("alpha-1")
        .with_form_poll(Duration::from_millis(5))
        .with_environment(&membership),
    );
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
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
    }));

    let cookie = sign_in(&router).await;
    let (status, progress) = request(
        &router,
        Method::POST,
        "/api/environment/clusters",
        Some(&cookie),
        None,
        Some(create_body(&["alpha-1", "alpha-2"])),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{progress}");
    // The first answer already carries the whole plan.
    let steps = progress["steps"].as_array().unwrap();
    assert!(steps
        .iter()
        .any(|s| s["step"] == "preflight" && s["node"] == "alpha-2"));
    assert!(steps.iter().any(|s| s["step"] == "record"));

    let done = poll_until_finished(&router, &cookie).await;
    assert_eq!(done["phase"], "complete", "{done}");

    let (_, body) = request(
        &router,
        Method::GET,
        "/api/environment",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(body["clusters"][0]["name"], "alpha");
    assert_eq!(body["clusters"][0]["preferred_node"], "alpha-1");
    assert_eq!(body["unassigned"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_malformed_create_is_a_validation_answer_with_fields() {
    let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
    let harness = harness(
        "create-invalid",
        MockBackend::appliance(),
        MockPeers::new(),
        Some(&membership),
    );
    let cookie = sign_in(&harness.router).await;
    let mut body = create_body(&["alpha-1", "alpha-2"]);
    body["core"]["subnet"] = serde_json::json!("not-a-subnet");
    body["name"] = serde_json::json!("Bad Name");
    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters",
        Some(&cookie),
        None,
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let errors = answer["errors"].as_array().unwrap();
    assert!(errors.iter().any(|e| e["code"] == "invalid_subnet"));
}

// --- destroy and node removal -----------------------------------------------

#[tokio::test]
async fn destroy_needs_the_acknowledgement_and_tears_every_member_down() {
    let mut membership = membership_of(&[("alpha-1", Some("alpha")), ("alpha-2", Some("alpha"))]);
    // Store the definition the way a create would have.
    let request_body: lumen_cluster::ClusterCreate =
        serde_json::from_value(create_body(&["alpha-1", "alpha-2"])).unwrap();
    let (definition, networks, _) = request_body.build().unwrap();
    membership.clusters.push(lumen_cluster::ClusterRecord {
        definition,
        networks,
        fence_tests: Default::default(),
    });

    let harness = harness(
        "destroy",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&membership),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::DELETE,
        "/api/environment/clusters/alpha",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(answer["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["code"] == "unacknowledged_destructive_operation"));

    let (status, _) = request(
        &harness.router,
        Method::DELETE,
        "/api/environment/clusters/alpha",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "i_understand_this_may_lose_data": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.peers.torn_down().len(), 2);

    let (_, body) = request(
        &harness.router,
        Method::GET,
        "/api/environment",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert!(body["clusters"].as_array().unwrap().is_empty());
    assert_eq!(body["unassigned"].as_array().unwrap().len(), 2);
}

// --- fencing ----------------------------------------------------------------

/// A membership record that knows cluster "alpha" whole — assignments and
/// the stored definition, as a completed create leaves them.
fn alpha_membership() -> EnvironmentMembership {
    let mut membership = membership_of(&[("alpha-1", Some("alpha")), ("alpha-2", Some("alpha"))]);
    let request_body: lumen_cluster::ClusterCreate =
        serde_json::from_value(create_body(&["alpha-1", "alpha-2"])).unwrap();
    let (definition, networks, _) = request_body.build().unwrap();
    membership.clusters.push(lumen_cluster::ClusterRecord {
        definition,
        networks,
        fence_tests: Default::default(),
    });
    membership
}

#[tokio::test]
async fn a_fence_test_needs_the_acknowledgement_and_its_answer_reaches_the_view() {
    let harness = harness(
        "fence-test",
        MockBackend::environment().with_untested_fencing("alpha"),
        MockPeers::new(),
        Some(&alpha_membership()),
    );
    let cookie = sign_in(&harness.router).await;

    // Without the acknowledgement: a validation answer naming the field.
    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/fence/alpha-2/test",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(answer["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["field"] == "i_understand_this_power_cycles_the_node"));

    // Acknowledged: the node is fenced for real and the answer says so.
    let (status, outcome) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/fence/alpha-2/test",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "i_understand_this_power_cycles_the_node": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{outcome}");
    assert_eq!(outcome["passed"], true);
    assert_eq!(outcome["node"], "alpha-2");

    // The environment view now shows this direction proven and the other
    // still pinned by the untested warning.
    let (_, body) = request(
        &harness.router,
        Method::GET,
        "/api/environment",
        Some(&cookie),
        None,
        None,
    )
    .await;
    let alpha = &body["clusters"][0];
    assert_eq!(alpha["fence"]["untested"], 1, "{alpha}");
    let tested = alpha["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["node"] == "alpha-2")
        .unwrap();
    assert_eq!(tested["fence"]["last_test"]["passed"], true);

    // A node's own direction is refused with the sentence that says where
    // to run it instead.
    let (status, refused) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/fence/alpha-1/test",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "i_understand_this_power_cycles_the_node": true })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(refused["error"]
        .as_str()
        .unwrap()
        .contains("another member"));
}

#[tokio::test]
async fn break_glass_confirms_only_an_unfenced_unreachable_peer() {
    let harness = harness(
        "break-glass",
        MockBackend::environment()
            .with_partition("alpha", "alpha-2")
            .with_fence_failure("alpha", "alpha-2"),
        MockPeers::new(),
        Some(&alpha_membership()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/nodes/alpha-2/confirm-dead",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(answer["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["field"] == "i_have_verified_the_node_is_powered_off"));

    let (status, confirmed) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/nodes/alpha-2/confirm-dead",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "i_have_verified_the_node_is_powered_off": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
    assert_eq!(confirmed["confirmed"], true);

    // Confirmed once, the node is no longer unclean — a second confirmation
    // has nothing to vouch for and is refused.
    let (status, refused) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/nodes/alpha-2/confirm-dead",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "i_have_verified_the_node_is_powered_off": true })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(refused["error"].as_str().unwrap().contains("not waiting"));
}

#[tokio::test]
async fn an_unassigned_node_can_be_removed_but_a_member_cannot() {
    let membership = membership_of(&[
        ("alpha-1", None),
        ("beta-1", Some("beta")),
        ("spare-1", None),
    ]);
    let harness = harness(
        "remove",
        MockBackend::appliance(),
        MockPeers::new(),
        Some(&membership),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, _) = request(
        &harness.router,
        Method::DELETE,
        "/api/environment/nodes/spare-1",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, answer) = request(
        &harness.router,
        Method::DELETE,
        "/api/environment/nodes/beta-1",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(answer["error"].as_str().unwrap().contains("beta"));
}
