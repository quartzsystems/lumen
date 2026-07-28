//! End-to-end networking API tests over the real router, with the in-memory
//! backend injected in place of NetworkManager — the same shape as the mock
//! realm in tests/auth_flow.rs. Nothing here touches the runner's networking,
//! so `make test` passes on a machine with no NetworkManager at all.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use lumen_controlplane::config::Config;
use lumen_controlplane::realm::{AuthFailure, Realm, RealmKind, RealmRegistry};
use lumen_controlplane::{app, AppState};
use lumen_net::backend::mock::MockBackend;
use lumen_net::backend::NetworkBackend;
use lumen_net::NetworkService;

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

/// A router plus the backend behind it, so a test can assert on what actually
/// happened to the box as well as on what the API said.
struct Harness {
    router: axum::Router,
    backend: Arc<MockBackend>,
    cookie: String,
    _state_dir: TempDir,
}

/// The state dir has to be per-test: the pending set and the checkpoint record
/// live on disk, and tests run concurrently.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "lumen-cp-network-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn harness(tag: &str) -> Harness {
    let mut config = Config::from_env();
    config.webui_dir = std::env::temp_dir().join("lumen-webui-none");
    config.no_tls = true;
    config.session_ttl_secs = 3600;
    config.net_confirm_secs = 60;

    let state_dir = TempDir::new(tag);
    let backend = Arc::new(MockBackend::appliance());
    let network = Arc::new(NetworkService::new(backend.clone(), &state_dir.0, 60));
    // The router needs every domain; these tests only exercise networking, and
    // the other two are the in-memory backends so nothing is touched.
    let storage = Arc::new(lumen_zfs::StorageService::new(Arc::new(
        lumen_zfs::backend::mock::MockBackend::appliance(),
    )));
    let virt = Arc::new(lumen_virt::VirtService::new(
        Arc::new(lumen_virt::backend::mock::MockBackend::appliance()),
        storage.clone(),
        network.clone(),
        Arc::new(lumen_drbd::MockVmVolumes::standalone()),
    ));

    let sys = Arc::new(lumen_sys::SysService::new(
        Arc::new(lumen_sys::backend::mock::MockPower::appliance()),
        Arc::new(lumen_sys::exec::MockExec::new()),
    ));
    let cluster = Arc::new(lumen_cluster::ClusterService::new(
        Arc::new(lumen_cluster::backend::mock::MockBackend::appliance()),
        Arc::new(lumen_cluster::MockPeers::new()),
        network.clone(),
        &state_dir.0,
        "test",
    ));
    let drbd = Arc::new(lumen_drbd::DrbdService::new(
        Arc::new(lumen_drbd::backend::mock::MockBackend::appliance()),
        Arc::new(lumen_drbd::MockVolumePeers::new()),
        cluster.clone(),
        storage.clone(),
    ));
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
        drbd,
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
        updates: Arc::new(lumen_update::UpdateService::new(
            Arc::new(lumen_update::MockUpdates::new()),
            "test-node",
        )),
        drain: Default::default(),
        update_job: Default::default(),
    }));

    // Sign in once; every networking route requires the session.
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
        .expect("login must set the session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    Harness {
        router,
        backend,
        cookie,
        _state_dir: state_dir,
    }
}

impl Harness {
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().method(method).uri(path);
        request = request.header(header::COOKIE, &self.cookie);
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
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

    async fn get(&self, path: &str) -> serde_json::Value {
        let (status, json) = self.call("GET", path, None).await;
        assert_eq!(status, StatusCode::OK, "GET {path} -> {json}");
        json
    }

    async fn post(&self, path: &str, body: &str) -> (StatusCode, serde_json::Value) {
        self.call("POST", path, Some(body)).await
    }
}

// --- the happy path ---------------------------------------------------------

