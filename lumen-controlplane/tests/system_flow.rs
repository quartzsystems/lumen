//! End-to-end tests for the node's own pages — accounts, power, and pools —
//! over the real router, with the in-memory backends injected.
//!
//! Nothing here reads or writes the accounts on the machine running the tests:
//! the account database is four files in a temporary directory, the power
//! backend records what it was asked to do instead of doing it, and the
//! privileged-command runner records commands instead of running them. `make
//! test` therefore cannot lock anybody out of anything.

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
use lumen_sys::backend::PowerBackend;
use lumen_sys::exec::MockExec;
use lumen_sys::{AccountFiles, SysService};
use lumen_virt::VirtService;
use lumen_zfs::backend::mock::MockBackend as MockZfsBackend;
use lumen_zfs::StorageService;

const TICKET_SECRET: &[u8] = b"test-secret-test-secret-test-secret!";

/// Signs in as `alice`, who is one of the two administrators on the pretend
/// node below — so the "you cannot lock yourself out" rules have somebody to
/// be about.
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
        if password == "correct-horse" && (username == "alice" || username == "carol") {
            Ok(())
        } else {
            Err(AuthFailure::Denied)
        }
    }
}

struct Harness {
    router: axum::Router,
    exec: Arc<MockExec>,
    power: Arc<MockPower>,
    zfs: Arc<MockZfsBackend>,
    cookie: String,
    _dir: TempDir,
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A node with root, two administrators, and one locked ordinary account, plus
/// four disks — one of which the appliance is running from.
async fn harness(tag: &str, signed_in_as: &str) -> Harness {
    let dir = TempDir(std::env::temp_dir().join(format!(
        "lumen-cp-sys-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    )));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    std::fs::write(
        dir.0.join("passwd"),
        "root:x:0:0:root:/root:/bin/bash\n\
         bin:x:1:1:bin:/bin:/sbin/nologin\n\
         alice:x:1000:1000:Alice Kowalski:/home/alice:/bin/bash\n\
         bob:x:1001:1001::/home/bob:/bin/bash\n\
         carol:x:1002:1002:Carol:/home/carol:/bin/bash\n",
    )
    .unwrap();
    std::fs::write(
        dir.0.join("group"),
        "root:x:0:\nbin:x:1:\nwheel:x:10:alice,carol\n\
         alice:x:1000:\nbob:x:1001:\ncarol:x:1002:\n",
    )
    .unwrap();
    std::fs::write(
        dir.0.join("shadow"),
        "root:$6$a$b:20000:0:99999:7:::\n\
         alice:$6$c$d:20100:0:99999:7:::\n\
         bob:!$6$e$f:20050:0:99999:7:::\n\
         carol:$6$g$h:20060:0:99999:7:::\n",
    )
    .unwrap();
    std::fs::write(dir.0.join("shells"), "/bin/sh\n/bin/bash\n").unwrap();

    let mut config = Config::from_env();
    config.webui_dir = std::env::temp_dir().join("lumen-webui-none");
    config.no_tls = true;
    config.session_ttl_secs = 3600;
    config.net_confirm_secs = 60;

    // The fake applies useradd/usermod/userdel/chpasswd to those same four
    // files, so the round trip the service actually performs — write, then
    // read the answer back out of the node — is what these tests exercise.
    let exec = Arc::new(MockExec::new().backed_by(AccountFiles::under(&dir.0)));
    let power = Arc::new(MockPower::appliance());
    let sys = Arc::new(
        SysService::new(power.clone(), exec.clone())
            .with_account_files(AccountFiles::under(&dir.0))
            .with_node("lumen"),
    );

    const TB: u64 = 1_000_000_000_000;
    let zfs = Arc::new(MockZfsBackend::appliance().with_disks(vec![
        MockZfsBackend::busy_disk("sda", TB),
        MockZfsBackend::free_disk("sdb", TB),
        MockZfsBackend::free_disk("sdc", TB),
        MockZfsBackend::free_disk("sdd", TB),
    ]));
    let storage = Arc::new(
        StorageService::new(zfs.clone())
            .with_iso_root(dir.0.join("iso"))
            // The appliance is installed on boot, which is therefore the one
            // pool the console will not destroy.
            .with_root_pool(Some("boot".into())),
    );

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
        import: Default::default(),
    }));

