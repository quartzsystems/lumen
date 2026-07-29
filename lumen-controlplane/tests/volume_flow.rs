//! End-to-end tests for replicated volumes: the grouped-by-cluster read, the
//! create/destroy/resize workflows over the real router, and the peer-ticket
//! rule on the volume half of the peer surface — every domain on its
//! in-memory backend, nothing touched on the machine running them.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use lumen_cluster::backend::mock::membership_of;
use lumen_cluster::networks::{AddressedMember, ClusterNetworks, CoreNetwork, ManagementNetwork};
use lumen_cluster::{
    BmcConfig, ClusterDefinition, ClusterRecord, ClusterService, EnvironmentMembership, MemberNode,
};
use lumen_controlplane::config::Config;
use lumen_controlplane::realm::{AuthFailure, Realm, RealmKind, RealmRegistry};
use lumen_controlplane::security;
use lumen_controlplane::{app, AppState};
use lumen_drbd::backend::mock::MockBackend as DrbdMockBackend;
use lumen_drbd::{DrbdService, MockVolumePeers};
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

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lumen-volume-flow-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

/// A membership whose two-node cluster "alpha" is recorded whole, with this
/// node (alpha-1) a member — the shape a completed cluster create leaves.
fn alpha_membership() -> EnvironmentMembership {
    let mut membership = membership_of(&[("alpha-1", Some("alpha")), ("alpha-2", Some("alpha"))]);
    let member = |name: &str, octet: u8| MemberNode {
        name: name.into(),
        ring0: std::net::Ipv4Addr::new(10, 10, 0, octet),
        ring1: std::net::Ipv4Addr::new(192, 168, 10, octet),
        bmc: BmcConfig {
            address: format!("10.20.0.{octet}"),
            username: "ADMIN".into(),
        },
    };
    let core = |name: &str, octet: u8| AddressedMember {
        node: name.into(),
        interface: "nic1".into(),
        address: std::net::Ipv4Addr::new(10, 10, 0, octet),
    };
    membership.clusters.push(ClusterRecord::new(
        ClusterDefinition {
            name: "alpha".into(),
            nodes: vec![member("alpha-1", 1), member("alpha-2", 2)],
            preferred_node: Some("alpha-1".into()),
        },
        ClusterNetworks {
            core: CoreNetwork {
                subnet: "10.10.0.0/24".parse().unwrap(),
                mtu: 9000,
                members: vec![core("alpha-1", 1), core("alpha-2", 2)],
            },
            management: ManagementNetwork {
                subnet: "192.168.10.0/24".parse().unwrap(),
                vip: None,
                members: Vec::new(),
            },
            external: Vec::new(),
        },
    ));
    membership
}

struct Harness {
    router: axum::Router,
    drbd_backend: Arc<DrbdMockBackend>,
    peers: Arc<MockVolumePeers>,
    cluster: Arc<ClusterService>,
    cluster_peers: Arc<lumen_cluster::MockPeers>,
    _state_dir: TempDir,
}

