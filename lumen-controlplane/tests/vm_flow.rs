//! End-to-end virtual-machine and storage API tests over the real router,
//! with the in-memory backends injected in place of the hypervisor and the
//! storage tools — the same shape as the mock realm in `tests/auth_flow.rs`
//! and the mock network backend in `tests/network_flow.rs`.
//!
//! Nothing here touches the runner's hypervisor or its pools, so `make test`
//! passes on a machine with neither installed.

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
use lumen_virt::backend::mock::MockBackend as MockVirtBackend;
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
    async fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthFailure> {
        if username == "root" && password == "correct-horse" {
            Ok(())
        } else {
            Err(AuthFailure::Denied)
        }
    }
}

/// A router plus the backends behind it, so a test can assert on what actually
/// happened to the node as well as on what the API said.
struct Harness {
    router: axum::Router,
    virt: Arc<MockVirtBackend>,
    zfs: Arc<MockZfsBackend>,
    cookie: String,
    _state_dir: TempDir,
}

/// The state dir has to be per-test: networking's committed document lives on
/// disk, and tests run concurrently.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "lumen-cp-vm-{tag}-{}-{:?}",
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
    let virt_backend = Arc::new(MockVirtBackend::appliance());
    let zfs_backend = Arc::new(MockZfsBackend::appliance());

    let network = Arc::new(NetworkService::new(
        Arc::new(lumen_net::backend::mock::MockBackend::appliance()),
        &state_dir.0,
        60,
    ));
    // A machine attaches to a bridge, so the node has to have one. This is the
    // same conversion a real first boot performs.
    network.management_bridge().await.unwrap();
    network.confirm().await.unwrap();

    // The media library and the guest database both live under this test's own
    // temporary root, so nothing here reads or writes what the machine running
    // the tests actually has.
    let storage = Arc::new(
        StorageService::new(zfs_backend.clone())
            .with_iso_root(state_dir.0.join("iso"))
            // The pretend appliance is installed on boot, so it is the one
            // pool the console will not destroy. Stated rather than read off
            // the machine running the tests, which has no pools at all.
            .with_root_pool(Some("boot".into())),
    );
    let virt = Arc::new(
        VirtService::new(virt_backend.clone(), storage.clone(), network.clone())
            .with_osinfo_root(state_dir.0.join("osinfo")),
    );

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
    }));

    // Sign in once; every machine and storage route requires the session.
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
        virt: virt_backend,
        zfs: zfs_backend,
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

    async fn put_bytes(&self, path: &str, bytes: &'static [u8]) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method("PUT")
            .uri(path)
            .header(header::COOKIE, &self.cookie)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(bytes))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn delete(&self, path: &str) -> (StatusCode, serde_json::Value) {
        self.call("DELETE", path, None).await
    }

    /// A well-formed WebSocket handshake, so the console route's own checks
    /// are what answers rather than the upgrade extractor rejecting a request
    /// that was never an upgrade in the first place.
    async fn handshake(&self, path: &str) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method("GET")
            .uri(path)
            .header(header::COOKIE, &self.cookie)
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            // Any 16 bytes, base64. Nothing checks its contents.
            .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Body::empty())
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    /// Define the machine every test below starts from: one disk, one adapter.
    async fn create_web01(&self) -> serde_json::Value {
        let (status, vm) = self
            .post(
                "/api/vms",
                r#"{"name":"web01","vcpus":2,"memory_mib":4096,
                    "disks":[{"pool":"boot","size_gib":32}],
                    "nics":[{"bridge":"br0"}]}"#,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{vm}");
        vm
    }
}

// --- the path the whole stage exists for -------------------------------------

