//! End-to-end tests for the environment and cluster reads, over the real
//! router with every domain on its in-memory backend — no corosync, no
//! Pacemaker, nothing touched on the machine running them.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use lumen_cluster::backend::mock::MockBackend as MockClusterBackend;
use lumen_cluster::ClusterService;
use lumen_controlplane::config::Config;
use lumen_controlplane::realm::{AuthFailure, Realm, RealmKind, RealmRegistry};
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

/// A router around the given cluster backend; every other domain is its
/// appliance-shaped mock.
fn test_app(cluster_backend: MockClusterBackend) -> axum::Router {
    let mut config = Config::from_env();
    config.webui_dir = std::env::temp_dir().join("lumen-webui-none");
    config.no_tls = true;
    config.session_ttl_secs = 3600;
    let state_dir = std::env::temp_dir().join("lumen-cluster-flow-state");
    let network = Arc::new(NetworkService::new(
        Arc::new(lumen_net::backend::mock::MockBackend::appliance()),
        &state_dir,
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
    // The tests claim to be alpha-1 — a member of the mock scenario's
    // two-node cluster — so the `local` markers have something to mark.
    let cluster =
        Arc::new(ClusterService::new(Arc::new(cluster_backend), "test").with_node("alpha-1"));
    let state = AppState {
        config,
        jwt_secret: TICKET_SECRET.to_vec(),
        realms: RealmRegistry::new().register(Box::new(MockRealm)),
        sys,
        network,
        storage,
        virt,
        cluster,
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
    };
    app(Arc::new(state))
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

async fn get_json(
    router: &axum::Router,
    cookie: &str,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(
            Request::get(path)
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

#[tokio::test]
async fn every_environment_route_requires_a_session() {
    let router = test_app(MockClusterBackend::environment());
    for path in ["/api/environment", "/api/environment/clusters/alpha"] {
        let response = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn a_standalone_node_answers_with_itself_as_the_one_unassigned_node() {
    let router = test_app(MockClusterBackend::appliance());
    let cookie = sign_in(&router).await;
    let (status, body) = get_json(&router, &cookie, "/api/environment").await;
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
    let router = test_app(MockClusterBackend::environment());
    let cookie = sign_in(&router).await;
    let (status, body) = get_json(&router, &cookie, "/api/environment").await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(body["environment"]["nodes"], 6);
    let clusters = body["clusters"].as_array().unwrap();
    assert_eq!(clusters.len(), 2);

    assert_eq!(clusters[0]["name"], "alpha");
    assert_eq!(clusters[0]["regime"], "two_node");
    assert_eq!(clusters[0]["health"], "ok");
    assert_eq!(clusters[0]["quorum"]["two_node"], true);
    assert_eq!(clusters[0]["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(clusters[0]["nodes"][0]["node"], "alpha-1");
    assert_eq!(clusters[0]["nodes"][0]["local"], true);
    assert_eq!(clusters[0]["nodes"][0]["fence"]["device"], "fence-alpha-1");

    assert_eq!(clusters[1]["name"], "beta");
    assert_eq!(clusters[1]["regime"], "quorum");
    assert_eq!(clusters[1]["nodes"].as_array().unwrap().len(), 3);

    let unassigned = body["unassigned"].as_array().unwrap();
    assert_eq!(unassigned.len(), 1);
    assert_eq!(unassigned[0]["node"], "spare-1");
    assert_eq!(unassigned[0]["local"], false);
}

#[tokio::test]
async fn a_partitioned_cluster_reads_critical_with_the_lost_node_marked() {
    let router = test_app(MockClusterBackend::environment().with_partition("alpha", "alpha-2"));
    let cookie = sign_in(&router).await;
    let (status, body) = get_json(&router, &cookie, "/api/environment/clusters/alpha").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["health"], "critical");
    let lost = &body["nodes"][1];
    assert_eq!(lost["node"], "alpha-2");
    assert_eq!(lost["online"], false);
    assert_eq!(lost["unclean"], true);
}

#[tokio::test]
async fn an_unreachable_cluster_is_listed_with_its_reason_not_dropped() {
    let router = test_app(MockClusterBackend::environment().with_unreachable_cluster("beta"));
    let cookie = sign_in(&router).await;
    let (status, body) = get_json(&router, &cookie, "/api/environment").await;
    assert_eq!(status, StatusCode::OK);
    let beta = &body["clusters"][1];
    assert_eq!(beta["name"], "beta");
    assert_eq!(beta["health"], "unknown");
    assert!(beta["error"].as_str().is_some(), "{beta}");
    assert_eq!(beta["nodes"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn asking_for_a_cluster_that_is_not_there_is_a_404_with_a_sentence() {
    let router = test_app(MockClusterBackend::environment());
    let cookie = sign_in(&router).await;
    let (status, body) = get_json(&router, &cookie, "/api/environment/clusters/gamma").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("gamma"));
}
