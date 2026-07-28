//! Taking a node out of service, over the real router with every domain on
//! its in-memory backend: the flag reaches the replicated record, Pacemaker is
//! told, the running machines move off, and a machine that cannot move is
//! named rather than quietly left behind.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use lumen_cluster::backend::mock::{membership_of, MockBackend as ClusterMockBackend};
use lumen_cluster::networks::{AddressedMember, ClusterNetworks, CoreNetwork, ManagementNetwork};
use lumen_cluster::{
    BmcConfig, ClusterDefinition, ClusterRecord, ClusterService, EnvironmentMembership, MemberNode,
    MockPeers, VolumeRecord, VolumeSeat,
};
use lumen_controlplane::config::Config;
use lumen_controlplane::realm::{AuthFailure, Realm, RealmKind, RealmRegistry};
use lumen_controlplane::{app, security, AppState};
use lumen_drbd::{VmDiskRequest, VmVolumes};
use lumen_net::NetworkService;
use lumen_virt::model::{VmConfig, VmDisk};
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
            "lumen-maintenance-flow-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

/// The two-node cluster "alpha", recorded whole: Core seats for the migration
/// path, and one replicated volume on minor 1.
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
    let mut record = ClusterRecord::new(
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
    );
    record.volumes.push(VolumeRecord {
        name: "vm-101-disk-0".into(),
        size_bytes: 1 << 30,
        zvol_bytes: (1 << 30) + (1 << 20),
        port: 7788,
        minor: 1,
        replicas: vec![
            VolumeSeat {
                node: "alpha-1".into(),
                pool: "boot".into(),
            },
            VolumeSeat {
                node: "alpha-2".into(),
                pool: "boot".into(),
            },
        ],
    });
    membership.clusters.push(record);
    membership
}

fn machine(vmid: u32, name: &str, source: &str) -> VmConfig {
    VmConfig {
        vmid,
        name: name.into(),
        vcpus: 1,
        memory_mib: 1024,
        disks: vec![VmDisk {
            id: "vda".into(),
            bus: Default::default(),
            source: source.into(),
            size: 1 << 30,
            cache: Default::default(),
            discard: true,
            boot_index: None,
        }],
        ..VmConfig::default()
    }
}

struct Harness {
    router: axum::Router,
    state: Arc<AppState>,
    cluster_backend: Arc<ClusterMockBackend>,
    virt_backend: Arc<lumen_virt::backend::mock::MockBackend>,
    volumes: Arc<lumen_drbd::MockVmVolumes>,
    _state_dir: TempDir,
}

/// This node is `alpha-1` in a healthy two-node cluster whose one volume is
/// UpToDate on both members — the state a drain is allowed to move machines
/// in.
fn harness(tag: &str) -> Harness {
    build(
        tag,
        "alpha-1",
        alpha_membership(),
        ClusterMockBackend::environment(),
    )
}

/// This node is `beta-1` of three, and `beta-3` is already gone: one more
/// vote leaving stops the cluster. The regime where the power guard has
/// something to say.
fn harness_short_of_votes(tag: &str) -> Harness {
    build(
        tag,
        "beta-1",
        membership_of(&[
            ("beta-1", Some("beta")),
            ("beta-2", Some("beta")),
            ("beta-3", Some("beta")),
        ]),
        ClusterMockBackend::environment().with_partition("beta", "beta-3"),
    )
}

fn build(
    tag: &str,
    node: &str,
    membership: EnvironmentMembership,
    cluster_backend: ClusterMockBackend,
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
    let sys = Arc::new(lumen_sys::SysService::new(
        Arc::new(lumen_sys::backend::mock::MockPower::appliance()),
        Arc::new(lumen_sys::exec::MockExec::new()),
    ));
    let cluster_backend = Arc::new(cluster_backend);
    let cluster = Arc::new(
        ClusterService::new(
            cluster_backend.clone(),
            Arc::new(MockPeers::new()),
            network.clone(),
            &state_dir.0,
            "test",
        )
        .with_node(node)
        .with_form_poll(Duration::from_millis(5))
        .with_environment(&membership),
    );
    // Both replicas current: what `choose_target` insists on before it will
    // move a machine onto a peer.
    let drbd = Arc::new(lumen_drbd::DrbdService::new(
        Arc::new(
            lumen_drbd::backend::mock::MockBackend::appliance().with_healthy(
                "alpha-vm-101-disk-0",
                "alpha-2",
                1,
            ),
        ),
        Arc::new(lumen_drbd::MockVolumePeers::new()),
        cluster.clone(),
        storage.clone(),
    ));
    let volumes = Arc::new(lumen_drbd::MockVmVolumes::clustered(
        "alpha",
        &["alpha-1", "alpha-2"],
    ));
    let virt_backend = Arc::new(lumen_virt::backend::mock::MockBackend::appliance());
    let virt = Arc::new(VirtService::new(
        virt_backend.clone(),
        storage.clone(),
        network.clone(),
        volumes.clone(),
    ));
    let state = Arc::new(AppState {
        config,
        jwt_secret: security::session_secret(TICKET_SECRET.to_vec()),
        tls: None,
        realms: RealmRegistry::new().register(Box::new(MockRealm)),
        sys,
        network,
        storage,
        virt,
        cluster,
        drbd,
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
        updates: Arc::new(lumen_update::UpdateService::new(
            Arc::new(lumen_update::MockUpdates::new()),
            "test-node",
        )),
        drain: Default::default(),
        update_job: Default::default(),
    });
    Harness {
        router: app(state.clone()),
        state,
        cluster_backend,
        virt_backend,
        volumes,
        _state_dir: state_dir,
    }
}