#[tokio::test]
async fn create_start_shutdown_delete() {
    let h = harness("lifecycle").await;

    // Nothing to begin with, and the shape is grouped by node from day one.
    let listing = h.get("/api/vms").await;
    let nodes = listing["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    assert!(nodes[0]["vms"].as_array().unwrap().is_empty());

    let vm = h.create_web01().await;
    assert_eq!(vm["vmid"], 100);
    assert_eq!(vm["name"], "web01");
    assert_eq!(vm["state"], "shut_off");
    assert_eq!(vm["disks"][0]["id"], "vda");
    assert_eq!(
        vm["disks"][0]["source"],
        "/dev/zvol/boot/lumen/vm-100-disk-0"
    );
    assert_eq!(vm["nics"][0]["bridge"], "br0");
    assert_eq!(vm["nics"][0]["id"], "52:54:00:00:64:00");
    assert_eq!(vm["firmware"], "uefi");
    assert_eq!(vm["machine"], "q35");
    // The machine has a screen, and no address for it yet: the stored
    // document defers the socket's path to the hypervisor, which chooses one
    // at start. The console endpoint is where the address lives.
    assert_eq!(vm["has_screen"], true);
    assert_eq!(vm["vnc_socket"], serde_json::Value::Null);
    // The controls carry their own reasons, so the console never has to guess.
    assert_eq!(vm["actions"]["start"]["allowed"], true);
    assert_eq!(vm["actions"]["shutdown"]["allowed"], false);
    assert!(vm["actions"]["shutdown"]["reason"].as_str().is_some());

    // The volume really exists.
    assert!(h.zfs.has_dataset("boot/lumen/vm-100-disk-0"));

    let (status, started) = h.post("/api/vms/100/start", "{}").await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(started["state"], "running");
    assert_eq!(started["current_memory_mib"], 4096);
    assert_eq!(started["current_vcpus"], 2);
    assert!(started["uptime_secs"].as_u64().is_some());
    assert_eq!(started["actions"]["stop"]["requires_acknowledgement"], true);

    // Starting it again is a conflict, not a second start.
    let (status, _) = h.post("/api/vms/100/start", "{}").await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, stopped) = h.post("/api/vms/100/shutdown", "{}").await;
    assert_eq!(status, StatusCode::OK, "{stopped}");
    assert_eq!(stopped["state"], "shut_off");
    assert!(stopped["uptime_secs"].is_null());

    let (status, removed) = h.call("DELETE", "/api/vms/100", Some("{}")).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert!(!h.virt.is_defined("web01"));
    // The default keeps the data, and says where it still is.
    assert_eq!(removed["removed_volumes"].as_array().unwrap().len(), 0);
    assert_eq!(removed["kept_volumes"][0], "boot/lumen/vm-100-disk-0");
    assert!(h.zfs.has_dataset("boot/lumen/vm-100-disk-0"));
}

// --- the task log ------------------------------------------------------------

