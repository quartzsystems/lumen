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
use lumen_cluster::networks::{
    AddressedMember, ClusterNetworks, CoreNetwork, ExternalNetwork, ManagementNetwork, Uplink,
    VlanMode,
};
use lumen_cluster::{
    BmcConfig, ClusterDefinition, ClusterRecord, ClusterService, EnvironmentMembership, JoinToken,
    MemberNode, MockPeers,
};
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
    harness_with_storage(
        tag,
        backend,
        peers,
        membership,
        Arc::new(lumen_zfs::backend::mock::MockBackend::appliance()),
    )
}

/// The same, with the node's disks arranged by the caller — for the tests
/// about clearing one, which need a node that actually has a disk in the
/// state a wipe is offered in.
fn harness_with_storage(
    tag: &str,
    backend: MockBackend,
    peers: MockPeers,
    membership: Option<&EnvironmentMembership>,
    disks: Arc<lumen_zfs::backend::mock::MockBackend>,
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
    let storage = Arc::new(StorageService::new(disks).with_root_pool(Some("boot".into())));
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
    let cluster = Arc::new(cluster);
    let drbd = Arc::new(lumen_drbd::DrbdService::new(
        Arc::new(lumen_drbd::backend::mock::MockBackend::appliance()),
        Arc::new(lumen_drbd::MockVolumePeers::new()),
        cluster.clone(),
        storage.clone(),
    ));
    let state = AppState {
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
        pool: lumen_controlplane::pool::PoolPresence::Absent,
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
        updates: Arc::new(lumen_update::UpdateService::new(
            Arc::new(lumen_update::MockUpdates::new()),
            "test-node",
        )),
        drain: Default::default(),
        update_job: Default::default(),
        roll: Default::default(),
        pool_deploy: std::sync::Arc::new(lumen_pool::PoolDeploy::new(
            lumen_sys::exec::MockExec::working(),
        )),
        pool_peers: std::sync::Arc::new(lumen_controlplane::inventory::NoPeers),
        pool_job: Default::default(),
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
        (Method::GET, "/api/environment/clusters/alpha/networks"),
        (Method::POST, "/api/environment/tokens"),
        (Method::POST, "/api/environment/join"),
        (Method::POST, "/api/environment/preflight"),
        (Method::POST, "/api/environment/nodes/alpha-2/bond"),
        (Method::POST, "/api/environment/clusters"),
        (Method::GET, "/api/environment/clusters/pending"),
        (Method::DELETE, "/api/environment/clusters/alpha"),
        (Method::DELETE, "/api/environment/nodes/spare-1"),
        (Method::POST, "/api/environment/clusters/alpha/nodes"),
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

/// The environment-wide read answers for a node that has joined nothing:
/// one member, itself, reachable. A console that has to special-case
/// "standalone" before it can draw a table is a console that will get it
/// wrong the day a second node arrives.
#[tokio::test]
async fn the_inventory_of_a_standalone_node_is_itself_alone() {
    let harness = harness(
        "inventory-standalone",
        MockBackend::appliance(),
        MockPeers::new(),
        None,
    );
    let cookie = sign_in(&harness.router).await;
    let (status, body) = request(
        &harness.router,
        Method::GET,
        "/api/environment/inventory",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let members = body["members"].as_array().unwrap();
    assert_eq!(members.len(), 1, "{body}");
    assert_eq!(members[0]["node"], "alpha-1");
    assert_eq!(members[0]["local"], true);
    assert_eq!(members[0]["reachable"], true);
    assert!(members[0]["inventory"].is_object(), "{body}");
}

/// A member this control plane cannot reach is a row with a reason on it,
/// not a failed request. The whole point of the endpoint is that one node
/// being away does not cost the operator the nodes that answered — so the
/// call is still 200, the local node still carries its inventory, and the
/// unreachable one says why it has none.
#[tokio::test]
async fn a_member_that_cannot_be_asked_is_a_row_carrying_its_reason() {
    let harness = harness(
        "inventory-unreachable",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&environment_membership()),
    );
    let cookie = sign_in(&harness.router).await;
    let (status, body) = request(
        &harness.router,
        Method::GET,
        "/api/environment/inventory",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let members = body["members"].as_array().unwrap();
    assert!(members.len() > 1, "the fixture has more than one member");

    let local = members
        .iter()
        .find(|m| m["local"] == true)
        .expect("the local node");
    assert_eq!(local["node"], "alpha-1");
    assert_eq!(local["reachable"], true);
    assert!(local["inventory"].is_object(), "{body}");

    // The harness holds no peer channel, so every peer is honestly
    // unreachable — and says so rather than being dropped from the list.
    let peer = members
        .iter()
        .find(|m| m["local"] == false)
        .expect("a peer row");
    assert_eq!(peer["reachable"], false);
    assert!(peer["inventory"].is_null() || peer.get("inventory").is_none());
    assert!(
        peer["error"]
            .as_str()
            .unwrap()
            .contains("could not be asked"),
        "{peer}"
    );
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

/// A membership whose "alpha" record carries the typed networks whole — and
/// a "beta" the nodes name but no record has replicated for. The External
/// entry pins the flattened VLAN-mode wire shape the console's types mirror.
fn membership_with_networks() -> EnvironmentMembership {
    let mut membership = membership_of(&[
        ("alpha-1", Some("alpha")),
        ("alpha-2", Some("alpha")),
        ("beta-1", Some("beta")),
    ]);
    let member = |name: &str, octet: u8| MemberNode {
        name: name.into(),
        ring0: std::net::Ipv4Addr::new(10, 10, 0, octet),
        ring1: std::net::Ipv4Addr::new(192, 168, 10, octet),
        bmc: BmcConfig {
            address: format!("10.20.0.{octet}"),
            username: "ADMIN".into(),
        },
    };
    let seat = |name: &str, interface: &str, a: u8, b: u8, c: u8, octet: u8| AddressedMember {
        node: name.into(),
        interface: interface.into(),
        address: std::net::Ipv4Addr::new(a, b, c, octet),
    };
    membership.clusters.push(ClusterRecord::new(
        ClusterDefinition {
            name: "alpha".into(),
            nodes: vec![member("alpha-1", 1), member("alpha-2", 2)],
            preferred_node: None,
        },
        ClusterNetworks {
            core: CoreNetwork {
                subnet: "10.10.0.0/24".parse().unwrap(),
                mtu: 9000,
                members: vec![
                    seat("alpha-1", "nic1", 10, 10, 0, 1),
                    seat("alpha-2", "nic1", 10, 10, 0, 2),
                ],
            },
            management: ManagementNetwork {
                subnet: "192.168.10.0/24".parse().unwrap(),
                vip: Some(std::net::Ipv4Addr::new(192, 168, 10, 100)),
                members: vec![
                    seat("alpha-1", "br0", 192, 168, 10, 1),
                    seat("alpha-2", "br0", 192, 168, 10, 2),
                ],
            },
            external: vec![ExternalNetwork {
                name: "vm-lan".into(),
                bridge: "vmbr1".into(),
                vlan: VlanMode::Trunk {
                    allowed: vec![10, 20],
                },
                uplinks: vec![
                    Uplink {
                        node: "alpha-1".into(),
                        interface: "nic2".into(),
                    },
                    Uplink {
                        node: "alpha-2".into(),
                        interface: "nic2".into(),
                    },
                ],
            }],
        },
    ));
    membership
}

#[tokio::test]
async fn the_typed_networks_are_read_off_the_replicated_record() {
    let harness = harness(
        "networks",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&membership_with_networks()),
    );
    let cookie = sign_in(&harness.router).await;
    let (status, body) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/alpha/networks",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(body["core"]["subnet"], "10.10.0.0/24");
    assert_eq!(body["core"]["mtu"], 9000);
    assert_eq!(
        body["core"]["members"][0],
        serde_json::json!({ "node": "alpha-1", "interface": "nic1", "address": "10.10.0.1" })
    );
    assert_eq!(body["management"]["subnet"], "192.168.10.0/24");
    assert_eq!(body["management"]["vip"], "192.168.10.100");
    assert_eq!(body["management"]["members"][1]["interface"], "br0");
    // The VLAN mode is flattened onto the External object — the wire shape
    // the console's ExternalNetwork type mirrors.
    assert_eq!(
        body["external"][0],
        serde_json::json!({
            "name": "vm-lan",
            "bridge": "vmbr1",
            "mode": "trunk",
            "allowed": [10, 20],
            "uplinks": [
                { "node": "alpha-1", "interface": "nic2" },
                { "node": "alpha-2", "interface": "nic2" },
            ],
        })
    );

    // A cluster nobody has heard of is a 404; one the membership names but
    // whose record has not replicated here is a conflict, said as such.
    let (status, _) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/gamma/networks",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/beta/networks",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
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

/// The console mints with a bodiless POST, and a browser's `fetch` sends that
/// as `Content-Type: application/json` with zero bytes behind it. An optional
/// body has to stay optional when the request says it is sending JSON and then
/// sends none — this came back 400 out of the extractor, before the handler
/// that would have bootstrapped the environment ever ran.
///
/// Built by hand rather than through `request`, which only sets the header when
/// there is a body and so could never have caught this.
#[tokio::test]
async fn minting_takes_a_declared_but_empty_json_body() {
    let harness = harness(
        "empty-body",
        MockBackend::appliance(),
        MockPeers::new(),
        None,
    );
    let cookie = sign_in(&harness.router).await;

    let response = harness
        .router
        .clone()
        .oneshot(
            Request::post("/api/environment/tokens")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

// --- the Core-redundancy shortcut -------------------------------------------

/// The wizard's "bond these NICs for Core" reaches the named member through
/// the peer channel and lands in that node's own networking domain. The bond
/// is an ordinary link afterwards — nothing about it belongs to the cluster.
#[tokio::test]
async fn bonding_a_members_nics_goes_through_that_nodes_networking() {
    let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
    let peers = MockPeers::new()
        .with_healthy_node(
            "alpha-1",
            env!("CARGO_PKG_VERSION"),
            &["nic0", "nic1", "nic2"],
        )
        .with_healthy_node(
            "alpha-2",
            env!("CARGO_PKG_VERSION"),
            &["nic0", "nic1", "nic2"],
        );
    let harness = harness("bond", MockBackend::appliance(), peers, Some(&membership));
    let cookie = sign_in(&harness.router).await;

    let (status, body) = request(
        &harness.router,
        Method::POST,
        "/api/environment/nodes/alpha-2/bond",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "name": "bond0",
            "mode": "active-backup",
            "ports": ["nic1", "nic2"],
            "miimon": 100,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bonded"], true);

    // The node reports the bond now, so the wizard's Core picker can seat
    // ring 0 on it.
    let (status, views) = request(
        &harness.router,
        Method::POST,
        "/api/environment/preflight",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "nodes": ["alpha-2"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{views}");
    let links = views[0]["report"]["links"].as_array().unwrap();
    let bond = links
        .iter()
        .find(|link| link["name"] == "bond0")
        .unwrap_or_else(|| panic!("the bond is missing: {views}"));
    assert_eq!(bond["kind"], "bond");
}

#[tokio::test]
async fn a_bond_needs_two_ports_and_a_node_in_the_environment() {
    let membership = membership_of(&[("alpha-1", None), ("alpha-2", None)]);
    let peers = MockPeers::new().with_healthy_node(
        "alpha-2",
        env!("CARGO_PKG_VERSION"),
        &["nic0", "nic1", "nic2"],
    );
    let harness = harness(
        "bond-bad",
        MockBackend::appliance(),
        peers,
        Some(&membership),
    );
    let cookie = sign_in(&harness.router).await;

    for (node, ports) in [
        // One port is the cable the bond was meant to survive…
        ("alpha-2", vec!["nic1"]),
        // …and a node outside the environment is not ours to configure.
        ("nobody", vec!["nic1", "nic2"]),
    ] {
        let (status, answer) = request(
            &harness.router,
            Method::POST,
            &format!("/api/environment/nodes/{node}/bond"),
            Some(&cookie),
            None,
            Some(serde_json::json!({
                "name": "bond0", "mode": "active-backup", "ports": ports,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{node} {ports:?} → {answer}");
    }
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
        Arc::new(lumen_drbd::MockVmVolumes::standalone()),
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
        pool: lumen_controlplane::pool::PoolPresence::Absent,
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
        updates: Arc::new(lumen_update::UpdateService::new(
            Arc::new(lumen_update::MockUpdates::new()),
            "test-node",
        )),
        drain: Default::default(),
        update_job: Default::default(),
        roll: Default::default(),
        pool_deploy: std::sync::Arc::new(lumen_pool::PoolDeploy::new(
            lumen_sys::exec::MockExec::working(),
        )),
        pool_peers: std::sync::Arc::new(lumen_controlplane::inventory::NoPeers),
        pool_job: Default::default(),
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

// --- pooling drives across members ------------------------------------------

/// Reformatting disks is not something to do because a field defaulted. The
/// acknowledgement is required before any member is touched.
#[tokio::test]
async fn pooling_across_members_needs_the_acknowledgement() {
    let harness = harness(
        "pool-unacked",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&alpha_with_record()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/storage/pools",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "name": "tank",
            "vdev": "mirror",
            "compression": "lz4",
            "seats": [{ "node": "alpha-1", "disks": ["/dev/disk/by-id/one"] }],
        })),
    )
    .await;
    assert_ne!(status, StatusCode::CREATED, "{answer}");
}

/// A member given no disks is a mistake, not an empty pool. Caught before
/// anything is built rather than leaving one member with a pool the others
/// do not have — which is exactly the drift this endpoint exists to prevent.
#[tokio::test]
async fn a_member_with_no_disks_is_refused() {
    let harness = harness(
        "pool-empty-seat",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&alpha_with_record()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/storage/pools",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "name": "tank",
            "vdev": "mirror",
            "compression": "lz4",
            "seats": [
                { "node": "alpha-1", "disks": ["/dev/disk/by-id/one"] },
                { "node": "alpha-2", "disks": [] },
            ],
            "i_understand_this_erases_the_disks": true,
        })),
    )
    .await;
    assert_ne!(status, StatusCode::CREATED, "{answer}");
}

/// A node that is not in the environment cannot be given disks to format.
#[tokio::test]
async fn a_stranger_cannot_be_given_disks_to_format() {
    let harness = harness(
        "pool-stranger",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&alpha_with_record()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/storage/pools",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "name": "tank",
            "vdev": "mirror",
            "compression": "lz4",
            "seats": [{ "node": "outsider", "disks": ["/dev/disk/by-id/one"] }],
            "i_understand_this_erases_the_disks": true,
        })),
    )
    .await;
    assert_ne!(status, StatusCode::CREATED, "{answer}");
}

// --- external networks -------------------------------------------------------

/// A membership whose cluster "alpha" is recorded whole, so the External
/// network verbs have a definition to add to.
fn alpha_with_record() -> EnvironmentMembership {
    let mut membership = membership_of(&[("alpha-1", Some("alpha")), ("alpha-2", Some("alpha"))]);
    let request_body: lumen_cluster::ClusterCreate =
        serde_json::from_value(create_body(&["alpha-1", "alpha-2"])).unwrap();
    let (definition, networks, _) = request_body.build().unwrap();
    membership
        .clusters
        .push(lumen_cluster::ClusterRecord::new(definition, networks));
    membership
}

/// The whole point of the type: every member builds the bridge, and only then
/// does the record admit the network exists.
#[tokio::test]
async fn an_external_network_is_built_on_every_member_before_it_is_recorded() {
    let peers = MockPeers::new();
    let harness = harness(
        "external-create",
        MockBackend::environment(),
        peers,
        Some(&alpha_with_record()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/networks/external",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "name": "vm-net",
            "bridge": "vmbr1",
            "mode": "trunk",
            "allowed": [10, 20],
            "uplinks": [
                { "node": "alpha-1", "interface": "nic3" },
                { "node": "alpha-2", "interface": "nic3" },
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    assert_eq!(answer["name"], "vm-net");

    // Built on both members, as a trunk — VLAN aware, no VLAN interface
    // underneath, because the tags are meant to reach the machines.
    let built = harness.peers.bridges();
    assert_eq!(built.len(), 2, "one bridge per member");
    assert!(built.iter().any(|(node, _)| node == "alpha-1"));
    assert!(built.iter().any(|(node, _)| node == "alpha-2"));
    for (_, seat) in &built {
        assert_eq!(seat.bridge.name, "vmbr1");
        assert!(seat.bridge.vlan_filtering, "a trunk is VLAN aware");
        assert!(seat.vlan.is_none(), "a trunk needs no VLAN interface");
        assert_eq!(seat.bridge.ports, vec!["nic3".to_string()]);
    }

    // And now it is in the record every member reads.
    let (status, networks) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/alpha/networks",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let external = networks["external"].as_array().unwrap();
    assert_eq!(external.len(), 1, "{networks}");
    assert_eq!(external[0]["name"], "vm-net");
}

/// An access network's bridge sits on a VLAN interface, not on the raw
/// uplink. Bridging the uplink directly would put the machines on whatever
/// the switch sends untagged, which is not the VLAN that was asked for.
#[tokio::test]
async fn an_access_network_bridges_a_vlan_interface_rather_than_the_uplink() {
    let harness = harness(
        "external-access",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&alpha_with_record()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/networks/external",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "name": "office",
            "bridge": "vmbr2",
            "mode": "access",
            "vlan": 30,
            "uplinks": [
                { "node": "alpha-1", "interface": "nic3" },
                { "node": "alpha-2", "interface": "nic3" },
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");

    for (_, seat) in harness.peers.bridges() {
        let vlan = seat.vlan.as_ref().expect("an access network needs one");
        assert_eq!(vlan.name, "nic3.30");
        assert_eq!(vlan.parent, "nic3");
        assert_eq!(vlan.vlan_id, 30);
        // The bridge takes the VLAN interface as its port, and does no
        // filtering of its own — the tag is the VLAN interface's job.
        assert_eq!(seat.bridge.ports, vec!["nic3.30".to_string()]);
        assert!(!seat.bridge.vlan_filtering);
    }
}

/// Every member or none. A definition missing a member is refused before
/// anything is built, rather than leaving a network a failover can land on
/// and find absent.
#[tokio::test]
async fn an_external_network_missing_a_member_is_refused_and_builds_nothing() {
    let harness = harness(
        "external-partial",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&alpha_with_record()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/networks/external",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "name": "half",
            "bridge": "vmbr3",
            "mode": "trunk",
            "allowed": [],
            "uplinks": [{ "node": "alpha-1", "interface": "nic3" }],
        })),
    )
    .await;
    assert_ne!(status, StatusCode::CREATED, "{answer}");
    assert!(
        harness.peers.bridges().is_empty(),
        "nothing may have been built"
    );
}

/// A member that cannot build its bridge fails the whole call, and the record
/// does not gain a network the cluster has not got.
#[tokio::test]
async fn a_member_that_cannot_build_the_bridge_fails_the_definition() {
    let harness = harness(
        "external-fails",
        MockBackend::environment(),
        MockPeers::new().fail_bridge_on("alpha-2"),
        Some(&alpha_with_record()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/networks/external",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "name": "doomed",
            "bridge": "vmbr4",
            "mode": "trunk",
            "allowed": [],
            "uplinks": [
                { "node": "alpha-1", "interface": "nic3" },
                { "node": "alpha-2", "interface": "nic3" },
            ],
        })),
    )
    .await;
    assert_ne!(status, StatusCode::CREATED, "{answer}");

    let (_, networks) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/alpha/networks",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert!(
        networks["external"].as_array().unwrap().is_empty(),
        "an unrealized network may not be recorded: {networks}"
    );
}

// --- destroy and node removal -----------------------------------------------

#[tokio::test]
async fn destroy_needs_the_acknowledgement_and_tears_every_member_down() {
    let mut membership = membership_of(&[("alpha-1", Some("alpha")), ("alpha-2", Some("alpha"))]);
    // Store the definition the way a create would have.
    let request_body: lumen_cluster::ClusterCreate =
        serde_json::from_value(create_body(&["alpha-1", "alpha-2"])).unwrap();
    let (definition, networks, _) = request_body.build().unwrap();
    membership
        .clusters
        .push(lumen_cluster::ClusterRecord::new(definition, networks));

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
    membership
        .clusters
        .push(lumen_cluster::ClusterRecord::new(definition, networks));
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
async fn a_node_add_validates_before_anything_runs() {
    let harness = harness(
        "add-node",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&alpha_membership()),
    );
    let cookie = sign_in(&harness.router).await;
    let member = serde_json::json!({
        "node": "ghost",
        "core_interface": "nic1",
        "core_address": "10.10.0.3",
        "management_interface": "nic0",
        "management_address": "192.168.10.3",
        "bmc_address": "10.20.0.3",
        "bmc_username": "ADMIN",
        "bmc_password": "pw",
    });
    // A stranger is refused before any workflow starts.
    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/nodes",
        Some(&cookie),
        None,
        Some(member),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert!(answer["error"]
        .as_str()
        .unwrap()
        .contains("has not joined this environment"));
    // And an unknown cluster is a 404, not a workflow.
    let (status, _) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/ghost/nodes",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "node": "alpha-2",
            "core_interface": "nic1",
            "core_address": "10.10.0.9",
            "management_interface": "nic0",
            "management_address": "192.168.10.9",
            "bmc_address": "10.20.0.9",
            "bmc_username": "ADMIN",
            "bmc_password": "pw",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
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

// --- changing an external network --------------------------------------------

/// The whole rule, on the way back out: a change is built on every member
/// before the record admits it, exactly as a create is.
#[tokio::test]
async fn changing_an_external_network_rebuilds_it_on_every_member() {
    let harness = harness(
        "external-update",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&membership_with_networks()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::PUT,
        "/api/environment/clusters/alpha/networks/external/vm-lan",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "name": "vm-lan",
            "bridge": "vmbr9",
            "mode": "access",
            "vlan": 30,
            "uplinks": [
                { "node": "alpha-1", "interface": "nic4" },
                { "node": "alpha-2", "interface": "nic4" },
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["bridge"], "vmbr9");
    assert_eq!(answer["mode"], "access");

    // Rebuilt on both, and as an access network this time: the bridge sits on
    // a VLAN interface rather than on the raw uplink.
    let built = harness.peers.bridges();
    assert_eq!(built.len(), 2, "one bridge per member");
    for (_, seat) in &built {
        assert_eq!(seat.bridge.name, "vmbr9");
        assert!(
            !seat.bridge.vlan_filtering,
            "an access network is not a trunk"
        );
        assert_eq!(
            seat.vlan.as_ref().map(|vlan| vlan.vlan_id),
            Some(30),
            "the tag is put on by a VLAN interface underneath"
        );
        assert_eq!(seat.bridge.ports, vec!["nic4.30".to_string()]);
    }

    let (_, networks) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/alpha/networks",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(networks["external"][0]["bridge"], "vmbr9");
    assert_eq!(networks["external"][0]["vlan"], 30);
}

/// The name is what a machine's adapter refers to. Renaming would leave every
/// machine pointing at a network that no longer exists, so it is refused
/// rather than quietly accepted and rebuilt under a new identity.
#[tokio::test]
async fn an_external_network_cannot_be_renamed_through_an_edit() {
    let harness = harness(
        "external-rename",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&membership_with_networks()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::PUT,
        "/api/environment/clusters/alpha/networks/external/vm-lan",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "name": "vm-lan-2",
            "bridge": "vmbr1",
            "mode": "trunk",
            "allowed": [10, 20],
            "uplinks": [
                { "node": "alpha-1", "interface": "nic2" },
                { "node": "alpha-2", "interface": "nic2" },
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .unwrap()
            .contains("cannot be changed"),
        "{answer}"
    );
    assert!(harness.peers.bridges().is_empty(), "nothing was rebuilt");
}

/// The every-member rule holds for a change too — and a member left without an
/// uplink is caught before anything is built, not halfway through.
#[tokio::test]
async fn an_edit_that_drops_a_member_is_refused_and_builds_nothing() {
    let harness = harness(
        "external-update-partial",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&membership_with_networks()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::PUT,
        "/api/environment/clusters/alpha/networks/external/vm-lan",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "name": "vm-lan",
            "bridge": "vmbr1",
            "mode": "trunk",
            "allowed": [10],
            "uplinks": [{ "node": "alpha-1", "interface": "nic2" }],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert!(
        answer["error"].as_str().unwrap().contains("alpha-2"),
        "{answer}"
    );
    assert!(harness.peers.bridges().is_empty(), "nothing was built");
}

/// Removing one forgets the definition and leaves the links. The bridges are
/// ordinary links with machines possibly still attached, and the answer says
/// so rather than letting an operator find a stray link later and wonder.
#[tokio::test]
async fn forgetting_an_external_network_leaves_its_bridges_alone() {
    let harness = harness(
        "external-forget",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&membership_with_networks()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::DELETE,
        "/api/environment/clusters/alpha/networks/external/vm-lan",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(
        answer["note"].as_str().unwrap().contains("left in place"),
        "{answer}"
    );

    let (_, networks) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/alpha/networks",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert!(
        networks["external"].as_array().unwrap().is_empty(),
        "{networks}"
    );

    let (status, answer) = request(
        &harness.router,
        Method::DELETE,
        "/api/environment/clusters/alpha/networks/external/vm-lan",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
}

// --- the cluster VIP ------------------------------------------------------

/// The operation the console offers against a latched failure.
///
/// Pacemaker keeps a failed operation's result until somebody clears it, so an
/// address stopped with "Not installed" stays stopped after the missing piece
/// is installed. This clears it and probes again — and answers with what
/// Pacemaker says next, not with a success flag.
#[tokio::test]
async fn recovering_the_cluster_address_clears_its_failure_and_reports_what_followed() {
    let backend = MockBackend::environment().with_stopped_vip("alpha", "Not installed");
    let harness = harness(
        "vip-recover",
        backend,
        MockPeers::new(),
        Some(&membership_with_networks()),
    );
    let cookie = sign_in(&harness.router).await;

    // Before: the console can see exactly why it is down.
    let (_, cluster) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/alpha",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(cluster["vip"]["state"]["reason"], "Not installed");
    assert_eq!(cluster["vip"]["state"]["active"], false);

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/vip/recover",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["address"], "192.168.10.100");
    assert_eq!(answer["state"]["active"], true, "{answer}");
    assert!(answer["state"]["reason"].is_null(), "{answer}");
}

/// A recovery run before the cause is fixed re-probes and fails the same way.
/// The answer says so instead of reporting the cleanup as a repair — the
/// console would otherwise show a green toast over an address nobody answers
/// on.
#[tokio::test]
async fn a_recovery_before_the_cause_is_fixed_answers_with_the_same_failure() {
    let backend = MockBackend::environment()
        .with_stopped_vip("alpha", "Not installed")
        .with_unfixed_cause();
    let harness = harness(
        "vip-recover-unfixed",
        backend,
        MockPeers::new(),
        Some(&membership_with_networks()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/vip/recover",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["state"]["active"], false, "{answer}");
    assert_eq!(answer["state"]["reason"], "Not installed", "{answer}");
}

/// Moving the address is a remove and a create: `IPaddr2` has no notion of its
/// address changing underneath it, and without the removal the old address
/// stays up on whichever member holds it.
#[tokio::test]
async fn moving_the_cluster_address_removes_the_old_resource_first() {
    let harness = harness(
        "vip-move",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&membership_with_networks()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::PUT,
        "/api/environment/clusters/alpha/vip",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "address": "192.168.10.150",
            "i_understand_this_may_disconnect_me": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["vip"]["address"], "192.168.10.150");

    let (_, networks) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/alpha/networks",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(networks["management"]["vip"], "192.168.10.150");
}

/// Without the acknowledgement, nothing happens. There is no safe version of
/// this — the old address comes down before the new one goes up — so the guard
/// is a refusal until the operator has been told.
#[tokio::test]
async fn changing_the_cluster_address_needs_the_acknowledgement() {
    let harness = harness(
        "vip-unacked",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&membership_with_networks()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::PUT,
        "/api/environment/clusters/alpha/vip",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "address": "192.168.10.150" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");

    let (_, networks) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/alpha/networks",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(networks["management"]["vip"], "192.168.10.100", "unchanged");
}

/// An address outside the Management subnet, or one a member already holds, is
/// refused before the old one comes down — the two ways a cluster VIP
/// becomes a resource that never starts.
#[tokio::test]
async fn an_unusable_cluster_address_is_refused_before_anything_moves() {
    let harness = harness(
        "vip-invalid",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&membership_with_networks()),
    );
    let cookie = sign_in(&harness.router).await;

    for (address, why) in [("10.99.0.5", "outside"), ("192.168.10.1", "already")] {
        let (status, answer) = request(
            &harness.router,
            Method::PUT,
            "/api/environment/clusters/alpha/vip",
            Some(&cookie),
            None,
            Some(serde_json::json!({
                "address": address,
                "i_understand_this_may_disconnect_me": true,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{address}: {answer}");
        assert!(
            answer.to_string().contains(why),
            "{address} should be refused as {why}: {answer}"
        );
    }

    let (_, networks) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/alpha/networks",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(networks["management"]["vip"], "192.168.10.100", "unchanged");
}

/// Clearing the address is how it is removed — the resource goes and the
/// members keep their own addresses, which is what a cluster defined without
/// one looks like.
#[tokio::test]
async fn clearing_the_cluster_address_removes_it() {
    let harness = harness(
        "vip-clear",
        MockBackend::environment(),
        MockPeers::new(),
        Some(&membership_with_networks()),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::PUT,
        "/api/environment/clusters/alpha/vip",
        Some(&cookie),
        None,
        Some(serde_json::json!({
            "address": serde_json::Value::Null,
            "i_understand_this_may_disconnect_me": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(answer["vip"].is_null(), "{answer}");

    let (_, networks) = request(
        &harness.router,
        Method::GET,
        "/api/environment/clusters/alpha/networks",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert!(networks["management"]["vip"].is_null(), "{networks}");

    // And there is nothing left to recover.
    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/vip/recover",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert!(
        answer["error"].as_str().unwrap().contains("no cluster VIP"),
        "{answer}"
    );
}

// --- clearing a disk across the environment ----------------------------------

/// The local node short-circuits to its own storage domain, the same way the
/// inventory read does — there is no socket to itself, and the answer is the
/// disk as the node now reports it.
#[tokio::test]
async fn clearing_a_disk_on_this_node_goes_through_its_own_storage_domain() {
    const TB: u64 = 1_000_000_000_000;
    let membership = membership_of(&[("alpha-1", None)]);
    let harness = harness_with_storage(
        "wipe-local",
        MockBackend::appliance(),
        MockPeers::new(),
        Some(&membership),
        Arc::new(
            lumen_zfs::backend::mock::MockBackend::appliance().with_disks(vec![
                lumen_zfs::backend::mock::MockBackend::partitioned_disk("sdb", TB, 2),
            ]),
        ),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/nodes/alpha-1/disks/sdb/wipe",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "i_understand_this_may_lose_data": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["name"], "sdb");
    assert_eq!(answer["in_use"], false, "{answer}");
    assert_eq!(answer["partitions"], 0, "{answer}");
}

/// Without the acknowledgement, nothing is touched — checked once at the
/// environment layer, before the request is routed anywhere.
#[tokio::test]
async fn clearing_a_disk_needs_the_acknowledgement() {
    let membership = membership_of(&[("alpha-1", None)]);
    let harness = harness(
        "wipe-unacked",
        MockBackend::appliance(),
        MockPeers::new(),
        Some(&membership),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/nodes/alpha-1/disks/sdb/wipe",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert!(
        answer["error"].as_str().unwrap().contains("no undo"),
        "{answer}"
    );
}

/// A node that is not in the environment is a 404, not an attempt to reach it.
#[tokio::test]
async fn clearing_a_disk_on_a_stranger_is_refused() {
    let membership = membership_of(&[("alpha-1", None)]);
    let harness = harness(
        "wipe-stranger",
        MockBackend::appliance(),
        MockPeers::new(),
        Some(&membership),
    );
    let cookie = sign_in(&harness.router).await;

    let (status, answer) = request(
        &harness.router,
        Method::POST,
        "/api/environment/nodes/somewhere-else/disks/sdb/wipe",
        Some(&cookie),
        None,
        Some(serde_json::json!({ "i_understand_this_may_lose_data": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
}