    let response = router
        .clone()
        .oneshot(
            Request::post("/api/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"username":"{signed_in_as}","password":"correct-horse"}}"#
                )))
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
        exec,
        power,
        zfs,
        cookie,
        _dir: dir,
    }
}

impl Harness {
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, &self.cookie);
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
}

/// The codes on a rejected request, which is what the console pins to fields.
fn codes(body: &serde_json::Value) -> Vec<String> {
    body["errors"]
        .as_array()
        .map(|errors| {
            errors
                .iter()
                .filter_map(|e| e["code"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// --- accounts ----------------------------------------------------------------

#[tokio::test]
async fn the_node_lists_its_own_accounts_with_what_may_be_done_to_each() {
    let h = harness("list", "alice").await;
    let body = h.get("/api/system/users").await;

    assert_eq!(body["node"], "lumen");
    assert_eq!(body["admin_group"], "wheel");
    assert_eq!(body["shells"][0], "/bin/bash");

    let users = body["users"].as_array().unwrap();
    let by_name = |name: &str| {
        users
            .iter()
            .find(|u| u["name"] == name)
            .unwrap_or_else(|| panic!("{name} should be listed"))
    };

    let alice = by_name("alice");
    assert_eq!(alice["uid"], 1000);
    assert_eq!(alice["full_name"], "Alice Kowalski");
    assert_eq!(alice["administrator"], true);
    assert_eq!(alice["login"], "enabled");
    assert_eq!(alice["is_you"], true);

    // Three different noes, and the console needs to tell them apart.
    assert_eq!(by_name("bob")["login"], "locked");
    assert_eq!(by_name("bin")["login"], "nologin");
    assert_eq!(by_name("bin")["system"], true);

    // root is listed and visibly not something this page changes.
    let root = by_name("root");
    assert_eq!(root["actions"]["delete"]["allowed"], false);
    assert!(root["actions"]["delete"]["reason"]
        .as_str()
        .unwrap()
        .contains("recovery account"));
}

#[tokio::test]
async fn an_account_is_created_and_its_password_never_becomes_an_argument() {
    let h = harness("create", "alice").await;
    let (status, body) = h
        .call(
            "POST",
            "/api/system/users",
            Some(r#"{"name":"dave","password":"correct-horse-battery","full_name":"Dave Lister","administrator":true}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let ran = h.exec.ran().await;
    assert!(
        ran.iter().all(|r| !r.display().contains("correct-horse")),
        "the password must never be an argument: {:#?}",
        ran.iter().map(|r| r.display()).collect::<Vec<_>>()
    );
    assert_eq!(
        h.exec.last_stdin().await.as_deref(),
        Some("dave:correct-horse-battery\n")
    );
    assert!(
        h.exec
            .ran_with(
                "/usr/sbin/useradd",
                &[
                    "-m",
                    "-s",
                    "/bin/bash",
                    "-c",
                    "Dave Lister",
                    "-G",
                    "wheel",
                    "dave"
                ]
            )
            .await
    );

    // The response is the account, and it carries no password field at all.
    assert_eq!(body["name"], "dave");
    assert!(body.get("password").is_none());
}

#[tokio::test]
async fn a_rejected_account_carries_codes_and_fields_and_touches_nothing() {
    let h = harness("rejected", "alice").await;
    let (status, body) = h
        .call(
            "POST",
            "/api/system/users",
            Some(r#"{"name":"Bad Name","password":"x","shell":"/bin/nope"}"#),
        )
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let codes = codes(&body);
    assert!(codes.contains(&"invalid_username".to_string()), "{body}");
    assert!(codes.contains(&"password_too_short".to_string()), "{body}");
    assert!(codes.contains(&"invalid_shell".to_string()), "{body}");
    // Every one names the field the dialog pins it to.
    assert!(body["errors"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["field"].is_string()));
    assert!(h.exec.ran().await.is_empty(), "nothing may have run");
}

#[tokio::test]
async fn an_account_that_already_exists_is_refused_by_name() {
    let h = harness("duplicate", "alice").await;
    let (status, body) = h
        .call(
            "POST",
            "/api/system/users",
            Some(r#"{"name":"bob","password":"correct-horse"}"#),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(codes(&body), vec!["duplicate_username"]);
}

/// The rule the whole appliance is built around, over HTTP: nothing the
/// console offers may take the console away from the operator using it.
#[tokio::test]
async fn you_cannot_lock_yourself_out_through_the_api() {
    let h = harness("self", "alice").await;

    for (method, path, body) in [
        ("PATCH", "/api/system/users/alice", r#"{"locked":true}"#),
        (
            "PATCH",
            "/api/system/users/alice",
            r#"{"administrator":false}"#,
        ),
        ("DELETE", "/api/system/users/alice", "{}"),
    ] {
        let (status, answer) = h.call(method, path, Some(body)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{method} {path} -> {answer}"
        );
        assert!(
            codes(&answer).contains(&"would_lock_you_out".to_string()),
            "{answer}"
        );
    }
    assert!(h.exec.ran().await.is_empty(), "nothing may have run");

    // Changing your own password is the ordinary case and is allowed.
    let (status, body) = h
        .call(
            "PATCH",
            "/api/system/users/alice",
            Some(r#"{"password":"a-brand-new-password"}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// One step removed from the same failure. Signed in as carol, locking the
/// only *other* administrator is fine — locking the last one is not.
#[tokio::test]
async fn the_last_administrator_cannot_be_locked_out_either() {
    let h = harness("last-admin", "carol").await;

    // alice is one of two administrators, so this is allowed.
    let (status, body) = h
        .call(
            "PATCH",
            "/api/system/users/alice",
            Some(r#"{"locked":true}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(h.exec.ran_with("/usr/sbin/usermod", &["-L", "alice"]).await);
}

#[tokio::test]
async fn root_is_not_changed_or_removed_from_a_web_page() {
    let h = harness("root", "alice").await;
    for (method, body) in [
        ("PATCH", r#"{"password":"a-brand-new-password"}"#),
        ("DELETE", r#"{"i_understand_this_may_lose_data":true}"#),
    ] {
        let (status, answer) = h.call(method, "/api/system/users/root", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} -> {answer}");
        assert!(
            codes(&answer).contains(&"reserved_username".to_string()),
            "{answer}"
        );
    }
    assert!(h.exec.ran().await.is_empty());
}

#[tokio::test]
async fn removing_an_account_says_where_its_files_went_or_did_not() {
    let h = harness("delete", "alice").await;

    // The home directory is kept unless asked for, and the answer says where.
    let (status, body) = h.call("DELETE", "/api/system/users/bob", Some("{}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["kept_home"], "/home/bob");
    assert!(body.get("removed_home").is_none());
    assert!(h.exec.ran_with("/usr/sbin/userdel", &["bob"]).await);
}

#[tokio::test]
async fn destroying_a_home_directory_needs_the_acknowledgement() {
    let h = harness("delete-home", "alice").await;
    let (status, body) = h
        .call(
            "DELETE",
            "/api/system/users/bob",
            Some(r#"{"remove_home":true}"#),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(codes(&body), vec!["unacknowledged_destructive_operation"]);
    assert!(h.exec.ran().await.is_empty());

    let (status, body) = h
        .call(
            "DELETE",
            "/api/system/users/bob",
            Some(r#"{"remove_home":true,"i_understand_this_may_lose_data":true}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["removed_home"], "/home/bob");
    assert!(h.exec.ran_with("/usr/sbin/userdel", &["-r", "bob"]).await);
}

#[tokio::test]
async fn an_account_that_is_not_there_is_a_not_found() {
    let h = harness("missing", "alice").await;
    let (status, _) = h.call("GET", "/api/system/users/nobody-here", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_typo_in_an_account_request_is_rejected_not_ignored() {
    let h = harness("typo", "alice").await;
    let (status, _) = h
        .call(
            "POST",
            "/api/system/users",
            Some(r#"{"name":"dave","password":"correct-horse","administratorr":true}"#),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// --- power -------------------------------------------------------------------

#[tokio::test]
async fn a_restart_can_be_scheduled_read_back_and_called_off() {
    let h = harness("schedule", "alice").await;

    let before = h.get("/api/system/power").await;
    assert_eq!(before["node"], "lumen");
    assert!(before["scheduled"].is_null());
    // The node's own clock, so the console counts down against it rather than
    // against a duration computed here.
    let now = before["now"].as_u64().unwrap();
    assert!(now > 1_700_000_000);

    let (status, body) = h
        .call(
            "POST",
            "/api/system/power",
            Some(&format!(r#"{{"action":"reboot","at":{}}}"#, now + 1800)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scheduled"]["action"], "reboot");
    assert_eq!(body["scheduled"]["at"], now + 1800);

    // It reads back on its own, because logind is holding it and not us.
    let after = h.get("/api/system/power").await;
    assert_eq!(after["scheduled"]["at"], now + 1800);

    let (status, body) = h.call("DELETE", "/api/system/power", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["scheduled"].is_null());

    // Cancelling twice is a conflict rather than a silent success.
    let (status, _) = h.call("DELETE", "/api/system/power", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_schedule_in_the_past_never_reaches_the_node() {
    let h = harness("past", "alice").await;
    let now = h.get("/api/system/power").await["now"].as_u64().unwrap();

    let (status, body) = h
        .call(
            "POST",
            "/api/system/power",
            Some(&format!(r#"{{"action":"reboot","at":{}}}"#, now - 60)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(codes(&body), vec!["time_in_the_past"]);
    assert!(h.power.scheduled().await.unwrap().is_none());
}

/// The connection is about to go away, so a body claiming success would be a
/// promise this daemon cannot keep.
#[tokio::test]
async fn an_immediate_restart_is_accepted_rather_than_answered() {
    use lumen_sys::PowerAction;
    let h = harness("now", "alice").await;

    let (status, _) = h
        .call("POST", "/api/system/power", Some(r#"{"action":"reboot"}"#))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(h.power.performed().await, vec![PowerAction::Reboot]);

    let (status, _) = h
        .call(
            "POST",
            "/api/system/power",
            Some(r#"{"action":"power_off"}"#),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        h.power.performed().await,
        vec![PowerAction::Reboot, PowerAction::PowerOff]
    );
}

/// Somebody ran `shutdown -r +30` at the keyboard; the console must show it,
/// because there is only one schedule and it is the node's.
#[tokio::test]
async fn a_schedule_somebody_else_set_shows_up_here() {
    use lumen_sys::PowerAction;
    let h = harness("foreign", "alice").await;
    let now = h.get("/api/system/power").await["now"].as_u64().unwrap();
    h.power
        .preset_schedule(PowerAction::PowerOff, now + 900)
        .await;

    let body = h.get("/api/system/power").await;
    assert_eq!(body["scheduled"]["action"], "power_off");
}

// --- pools -------------------------------------------------------------------

#[tokio::test]
async fn the_picker_says_what_is_already_on_every_disk() {
    let h = harness("devices", "alice").await;
    let body = h.get("/api/storage/devices").await;

    assert_eq!(body["root_pool"], "boot");
    let devices = body["devices"].as_array().unwrap();
    let sda = devices.iter().find(|d| d["name"] == "sda").unwrap();
    assert_eq!(sda["in_use"], true);
    assert_eq!(sda["used_by"], "mounted at /");

    let sdb = devices.iter().find(|d| d["name"] == "sdb").unwrap();
    assert_eq!(sdb["in_use"], false);
    // The stable path, not the kernel name the enumeration order produced.
    assert_eq!(sdb["path"], "/dev/disk/by-id/scsi-sdb");
}

#[tokio::test]
async fn a_pool_is_created_on_the_disks_that_were_chosen() {
    let h = harness("create-pool", "alice").await;
    let (status, body) = h
        .call(
            "POST",
            "/api/storage/pools",
            Some(r#"{"name":"tank","vdev":"raidz1","disks":["sdb","sdc","sdd"]}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "tank");
    assert_eq!(body["health"], "online");
    // Not the pool the appliance runs from, so it may be removed again.
    assert_eq!(body["destroyable"], true);

    let built = h.zfs.created_pools();
    assert_eq!(built.len(), 1);
    // The stable paths, whatever the request named them by.
    assert_eq!(
        built[0].disks,
        [
            "/dev/disk/by-id/scsi-sdb",
            "/dev/disk/by-id/scsi-sdc",
            "/dev/disk/by-id/scsi-sdd"
        ]
    );
    assert!(!built[0].force);

    // And it shows up in the listing without anything else being told.
    let pools = h.get("/api/storage/pools").await;
    let names: Vec<&str> = pools["nodes"][0]["pools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"tank"), "{names:?}");
}

/// The single most effective way to lose a node, and the check that stops it.
#[tokio::test]
async fn a_pool_cannot_be_built_on_the_running_system_by_accident() {
    let h = harness("busy-disk", "alice").await;
    let (status, body) = h
        .call(
            "POST",
            "/api/storage/pools",
            Some(r#"{"name":"tank","vdev":"stripe","disks":["sda"]}"#),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(codes(&body), vec!["disk_in_use"]);
    // The message says what is on it rather than only that something is.
    assert!(
        body["error"].as_str().unwrap().contains("mounted at /"),
        "{body}"
    );
    assert!(h.zfs.created_pools().is_empty(), "nothing may have run");

    // Acknowledged — an operator rebuilding a node has to be able to say so.
    let (status, body) = h
        .call(
            "POST",
            "/api/storage/pools",
            Some(
                r#"{"name":"tank","vdev":"stripe","disks":["sda"],"i_understand_this_may_lose_data":true}"#,
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(h.zfs.created_pools()[0].force, "-f only then");
}

#[tokio::test]
async fn a_rejected_pool_carries_every_problem_and_touches_no_disk() {
    let h = harness("bad-pool", "alice").await;
    let (status, body) = h
        .call(
            "POST",
            "/api/storage/pools",
            Some(r#"{"name":"mirror","vdev":"raidz2","disks":["sdb","sdb"]}"#),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let codes = codes(&body);
    for expected in ["invalid_pool_name", "duplicate_disk", "not_enough_disks"] {
        assert!(codes.contains(&expected.to_string()), "{expected} {body}");
    }
    assert!(h.zfs.created_pools().is_empty());
}

/// The same rule the account page keeps for `root`.
#[tokio::test]
async fn the_pool_the_appliance_is_installed_on_is_never_destroyed() {
    let h = harness("root-pool", "alice").await;
    let (status, body) = h
        .call(
            "DELETE",
            "/api/storage/pools/boot",
            Some(r#"{"i_understand_this_may_lose_data":true}"#),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("installed on"));
    assert!(h.zfs.has_pool("boot"));
}

#[tokio::test]
async fn destroying_a_pool_needs_the_acknowledgement() {
    let h = harness("destroy-pool", "alice").await;
    h.call(
        "POST",
        "/api/storage/pools",
        Some(r#"{"name":"tank","vdev":"mirror","disks":["sdb","sdc"]}"#),
    )
    .await;

    let (status, body) = h
        .call("DELETE", "/api/storage/pools/tank", Some("{}"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(codes(&body), vec!["unacknowledged_destructive_operation"]);
    assert!(h.zfs.has_pool("tank"));

    let (status, body) = h
        .call(
            "DELETE",
            "/api/storage/pools/tank",
            Some(r#"{"i_understand_this_may_lose_data":true}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!h.zfs.has_pool("tank"));
}

// --- authentication ----------------------------------------------------------

#[tokio::test]
async fn every_system_and_pool_route_requires_a_session() {
    let h = harness("auth", "alice").await;
    for (method, path) in [
        ("GET", "/api/system/users"),
        ("POST", "/api/system/users"),
        ("GET", "/api/system/users/alice"),
        ("PATCH", "/api/system/users/alice"),
        ("DELETE", "/api/system/users/alice"),
        ("GET", "/api/system/power"),
        ("POST", "/api/system/power"),
        ("DELETE", "/api/system/power"),
        ("GET", "/api/storage/devices"),
        ("POST", "/api/storage/pools"),
        ("DELETE", "/api/storage/pools/tank"),
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