#[tokio::test]
async fn every_action_lands_in_the_machines_task_log() {
    let h = harness("tasks").await;
    h.create_web01().await;
    h.post("/api/vms/100/start", "{}").await;
    // A refusal is history too: a stop without the acknowledgement.
    let (status, _) = h.post("/api/vms/100/stop", "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    h.post("/api/vms/100/shutdown", "{}").await;
    let (status, body) = h
        .call(
            "PATCH",
            "/api/vms/100",
            Some(r#"{"vcpus":4,"memory_mib":8192}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let response = h.get("/api/vms/100/tasks").await;
    let tasks = response["tasks"].as_array().unwrap();
    // Newest first: update, shutdown, the refused stop, start, create.
    assert_eq!(tasks.len(), 5, "{response}");
    assert_eq!(tasks[0]["action"], "update");
    assert_eq!(tasks[0]["detail"], "Change processors, memory");
    assert_eq!(tasks[0]["status"], "ok");
    assert_eq!(tasks[1]["action"], "shutdown");
    assert_eq!(tasks[2]["action"], "stop");
    assert_eq!(tasks[2]["status"], "error");
    assert!(tasks[2]["error"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(tasks[3]["action"], "start");
    assert_eq!(tasks[4]["action"], "create");
    // Recorded against the principal who asked, not just the account.
    assert_eq!(tasks[4]["user"], "root@lumen");

    // Another machine's history is its own — and an identifier nothing has
    // touched answers with an empty list, not a 404.
    let empty = h.get("/api/vms/999/tasks").await;
    assert!(empty["tasks"].as_array().unwrap().is_empty());
}

// --- the acknowledgement paths ----------------------------------------------

#[tokio::test]
async fn a_forced_stop_is_refused_without_the_acknowledgement() {
    let h = harness("stop-ack").await;
    h.create_web01().await;
    h.post("/api/vms/100/start", "{}").await;

    let (status, body) = h.post("/api/vms/100/stop", "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["errors"][0]["code"],
        "unacknowledged_destructive_operation"
    );
    assert_eq!(
        body["errors"][0]["field"],
        "i_understand_this_may_lose_data"
    );
    // The standard envelope is still there for anything that only knows it.
    assert!(body["error"].as_str().is_some_and(|s| !s.is_empty()));
    // And the machine is untouched.
    assert_eq!(h.get("/api/vms/100").await["state"], "running");

    let (status, stopped) = h
        .post(
            "/api/vms/100/stop",
            r#"{"i_understand_this_may_lose_data":true}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{stopped}");
    assert_eq!(stopped["state"], "shut_off");
}

#[tokio::test]
async fn destroying_the_disks_is_refused_without_the_acknowledgement() {
    let h = harness("purge-ack").await;
    h.create_web01().await;

    let (status, body) = h
        .call("DELETE", "/api/vms/100", Some(r#"{"purge_disks":true}"#))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["errors"][0]["code"],
        "unacknowledged_destructive_operation"
    );
    // Nothing happened on the way to being refused.
    assert!(h.virt.is_defined("web01"));
    assert!(h.zfs.has_dataset("boot/lumen/vm-100-disk-0"));

    let (status, removed) = h
        .call(
            "DELETE",
            "/api/vms/100",
            Some(r#"{"purge_disks":true,"i_understand_this_may_lose_data":true}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["removed_volumes"][0], "boot/lumen/vm-100-disk-0");
    assert!(!h.zfs.has_dataset("boot/lumen/vm-100-disk-0"));
}

#[tokio::test]
async fn removing_a_running_machine_needs_the_acknowledgement_too() {
    let h = harness("delete-running").await;
    h.create_web01().await;
    h.post("/api/vms/100/start", "{}").await;

    let (status, body) = h.call("DELETE", "/api/vms/100", Some("{}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(h.virt.is_defined("web01"));

    let (status, _) = h
        .call(
            "DELETE",
            "/api/vms/100",
            Some(r#"{"i_understand_this_may_lose_data":true}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!h.virt.is_defined("web01"));
}

// --- validation --------------------------------------------------------------

#[tokio::test]
async fn validation_failures_carry_codes_and_fields() {
    let h = harness("validation").await;
    let (status, body) = h
        .post(
            "/api/vms",
            r#"{"name":"web01","vcpus":0,"memory_mib":4096,"nics":[{"bridge":"br9"}]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let codes: Vec<&str> = body["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"invalid_vcpus"), "{codes:?}");
    assert!(codes.contains(&"unknown_bridge"), "{codes:?}");

    // Nothing was defined by the failed request.
    assert!(h.virt.names().is_empty());
    assert!(h.get("/api/vms").await["nodes"][0]["vms"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_disk_larger_than_the_pool_is_refused_before_a_volume_is_made() {
    let h = harness("disk-too-big").await;
    let (status, body) = h
        .post(
            "/api/vms",
            r#"{"name":"big","disks":[{"pool":"boot","size_gib":8192}]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["errors"][0]["code"], "disk_exceeds_pool");
    assert!(h.zfs.datasets_snapshot().iter().all(|d| d.name == "boot"));
}

#[tokio::test]
async fn a_typo_in_a_request_is_rejected_not_ignored() {
    let h = harness("typo").await;
    let (status, _) = h
        .post("/api/vms", r#"{"name":"web01","memory_mibb":4096}"#)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn two_machines_cannot_share_a_name() {
    let h = harness("duplicate").await;
    h.create_web01().await;
    let (status, body) = h
        .post("/api/vms", r#"{"name":"web01","nics":[{"bridge":"br0"}]}"#)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["errors"][0]["code"], "duplicate_name");
}

// --- hardware ----------------------------------------------------------------

#[tokio::test]
async fn a_disk_can_be_attached_and_detached_through_the_api() {
    let h = harness("disks").await;
    h.create_web01().await;

    let (status, attached) = h
        .post(
            "/api/vms/100/disks",
            r#"{"pool":"boot","size_gib":16,"bus":"virtio-scsi"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{attached}");
    assert_eq!(attached["vm"]["disks"].as_array().unwrap().len(), 2);
    assert_eq!(attached["vm"]["disks"][1]["id"], "sda");
    assert!(h.zfs.has_dataset("boot/lumen/vm-100-disk-1"));
    // The machine is stopped, so nothing is waiting on a restart.
    assert!(attached["pending_reboot"].as_array().unwrap().is_empty());

    let (status, detached) = h
        .call(
            "DELETE",
            "/api/vms/100/disks/sda",
            Some(r#"{"purge_disks":true,"i_understand_this_may_lose_data":true}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{detached}");
    assert_eq!(detached["vm"]["disks"].as_array().unwrap().len(), 1);
    assert!(!h.zfs.has_dataset("boot/lumen/vm-100-disk-1"));
}

#[tokio::test]
async fn an_adapter_can_be_attached_and_detached_through_the_api() {
    let h = harness("nics").await;
    h.create_web01().await;

    let (status, attached) = h
        .post("/api/vms/100/nics", r#"{"bridge":"br0","vlan_tag":100}"#)
        .await;
    assert_eq!(status, StatusCode::OK, "{attached}");
    let nics = attached["vm"]["nics"].as_array().unwrap();
    assert_eq!(nics.len(), 2);
    assert_eq!(nics[1]["vlan_tag"], 100);
    let mac = nics[1]["id"].as_str().unwrap().to_string();

    let (status, detached) = h
        .call("DELETE", &format!("/api/vms/100/nics/{mac}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{detached}");
    assert_eq!(detached["vm"]["nics"].as_array().unwrap().len(), 1);
}

/// The distinction the console exists to show, end to end.
#[tokio::test]
async fn a_change_the_running_machine_cannot_take_is_reported_as_waiting() {
    let h = harness("pending").await;
    h.create_web01().await;
    h.post("/api/vms/100/start", "{}").await;

    let (status, grew) = h
        .call("PATCH", "/api/vms/100", Some(r#"{"memory_mib":6144}"#))
        .await;
    assert_eq!(status, StatusCode::OK, "{grew}");
    assert_eq!(grew["applied_live"].as_array().unwrap().len(), 1);
    assert!(grew["pending_reboot"].as_array().unwrap().is_empty());

    h.virt.refuse_live_changes("cannot exceed the boot maximum");
    let (status, waited) = h
        .call(
            "PATCH",
            "/api/vms/100",
            Some(r#"{"memory_mib":8192,"firmware":"bios"}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{waited}");
    let pending: Vec<&str> = waited["pending_reboot"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(pending.iter().any(|p| p.contains("memory")), "{pending:?}");
    assert!(
        pending.iter().any(|p| p.contains("firmware")),
        "{pending:?}"
    );
    // Stored either way; the running machine has not caught up.
    assert_eq!(waited["vm"]["memory_mib"], 8192);
    assert_eq!(waited["vm"]["current_memory_mib"], 6144);
}

#[tokio::test]
async fn options_round_trip_through_the_hypervisor() {
    let h = harness("options").await;
    h.create_web01().await;
    let (status, updated) = h
        .call(
            "PATCH",
            "/api/vms/100",
            Some(
                r#"{"description":"Public web server","tags":["Production","web"],
                    "start_on_boot":true}"#,
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["vm"]["description"], "Public web server");
    assert_eq!(updated["vm"]["tags"][0], "production");
    assert_eq!(updated["vm"]["start_on_boot"], true);

    // Read back through a fresh request: it really is in the hypervisor.
    let vm = h.get("/api/vms/100").await;
    assert_eq!(vm["description"], "Public web server");
    assert_eq!(vm["start_on_boot"], true);
}

#[tokio::test]
async fn a_machine_that_is_not_there_is_a_not_found() {
    let h = harness("missing").await;
    let (status, _) = h.call("GET", "/api/vms/999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h.post("/api/vms/999/start", "{}").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- storage -----------------------------------------------------------------

#[tokio::test]
async fn pools_are_listed_read_only_and_grouped_by_node() {
    let h = harness("pools").await;
    let response = h.get("/api/storage/pools").await;
    let nodes = response["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1);
    let boot = &nodes[0]["pools"][0];
    assert_eq!(boot["name"], "boot");
    assert_eq!(boot["health"], "online");
    assert!(boot["size"].as_u64().unwrap() > 0);
    assert!(boot["used_percent"].as_u64().is_some());
    // This is the pool the appliance is installed on, so it is visibly not
    // destroyable and the reason is said out loud.
    assert_eq!(boot["destroyable"], false);
    assert!(boot["destroy_blocked_reason"]
        .as_str()
        .unwrap()
        .contains("installed on"));
}

#[tokio::test]
async fn a_machines_disks_show_up_under_its_pool() {
    let h = harness("volumes").await;
    h.create_web01().await;

    let response = h.get("/api/storage/pools/boot/volumes").await;
    assert_eq!(response["pool"], "boot");
    let volumes = response["volumes"].as_array().unwrap();
    let disk = volumes
        .iter()
        .find(|v| v["name"] == "boot/lumen/vm-100-disk-0")
        .expect("the machine's disk is listed");
    assert_eq!(disk["kind"], "volume");
    assert_eq!(disk["lumen_managed"], true);
    // The pool's own root is not a row anyone came here to look at.
    assert!(!volumes.iter().any(|v| v["name"] == "boot"));

    let (status, _) = h.call("GET", "/api/storage/pools/tank/volumes", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A volume is created *for a machine* and never on its own, so there is no
/// route that makes one directly — and there must not be one by accident. The
/// pool routes are exercised in tests/system_flow.rs.
#[tokio::test]
async fn a_volume_is_only_ever_reached_through_the_machine_it_belongs_to() {
    let h = harness("no-volume-writes").await;
    for (method, path) in [
        ("POST", "/api/storage/pools/boot/volumes"),
        ("DELETE", "/api/storage/pools/boot/volumes/vm-100-disk-0"),
    ] {
        let (status, _) = h.call(method, path, Some("{}")).await;
        assert_ne!(status, StatusCode::OK, "{method} {path} must not exist");
    }

    // And the pool the appliance is installed on is refused even with the
    // acknowledgement — see tests/system_flow.rs for the rest of the story.
    let (status, body) = h
        .call(
            "DELETE",
            "/api/storage/pools/boot",
            Some(r#"{"i_understand_this_may_lose_data":true}"#),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

// --- clustering placeholders -------------------------------------------------

#[tokio::test]
async fn a_request_for_another_node_is_refused_clearly() {
    let h = harness("other-node").await;
    let (status, body) = h
        .post(
            "/api/vms",
            r#"{"node":"lumen02","name":"web01","nics":[{"bridge":"br0"}]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("not in a cluster"),
        "{body}"
    );

    // …and on the routes whose body carries nothing else.
    h.create_web01().await;
    let (status, _) = h.post("/api/vms/100/start", r#"{"node":"lumen02"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// --- the console viewer -------------------------------------------------------

/// A machine that is not running has no screen, and the console is told so in
/// words rather than by a viewer that opens and immediately goes grey.
#[tokio::test]
async fn the_console_is_offered_only_while_the_machine_is_running() {
    let h = harness("console").await;
    let vm = h.create_web01().await;

    // The row already carries the answer, so a disabled control explains
    // itself without asking anything.
    assert_eq!(vm["actions"]["console"]["allowed"], false);
    let reason = vm["actions"]["console"]["reason"].as_str().unwrap();
    assert!(reason.contains("no console"), "{reason}");

    let (status, body) = h.call("GET", "/api/vms/100/console", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], reason);

    // Started, it says where to connect — and the path is the same origin the
    // console is already on, so the session cookie is the only credential.
    let (status, _) = h.post("/api/vms/100/start", "{}").await;
    assert_eq!(status, StatusCode::OK);

    let console = h.get("/api/vms/100/console").await;
    assert_eq!(console["vmid"], 100);
    assert_eq!(console["name"], "web01");
    assert_eq!(console["protocol"], "vnc");
    // The hypervisor's own choice, made at start under its per-domain
    // directory and read back out of the live document.
    assert_eq!(
        console["socket"],
        "/var/lib/libvirt/qemu/domain-1-web01/vnc.sock"
    );
    assert_eq!(console["websocket"], "/api/vms/100/console/ws");

    // The running row still defers the address — the stored document never
    // carries it — so the console endpoint is the one answer, not one of two.
    let running = h.get("/api/vms/100").await;
    assert_eq!(running["actions"]["console"]["allowed"], true);
    assert_eq!(running["vnc_socket"], serde_json::Value::Null);
}

#[tokio::test]
async fn the_console_of_a_machine_that_is_not_there_is_a_not_found() {
    let h = harness("console-missing").await;
    let (status, body) = h.call("GET", "/api/vms/404/console", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// A stopped machine is refused while the request is still HTTP, so the reason
/// is a status and a sentence rather than a close code nobody can act on.
///
/// This is the whole argument for checking everything before calling
/// `on_upgrade`: once the handshake completes there is no way left to say why.
#[tokio::test]
async fn the_console_stream_refuses_a_stopped_machine_before_upgrading() {
    let h = harness("console-stream").await;
    h.create_web01().await;

    let (status, body) = h.handshake("/api/vms/100/console/ws").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("no console"),
        "{body}"
    );

    // Started, the same request gets past the machine check and stops at the
    // next one — the transport. That is the order the handler documents: the
    // question about the machine is answered before the question about the
    // connection, so a switched-off machine is never told it sent the wrong
    // kind of request.
    let (status, _) = h.post("/api/vms/100/start", "{}").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = h.handshake("/api/vms/100/console/ws").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(
        error.contains("WebSocket") && error.contains("web01"),
        "{error}"
    );
}

// --- authentication ----------------------------------------------------------

#[tokio::test]
async fn every_machine_and_storage_route_requires_a_session() {
    let h = harness("auth").await;
    for (method, path) in [
        ("GET", "/api/vms"),
        ("POST", "/api/vms"),
        ("GET", "/api/vms/100"),
        ("PATCH", "/api/vms/100"),
        ("DELETE", "/api/vms/100"),
        ("POST", "/api/vms/100/start"),
        ("POST", "/api/vms/100/shutdown"),
        ("POST", "/api/vms/100/stop"),
        ("POST", "/api/vms/100/reboot"),
        ("POST", "/api/vms/100/reset"),
        ("GET", "/api/vms/100/tasks"),
        ("GET", "/api/vms/100/console"),
        ("GET", "/api/vms/100/console/ws"),
        ("POST", "/api/vms/100/disks"),
        ("DELETE", "/api/vms/100/disks/vda"),
        ("POST", "/api/vms/100/nics"),
        ("DELETE", "/api/vms/100/nics/52:54:00:00:64:00"),
        ("GET", "/api/storage/pools"),
        ("GET", "/api/storage/pools/boot/volumes"),
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

// --- installation media, end to end ------------------------------------------

/// The whole media path over HTTP: make the library, upload an image, see it
/// listed, define a machine that boots it, and remove it again.
///
/// This is the test the ISO work exists for. Every step is a real request
/// through the real router — the only things mocked are the hypervisor and the
/// pool, exactly as everywhere else in this file.
#[tokio::test]
async fn an_image_is_uploaded_and_a_machine_boots_it() {
    let h = harness("media").await;

    // A fresh node has a pool but no library, and says how to make one rather
    // than showing an empty picker with no explanation.
    let before = h.get("/api/storage/iso").await;
    assert!(before["images"].as_array().unwrap().is_empty());
    let store = &before["stores"][0];
    assert_eq!(store["storage"], "boot");
    assert_eq!(store["ready"], false);
    assert!(store["reason"].as_str().unwrap().contains("zfs create"));

    // Making it creates the dataset. The mock does not mount anything, so the
    // directory is made here the way the mount would have.
    let (status, _) = h.post("/api/storage/iso/boot", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(h.zfs.has_dataset("boot/lumen/iso"));
    std::fs::create_dir_all(h._state_dir.0.join("iso/boot")).unwrap();

    // Upload. The body is the file itself.
    let (status, body) = h
        .put_bytes(
            "/api/storage/iso/boot/almalinux-10.iso",
            b"CD001 pretend media",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["size"], 19);

    let listed = h.get("/api/storage/iso").await;
    let images = listed["images"].as_array().unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0]["name"], "almalinux-10.iso");
    assert_eq!(images[0]["storage"], "boot");

    // The same name twice is refused rather than overwriting an image a
    // machine may already be booting.
    let (status, _) = h
        .put_bytes("/api/storage/iso/boot/almalinux-10.iso", b"different")
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A machine that boots the installer, with the driver disc behind it.
    let (status, vm) = h
        .post(
            "/api/vms",
            r#"{"name":"win01","vcpus":2,"memory_mib":4096,
                "os_id":"http://microsoft.com/win/11",
                "boot_order":["cdrom","disk"],
                "cdroms":[{"storage":"boot","image":"almalinux-10.iso"}],
                "disks":[{"pool":"boot","size_gib":32}],
                "nics":[{"bridge":"br0"}]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{vm}");
    assert_eq!(vm["os_id"], "http://microsoft.com/win/11");
    assert_eq!(vm["cdroms"][0]["id"], "sda");
    assert!(vm["cdroms"][0]["source"]
        .as_str()
        .unwrap()
        .ends_with("almalinux-10.iso"));
    // The media boots before the disk, and it is the devices that carry it.
    assert_eq!(vm["boot_order"][0], "cdrom");
    assert_eq!(vm["cdroms"][0]["boot_index"], 1);

    // Media that is not there is refused, and nothing is left behind.
    let (status, _) = h
        .post(
            "/api/vms",
            r#"{"name":"web02","cdroms":[{"storage":"boot","image":"nothere.iso"}]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // …as is a name that is really a path.
    let (status, _) = h
        .post(
            "/api/vms",
            r#"{"name":"web03","cdroms":[{"storage":"boot","image":"../../etc/passwd.iso"}]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = h.delete("/api/storage/iso/boot/almalinux-10.iso").await;
    assert_eq!(status, StatusCode::OK);
    assert!(h.get("/api/storage/iso").await["images"]
        .as_array()
        .unwrap()
        .is_empty());
}

/// What the create dialog fills its pickers from. Three reads, no side effects.
#[tokio::test]
async fn the_node_reports_what_it_offers_a_new_machine() {
    let h = harness("offers").await;

    // The identifier is advisory: it moves once a machine takes it.
    assert_eq!(h.get("/api/vms/next-id").await["vmid"], 100);
    h.create_web01().await;
    assert_eq!(h.get("/api/vms/next-id").await["vmid"], 101);

    let cpus = h.get("/api/vms/cpu-models").await;
    assert_eq!(cpus["host_model"], "EPYC-Rome");
    assert_eq!(cpus["host_passthrough"], true);
    let models = cpus["models"].as_array().unwrap();
    assert!(models
        .iter()
        .any(|m| m["name"] == "EPYC" && m["usable"] == true));
    assert!(models
        .iter()
        .any(|m| m["name"] == "Skylake-Server" && m["usable"] == false));

    // No database under the test root, so the catalogue is empty and says why
    // rather than failing the request — the console falls back to free text.
    let guests = h.get("/api/vms/os-catalog").await;
    assert!(guests["families"].as_array().unwrap().is_empty());
    assert!(guests["reason"].as_str().unwrap().contains("osinfo-db"));
}