#[tokio::test]
async fn stage_validate_apply_confirm() {
    let h = harness("confirm").await;

    // Nothing staged to begin with.
    let pending = h.get("/api/network/pending").await;
    assert!(pending["target"].is_null());
    assert!(pending["checkpoint"].is_null());

    // The observed table is grouped by node even with one node.
    let interfaces = h.get("/api/network/interfaces").await;
    let nodes = interfaces["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    let rows = nodes[0]["interfaces"].as_array().unwrap();
    let nic0 = rows.iter().find(|r| r["name"] == "nic0").unwrap();
    assert_eq!(nic0["management"], true);
    assert_eq!(nic0["deletable"], false);
    assert_eq!(nic0["addresses"][0], "192.168.10.5/24");
    // Every column the console renders comes out of this one response.
    assert_eq!(nic0["altname"], "enp1s0");
    assert_eq!(nic0["ip"]["mode"], "static");
    assert_eq!(nic0["ip"]["cidr"], "192.168.10.5/24");
    assert_eq!(nic0["vlan_aware"], false);
    assert!(nic0["comment"].is_null());
    assert!(
        !rows.iter().any(|r| r["name"] == "lo"),
        "loopback is not a row anyone can act on"
    );

    // Stage a bridge over the spare NIC.
    let (status, staged) = h
        .post(
            "/api/network/bridges",
            r#"{"name":"br1","ports":["nic1"],"stp":false}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{staged}");
    assert_eq!(staged["errors"].as_array().unwrap().len(), 0);
    let changes = staged["changes"].as_array().unwrap();
    assert!(changes
        .iter()
        .any(|c| c["link"] == "br1" && c["change"] == "created"));
    assert!(changes
        .iter()
        .any(|c| c["link"] == "nic1" && c["change"] == "modified"));

    // It shows in the table before it is applied, badged.
    let rows = h.get("/api/network/interfaces").await["nodes"][0]["interfaces"]
        .as_array()
        .unwrap()
        .clone();
    let br1 = rows.iter().find(|r| r["name"] == "br1").unwrap();
    assert_eq!(br1["change"], "created");
    assert_eq!(br1["present"], false);

    // Apply: a checkpoint and an absolute deadline come back.
    let (status, applied) = h.post("/api/network/apply", "{}").await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert_eq!(applied["checkpoint"]["rollback_secs"], 60);
    assert!(applied["checkpoint"]["confirm_deadline"].as_u64().unwrap() > 0);
    assert!(!applied["operations"].as_array().unwrap().is_empty());
    assert!(h.backend.state().link("br1").is_some());

    // Confirm: permanent, nothing staged, no checkpoint.
    let (status, confirmed) = h.post("/api/network/confirm", "{}").await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
    assert!(confirmed["target"].is_null());
    assert!(confirmed["checkpoint"].is_null());
    assert!(h.backend.checkpoints().await.unwrap().is_empty());

    let config = h.get("/api/network/config").await;
    assert!(config["bridges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["name"] == "br1"));
}

// --- the path the whole design exists for -----------------------------------

#[tokio::test]
async fn stage_apply_then_expire_without_confirming() {
    let h = harness("expire").await;
    let before = h.backend.state();

    h.post("/api/network/bridges", r#"{"name":"br1","ports":["nic1"]}"#)
        .await;
    let (status, _) = h.post("/api/network/apply", "{}").await;
    assert_eq!(status, StatusCode::OK);
    assert!(h.backend.state().link("br1").is_some());

    // NetworkManager's own timer fires, out of process. Nobody told the
    // control plane; it has to notice.
    assert_eq!(h.backend.expire_checkpoints(), 1);

    let pending = h.get("/api/network/pending").await;
    assert!(
        pending["checkpoint"].is_null(),
        "the API must report the checkpoint as gone: {pending}"
    );
    assert!(
        !pending["target"].is_null(),
        "the staged change stays staged so it can be fixed and retried"
    );
    assert_eq!(h.backend.state(), before, "the box reverted itself");

    // The change was never committed…
    let config = h.get("/api/network/config").await;
    assert!(!config["bridges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["name"] == "br1"));
    // …and confirming after the fact is a conflict, not a silent success.
    let (status, _) = h.post("/api/network/confirm", "{}").await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn rollback_reverts_immediately() {
    let h = harness("rollback").await;
    let before = h.backend.state();

    h.post("/api/network/bridges", r#"{"name":"br1","ports":["nic1"]}"#)
        .await;
    h.post("/api/network/apply", "{}").await;
    let (status, rolled) = h.post("/api/network/rollback", "{}").await;
    assert_eq!(status, StatusCode::OK, "{rolled}");
    assert!(rolled["checkpoint"].is_null());
    assert_eq!(h.backend.state(), before);
}

#[tokio::test]
async fn the_confirm_window_can_be_extended() {
    let h = harness("extend").await;
    h.post("/api/network/bridges", r#"{"name":"br1","ports":["nic1"]}"#)
        .await;
    let (_, applied) = h.post("/api/network/apply", "{}").await;
    let deadline = applied["checkpoint"]["confirm_deadline"].as_u64().unwrap();

    let (status, extended) = h
        .post("/api/network/apply/extend", r#"{"seconds":120}"#)
        .await;
    assert_eq!(status, StatusCode::OK, "{extended}");
    assert_eq!(
        extended["confirm_deadline"].as_u64().unwrap(),
        deadline + 120
    );
    assert_eq!(extended["rollback_secs"], 180);
}

#[tokio::test]
async fn a_second_apply_is_refused_while_one_is_outstanding() {
    let h = harness("second-apply").await;
    h.post("/api/network/bridges", r#"{"name":"br1","ports":["nic1"]}"#)
        .await;
    h.post("/api/network/apply", "{}").await;

    h.post("/api/network/bridges", r#"{"name":"br2","ports":[]}"#)
        .await;
    let (status, body) = h.post("/api/network/apply", "{}").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("confirm"),
        "{body}"
    );
}

// --- validation -------------------------------------------------------------

#[tokio::test]
async fn validation_failures_carry_codes_and_fields() {
    let h = harness("validation").await;
    let (status, body) = h
        .post(
            "/api/network/vlans",
            r#"{"name":"vlan9999","parent":"nic1","vlan_id":9999}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // The standard envelope is still there…
    assert!(body["error"].as_str().is_some_and(|s| !s.is_empty()));
    // …plus the machine-readable detail the console pins to a field.
    let errors = body["errors"].as_array().unwrap();
    assert_eq!(errors[0]["code"], "vlan_id_out_of_range");
    assert_eq!(errors[0]["field"], "vlan_id");
    assert_eq!(errors[0]["link"], "vlan9999");

    // Nothing was staged by the failed request.
    assert!(h.get("/api/network/pending").await["target"].is_null());
}

#[tokio::test]
async fn a_port_cannot_belong_to_two_controllers() {
    let h = harness("two-controllers").await;
    h.post("/api/network/bridges", r#"{"name":"br1","ports":["nic1"]}"#)
        .await;
    let (status, body) = h
        .post(
            "/api/network/bonds",
            r#"{"name":"bond0","mode":"active-backup","ports":["nic1"]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["errors"][0]["code"], "multiple_controllers");
}

#[tokio::test]
async fn a_typo_in_a_request_is_rejected_not_ignored() {
    let h = harness("typo").await;
    let (status, _) = h
        .post("/api/network/bridges", r#"{"name":"br1","stpp":true}"#)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn enslaving_the_management_nic_by_hand_is_refused() {
    // The trap: build a bridge over the management NIC and forget to move the
    // address. NetworkManager would accept it and the box would go dark.
    // /api/network/management-bridge is the route that does this correctly.
    let h = harness("hand-built-bridge").await;
    let (status, body) = h
        .post(
            "/api/network/bridges",
            r#"{"name":"br0","ports":["nic0"],"stp":false}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["errors"][0]["code"], "management_is_a_port");
    assert!(body["errors"][0]["message"]
        .as_str()
        .unwrap()
        .contains("move the address"));
}

// --- the management bridge --------------------------------------------------

#[tokio::test]
async fn management_bridge_conversion_preserves_the_address_and_pins_the_mac() {
    let h = harness("mgmt-bridge").await;

    let (status, body) = h.post("/api/network/management-bridge", "{}").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["converted"], true);
    assert_eq!(body["bridge"], "br0");
    assert!(!body["checkpoint"].is_null(), "it runs inside a checkpoint");

    let state = h.backend.state();
    let br0 = state.link("br0").expect("the bridge exists");
    assert_eq!(br0.addresses, vec!["192.168.10.5/24".to_string()]);
    assert_eq!(
        br0.mac.as_deref(),
        Some("52:54:00:aa:bb:00"),
        "the bridge MAC is pinned to the NIC's permanent address"
    );
    assert_eq!(br0.ports, vec!["nic0".to_string()]);
    assert!(
        state.link("nic0").unwrap().addresses.is_empty(),
        "the address moved rather than being duplicated"
    );

    h.post("/api/network/confirm", "{}").await;

    // Idempotent: doing it again is success with nothing to do.
    let (status, again) = h.post("/api/network/management-bridge", "{}").await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["converted"], false);
    assert!(again["checkpoint"].is_null());
}

// --- editing ----------------------------------------------------------------

#[tokio::test]
async fn a_nic_can_be_patched_but_not_deleted() {
    let h = harness("patch-nic").await;
    let (status, staged) = h
        .call("PATCH", "/api/network/nics/nic1", Some(r#"{"mtu":9000}"#))
        .await;
    assert_eq!(status, StatusCode::OK, "{staged}");
    assert!(staged["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["link"] == "nic1" && c["change"] == "modified"));

    let (status, body) = h.call("DELETE", "/api/network/bridges/nic1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// The bridge dialog's fields, end to end: address, gateway, VLAN awareness,
/// ports, and a comment, staged in one request and read back off the table.
#[tokio::test]
async fn a_bridge_can_be_created_with_an_address_and_a_comment() {
    let h = harness("bridge-fields").await;
    let (status, staged) = h
        .post(
            "/api/network/bridges",
            r#"{"name":"br1","ports":["nic1"],"vlan_filtering":true,
                "comment":"guest traffic",
                "ip":{"mode":"static","cidr":"10.0.0.5/24","gateway":"10.0.0.1"}}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{staged}");
    assert_eq!(staged["errors"].as_array().unwrap().len(), 0);

    let rows = h.get("/api/network/interfaces").await["nodes"][0]["interfaces"]
        .as_array()
        .unwrap()
        .clone();
    let br1 = rows.iter().find(|r| r["name"] == "br1").unwrap();
    assert_eq!(br1["vlan_aware"], true);
    assert_eq!(br1["comment"], "guest traffic");
    assert_eq!(br1["ip"]["cidr"], "10.0.0.5/24");
    assert_eq!(br1["ip"]["gateway"], "10.0.0.1");

    // And it applies without disturbing the management address on nic0.
    let (status, applied) = h.post("/api/network/apply", "{}").await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    let state = h.backend.state();
    assert_eq!(
        state.link("br1").unwrap().addresses,
        vec!["10.0.0.5/24".to_string()]
    );
    assert_eq!(
        state.link("nic0").unwrap().addresses,
        vec!["192.168.10.5/24".to_string()]
    );
}

/// Leaving an address on a link while enslaving it is silently dropped by the
/// kernel, so it is refused here with the field it belongs to.
#[tokio::test]
async fn an_address_left_on_a_port_is_refused() {
    let h = harness("port-address").await;
    h.call(
        "PATCH",
        "/api/network/nics/nic1",
        Some(r#"{"ip":{"mode":"dhcp"}}"#),
    )
    .await;
    let (status, body) = h
        .post("/api/network/bridges", r#"{"name":"br1","ports":["nic1"]}"#)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["errors"][0]["code"], "port_has_address");
    assert_eq!(body["errors"][0]["field"], "ip");
    assert_eq!(body["errors"][0]["link"], "nic1");
}

#[tokio::test]
async fn a_staged_delete_stays_visible_until_it_is_applied() {
    let h = harness("staged-delete").await;
    h.post("/api/network/bridges", r#"{"name":"br1","ports":["nic1"]}"#)
        .await;
    h.post("/api/network/apply", "{}").await;
    h.post("/api/network/confirm", "{}").await;

    let (status, staged) = h.call("DELETE", "/api/network/bridges/br1", None).await;
    assert_eq!(status, StatusCode::OK, "{staged}");
    let rows = h.get("/api/network/interfaces").await["nodes"][0]["interfaces"]
        .as_array()
        .unwrap()
        .clone();
    let br1 = rows
        .iter()
        .find(|r| r["name"] == "br1")
        .expect("still shown");
    assert_eq!(br1["change"], "deleted");
}

#[tokio::test]
async fn discarding_clears_the_staged_set() {
    let h = harness("discard").await;
    h.post("/api/network/bridges", r#"{"name":"br1","ports":["nic1"]}"#)
        .await;
    let (status, after) = h.call("DELETE", "/api/network/pending", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(after["target"].is_null());
    assert_eq!(after["changes"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn one_interface_can_be_fetched_on_its_own() {
    let h = harness("one-interface").await;
    let nic0 = h.get("/api/network/interfaces/nic0").await;
    assert_eq!(nic0["name"], "nic0");
    assert_eq!(nic0["kind"], "ethernet");
    assert_eq!(nic0["perm_mac"], "52:54:00:aa:bb:00");
    assert_eq!(nic0["speed_mbps"], 1000);
    assert_eq!(nic0["altname"], "enp1s0");

    let (status, _) = h.call("GET", "/api/network/interfaces/nic9", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- clustering placeholders ------------------------------------------------

#[tokio::test]
async fn a_request_for_another_node_is_refused_clearly() {
    let h = harness("other-node").await;
    let (status, body) = h
        .post(
            "/api/network/bridges",
            r#"{"node":"lumen02","name":"br1","ports":["nic1"]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("not in a cluster"),
        "{body}"
    );
}

#[tokio::test]
async fn the_node_check_also_covers_routes_with_no_other_payload() {
    // Quietly applying a request meant for another node to this one is the
    // wrong failure, so the check has to hold even where `node` is the only
    // field the body can carry.
    let h = harness("other-node-bodyless").await;
    h.post("/api/network/bridges", r#"{"name":"br1","ports":["nic1"]}"#)
        .await;
    for path in [
        "/api/network/confirm",
        "/api/network/rollback",
        "/api/network/management-bridge",
    ] {
        let (status, body) = h.post(path, r#"{"node":"lumen02"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path} -> {body}");
    }
    let (status, body) = h
        .call(
            "DELETE",
            "/api/network/pending",
            Some(r#"{"node":"lumen02"}"#),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // …and the staged set is untouched by any of it.
    assert!(!h.get("/api/network/pending").await["target"].is_null());
}

// --- authentication ---------------------------------------------------------

#[tokio::test]
async fn every_networking_route_requires_a_session() {
    let h = harness("auth").await;
    for (method, path) in [
        ("GET", "/api/network/interfaces"),
        ("GET", "/api/network/config"),
        ("GET", "/api/network/pending"),
        ("POST", "/api/network/bridges"),
        ("POST", "/api/network/apply"),
        ("POST", "/api/network/confirm"),
        ("POST", "/api/network/rollback"),
        ("POST", "/api/network/management-bridge"),
        ("DELETE", "/api/network/pending"),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = h.router.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path} must require a session"
        );
    }
}