fn harness(tag: &str, membership: &EnvironmentMembership) -> Harness {
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
    let cluster_peers = Arc::new(lumen_cluster::MockPeers::new());
    let cluster = Arc::new(
        ClusterService::new(
            Arc::new(lumen_cluster::backend::mock::MockBackend::environment()),
            cluster_peers.clone(),
            network.clone(),
            &state_dir.0,
            "test",
        )
        .with_node("alpha-1")
        .with_environment(membership),
    );
    let drbd_backend = Arc::new(DrbdMockBackend::appliance());
    let peers = Arc::new(MockVolumePeers::new().with_backend(drbd_backend.clone()));
    let drbd = Arc::new(DrbdService::new(
        drbd_backend.clone(),
        peers.clone(),
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
        cluster: cluster.clone(),
        peers: Arc::new(lumen_controlplane::inventory::NoPeers),
        drbd,
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
        updates: Arc::new(lumen_update::UpdateService::new(
            Arc::new(lumen_update::MockUpdates::new()),
            "test-node",
        )),
        drain: Default::default(),
        update_job: Default::default(),
        roll: Default::default(),
    }));
    Harness {
        router,
        drbd_backend,
        peers,
        cluster,
        cluster_peers,
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

fn create_body(name: &str, size: u64) -> serde_json::Value {
    serde_json::json!({
        "cluster": "alpha",
        "name": name,
        "size_bytes": size,
        "members": [
            { "node": "alpha-1", "pool": "boot" },
            { "node": "alpha-2", "pool": "boot" },
        ],
    })
}

#[tokio::test]
async fn every_volume_route_requires_a_session() {
    let harness = harness("auth", &alpha_membership());
    for (method, path) in [
        (Method::GET, "/api/storage/replicated"),
        (Method::POST, "/api/storage/replicated"),
        (Method::DELETE, "/api/storage/replicated/alpha/v0"),
        (Method::POST, "/api/storage/replicated/alpha/v0/resize"),
    ] {
        let (status, _) = request(&harness.router, method.clone(), path, None, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
}

#[tokio::test]
async fn a_volume_is_created_listed_grown_and_destroyed_through_the_api() {
    let harness = harness("lifecycle", &alpha_membership());
    let cookie = sign_in(&harness.router).await;

    let (status, view) = request(
        &harness.router,
        Method::POST,
        "/api/storage/replicated",
        Some(&cookie),
        None,
        Some(create_body("vm-101-disk-0", 1 << 30)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["resource"], "alpha-vm-101-disk-0");
    assert_eq!(view["device"], "/dev/drbd1");
    assert_eq!(view["health"], "up_to_date");
    // Both members prepared, initial sync skipped once.
    assert_eq!(harness.peers.prepared().len(), 2);
    assert_eq!(harness.peers.primed().len(), 1);

    let (status, listed) = request(
        &harness.router,
        Method::GET,
        "/api/storage/replicated",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["clusters"][0]["cluster"], "alpha");
    let volume = &listed["clusters"][0]["volumes"][0];
    assert_eq!(volume["name"], "vm-101-disk-0");
    assert_eq!(volume["replicas"].as_array().unwrap().len(), 2, "{volume}");

    // Grow it; shrinking through the same route is refused.
    let (status, _) = request(
        &harness.router,
        Method::POST,
        "/api/storage/replicated/alpha/vm-101-disk-0/resize",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "size_bytes": 4u64 << 30 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.peers.grown().len(), 1);
    let (status, refused) = request(
        &harness.router,
        Method::POST,
        "/api/storage/replicated/alpha/vm-101-disk-0/resize",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "size_bytes": 1u64 << 30 })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");

    // Destroy: refused without the acknowledgement, complete with it.
    let (status, answer) = request(
        &harness.router,
        Method::DELETE,
        "/api/storage/replicated/alpha/vm-101-disk-0",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    let (status, _) = request(
        &harness.router,
        Method::DELETE,
        "/api/storage/replicated/alpha/vm-101-disk-0",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "i_understand_this_may_lose_data": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.peers.torn_down().len(), 2);

    let (_, listed) = request(
        &harness.router,
        Method::GET,
        "/api/storage/replicated",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert!(listed["clusters"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_malformed_create_is_a_validation_answer_with_fields() {
    let harness = harness("invalid", &alpha_membership());
    let cookie = sign_in(&harness.router).await;
    let mut body = create_body("Bad Name", 0);
    body["members"] = serde_json::json!([{ "node": "alpha-1", "pool": "boot" }]);
    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/storage/replicated",
        Some(&cookie),
        None,
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    let errors = answer["errors"].as_array().unwrap();
    let codes: Vec<&str> = errors.iter().filter_map(|e| e["code"].as_str()).collect();
    assert!(codes.contains(&"invalid_volume_name"), "{codes:?}");
    assert!(codes.contains(&"invalid_volume_size"), "{codes:?}");
    assert!(harness.peers.prepared().is_empty());
}

#[tokio::test]
async fn a_machines_definition_replicates_to_co_members_and_is_withdrawn() {
    let harness = harness("definitions", &alpha_membership());
    let cookie = sign_in(&harness.router).await;

    let (status, vm) = request(
        &harness.router,
        Method::POST,
        "/api/vms",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "name": "web01", "vcpus": 1, "memory_mib": 1024 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{vm}");
    let vmid = vm["vmid"].as_u64().unwrap() as u32;

    // The definition went to the cluster's other member and stayed here too
    // — the HA manager's restart inventory, filled at define time, home
    // node riding along.
    let pushed = harness.cluster_peers.definitions();
    assert_eq!(pushed.len(), 1, "{pushed:?}");
    assert_eq!(pushed[0].0, "alpha-2");
    assert_eq!(pushed[0].1.vmid, vmid);
    assert_eq!(pushed[0].1.node, "alpha-1", "the home travels with it");
    assert!(
        pushed[0].1.xml.contains("web01"),
        "the document itself travels"
    );
    assert_eq!(harness.cluster.stored_definitions().unwrap().len(), 1);

    // Deleting the machine withdraws it everywhere: a stored definition for
    // a machine that no longer exists is a machine waiting to be wrongly
    // resurrected.
    let (status, _) = request(
        &harness.router,
        Method::DELETE,
        &format!("/api/vms/{vmid}"),
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        harness.cluster_peers.dropped_definitions(),
        vec![("alpha-2".to_string(), vmid)]
    );
    assert!(harness.cluster.stored_definitions().unwrap().is_empty());
}

#[tokio::test]
async fn peer_definition_routes_take_peer_tickets_and_store() {
    let harness = harness("peer-definitions", &alpha_membership());
    let body = serde_json::json!({ "vmid": 42, "node": "alpha-2", "xml": "<domain/>" });

    let (status, _) = request(
        &harness.router,
        Method::POST,
        "/api/peer/definition/store",
        None,
        None,
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let ticket = security::issue_peer_ticket(TICKET_SECRET, "alpha-2").unwrap();
    let (status, _) = request(
        &harness.router,
        Method::POST,
        "/api/peer/definition/store",
        None,
        Some(&ticket),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let stored = harness.cluster.stored_definitions().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].vmid, 42);
    assert_eq!(stored[0].node, "alpha-2");

    let (status, _) = request(
        &harness.router,
        Method::POST,
        "/api/peer/definition/drop",
        None,
        Some(&ticket),
        Some(serde_json::json!({ "vmid": 42 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(harness.cluster.stored_definitions().unwrap().is_empty());
}

#[tokio::test]
async fn peer_volume_routes_take_peer_tickets_and_nothing_else() {
    let harness = harness("peer-auth", &alpha_membership());
    let cookie = sign_in(&harness.router).await;
    let body = serde_json::json!({ "resource": "alpha-v0" });

    let (status, _) = request(
        &harness.router,
        Method::POST,
        "/api/peer/volume/prime",
        None,
        None,
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // An operator's session is not a peer.
    let (status, _) = request(
        &harness.router,
        Method::POST,
        "/api/peer/volume/prime",
        Some(&cookie),
        None,
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let ticket = security::issue_peer_ticket(TICKET_SECRET, "alpha-2").unwrap();
    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/peer/volume/prime",
        None,
        Some(&ticket),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(harness.drbd_backend.primed(), vec!["alpha-v0"]);
}