impl Harness {
    /// A running machine on this node, with a replicated disk the mock
    /// volumes know about.
    async fn running_machine(&self, vmid: u32, name: &str) {
        self.volumes
            .create_disk(&VmDiskRequest {
                name: format!("vm-{vmid}-disk-0"),
                size_bytes: 1 << 30,
                members: Vec::new(),
            })
            .await
            .unwrap();
        let xml = lumen_virt::domain_xml::render(&machine(vmid, name, "/dev/drbd1"));
        self.state.virt.adopt(&xml).await.unwrap();
    }

    /// A running machine whose disk is a local zvol — one that cannot follow
    /// the machine anywhere.
    async fn stuck_machine(&self, vmid: u32, name: &str) {
        let xml = lumen_virt::domain_xml::render(&machine(
            vmid,
            name,
            "/dev/zvol/boot/lumen/vm-102-disk-0",
        ));
        self.state.virt.adopt(&xml).await.unwrap();
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
    response
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn request(
    router: &axum::Router,
    method: Method,
    path: &str,
    cookie: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, cookie);
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

/// Poll the drain feed until it stops running.
async fn drained(router: &axum::Router, cookie: &str) -> serde_json::Value {
    for _ in 0..400 {
        let (status, body) = request(
            router,
            Method::GET,
            "/api/environment/maintenance",
            cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        if body["phase"] != "running" {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the drain never finished");
}

const ENTER: &str = "/api/environment/clusters/alpha/nodes/alpha-1/maintenance";

#[tokio::test]
async fn entering_maintenance_moves_the_machines_off_and_says_so() {
    let harness = harness("drain");
    harness.running_machine(101, "web01").await;
    let cookie = sign_in(&harness.router).await;

    let (status, accepted) = request(&harness.router, Method::POST, ENTER, &cookie, None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["node"], "alpha-1");
    assert_eq!(accepted["cluster"], "alpha");
    // The node is out of service by the time the request answers — the
    // machines are what is still moving.
    assert_eq!(
        harness.cluster_backend.standby_calls(),
        vec![("alpha-1".to_string(), true)]
    );
    assert!(harness
        .state
        .cluster
        .maintenance_of("alpha-1")
        .unwrap()
        .is_some());

    let finished = drained(&harness.router, &cookie).await;
    assert_eq!(finished["phase"], "complete");
    assert_eq!(
        finished["stranded"].as_array().unwrap().len(),
        0,
        "nothing left behind: {finished}"
    );

    // The machine really left, over the Core network rather than Management.
    let migrated = harness.virt_backend.migrated();
    assert_eq!(migrated.len(), 1, "{migrated:?}");
    assert_eq!(migrated[0].0, "web01");
    assert_eq!(migrated[0].1, "qemu+tcp://10.10.0.2/system");

    // And the step feed names where each machine went.
    let steps = finished["steps"].as_array().unwrap();
    let web = steps.iter().find(|s| s["step"] == "web01").unwrap();
    assert_eq!(web["state"], "done");
    assert_eq!(web["node"], "alpha-2");

    // Coming back is one request, and the machine stays where it went —
    // failback is the operator's call, exactly as after an HA restart.
    let (status, view) = request(&harness.router, Method::DELETE, ENTER, &cookie, None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert!(view.get("maintenance").is_none(), "{view}");
    assert_eq!(
        harness.cluster_backend.standby_calls(),
        vec![
            ("alpha-1".to_string(), true),
            ("alpha-1".to_string(), false)
        ]
    );
    assert_eq!(harness.virt_backend.migrated().len(), 1, "no failback");
}

/// The honest half: a machine that cannot move does not fail the drain, and it
/// does not vanish either. It is named, with the reason, because "out of
/// service" and "empty" are different facts.
#[tokio::test]
async fn a_machine_that_cannot_move_is_named_not_hidden() {
    let harness = harness("stranded");
    harness.running_machine(101, "web01").await;
    harness.stuck_machine(102, "db01").await;
    let cookie = sign_in(&harness.router).await;

    let (status, _) = request(&harness.router, Method::POST, ENTER, &cookie, None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let finished = drained(&harness.router, &cookie).await;

    assert_eq!(finished["phase"], "complete");
    let stranded = finished["stranded"].as_array().unwrap();
    assert_eq!(stranded.len(), 1, "{finished}");
    assert_eq!(stranded[0]["name"], "db01");
    assert!(
        stranded[0]["reason"].as_str().unwrap().contains("replica"),
        "the reason has to be actionable: {}",
        stranded[0]["reason"]
    );
    // The one that could move still did — a drain works through the whole
    // list rather than stopping at the first refusal.
    assert_eq!(harness.virt_backend.migrated().len(), 1);
}

#[tokio::test]
async fn a_drain_that_was_not_asked_for_still_reports_what_is_running() {
    let harness = harness("no-evacuate");
    harness.running_machine(101, "web01").await;
    let cookie = sign_in(&harness.router).await;

    let (status, body) = request(
        &harness.router,
        Method::POST,
        ENTER,
        &cookie,
        Some(serde_json::json!({ "evacuate": false })),
    )
    .await;
    // Complete on the spot: there was nothing to wait for.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["phase"], "complete");
    assert_eq!(body["stranded"][0]["name"], "web01");
    assert!(harness.virt_backend.migrated().is_empty(), "nothing moved");
    assert!(harness
        .state
        .cluster
        .maintenance_of("alpha-1")
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn maintenance_is_run_from_the_node_it_is_about() {
    let harness = harness("elsewhere");
    let cookie = sign_in(&harness.router).await;

    let (status, body) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/alpha/nodes/alpha-2/maintenance",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("alpha-2"),
        "{body}"
    );

    // And the cluster in the path has to be the one the node is actually in.
    let (status, body) = request(
        &harness.router,
        Method::POST,
        "/api/environment/clusters/beta/nodes/alpha-1/maintenance",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("alpha"), "{body}");
}

/// The hole this closes: nothing used to stand between an operator and
/// shutting down the last vote a cluster had to spare.
#[tokio::test]
async fn power_refuses_to_take_the_cluster_down_with_the_node() {
    let harness = harness_short_of_votes("power");
    let cookie = sign_in(&harness.router).await;
    let restart = serde_json::json!({ "action": "reboot" });

    let (status, body) = request(
        &harness.router,
        Method::POST,
        "/api/system/power",
        &cookie,
        Some(restart.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let message = body["error"].as_str().unwrap();
    assert!(message.contains("quorum"), "{message}");
    assert!(
        message.contains("maintenance"),
        "the refusal has to name the way through it: {message}"
    );

    // An owner who means it is never locked out of their own appliance.
    let (status, _) = request(
        &harness.router,
        Method::POST,
        "/api/system/power",
        &cookie,
        Some(serde_json::json!({
            "action": "reboot",
            "i_understand_the_cluster_loses_quorum": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // And a node already in maintenance has been through the guards and been
    // told, so the power page stops arguing with it. Getting there means the
    // cluster has finished with beta-3 first — maintenance refuses to start
    // while a member is still waiting on a fence, which is its own guard
    // doing its job.
    use lumen_cluster::backend::ClusterBackend;
    harness
        .cluster_backend
        .confirm_node_dead("beta-3")
        .await
        .unwrap();
    harness
        .state
        .cluster
        .set_maintenance("beta-1", true, "root@pam")
        .await
        .unwrap();
    let (status, _) = request(
        &harness.router,
        Method::POST,
        "/api/system/power",
        &cookie,
        Some(restart),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

/// The two-node regime is the case the majority rule would get wrong: it
/// exists so one survivor carries on, and refusing a restart there would take
/// the console away exactly where the design says it should not.
#[tokio::test]
async fn power_does_not_argue_with_a_two_node_cluster() {
    let harness = harness("power-two-node");
    let cookie = sign_in(&harness.router).await;

    let (status, body) = request(
        &harness.router,
        Method::POST,
        "/api/system/power",
        &cookie,
        Some(serde_json::json!({ "action": "reboot" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}
