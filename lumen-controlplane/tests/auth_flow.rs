//! End-to-end API tests over the real router with a mock realm injected in
//! place of PAM (no OS accounts are touched).

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
use lumen_net::NetworkService;
use lumen_virt::VirtService;
use lumen_zfs::StorageService;

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

fn test_app() -> axum::Router {
    // Env-independent config: point the webui at a dir that doesn't exist
    // (the API must work without it) and mark no_tls so cookies skip Secure.
    let mut config = Config::from_env();
    config.webui_dir = std::env::temp_dir().join("lumen-webui-none");
    config.no_tls = true;
    config.session_ttl_secs = 3600;
    let state_dir = std::env::temp_dir().join("lumen-auth-flow-state");
    let network = Arc::new(NetworkService::new(
        Arc::new(MockBackend::appliance()),
        &state_dir,
        60,
    ));
    // These tests are about sessions, not about machines, but the router needs
    // every domain — and every one of them is the in-memory backend, so
    // nothing here touches the runner's networking, storage, or hypervisor.
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
    let cluster = Arc::new(lumen_cluster::ClusterService::new(
        Arc::new(lumen_cluster::backend::mock::MockBackend::appliance()),
        Arc::new(lumen_cluster::MockPeers::new()),
        network.clone(),
        &state_dir,
        "test",
    ));
    let drbd = Arc::new(lumen_drbd::DrbdService::new(
        Arc::new(lumen_drbd::backend::mock::MockBackend::appliance()),
        Arc::new(lumen_drbd::MockVolumePeers::new()),
        cluster.clone(),
        storage.clone(),
    ));
    let state = AppState {
        config,
        jwt_secret: lumen_controlplane::security::session_secret(
            b"test-secret-test-secret-test-secret!".to_vec(),
        ),
        tls: None,
        realms: RealmRegistry::new().register(Box::new(MockRealm)),
        sys,
        network,
        storage,
        virt,
        cluster,
        drbd,
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
    };
    app(Arc::new(state))
}

async fn body_json(res: axum::response::Response) -> serde_json::Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn login_request(body: &str) -> Request<Body> {
    Request::post("/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn realms_lists_builtin_default() {
    let res = test_app()
        .oneshot(
            Request::get("/api/auth/realms")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json = body_json(res).await;
    assert_eq!(
        json,
        serde_json::json!([
            { "id": "lumen", "name": "Lumen", "kind": "builtin", "default": true }
        ])
    );
}

#[tokio::test]
async fn login_success_sets_cookie_and_me_works() {
    let app = test_app();
    let res = app
        .clone()
        .oneshot(login_request(
            r#"{"username":"root","password":"correct-horse","realm":"lumen"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .expect("login must set the session cookie")
        .to_str()
        .unwrap()
        .to_string();
    assert!(cookie.starts_with("lumen_auth="));
    assert!(cookie.contains("HttpOnly"));
    let json = body_json(res).await;
    assert_eq!(json["username"], "root");
    assert_eq!(json["realm"], "lumen");

    // The ticket in the cookie authenticates /me.
    let ticket = cookie.split(';').next().unwrap().to_string();
    let res = app
        .oneshot(
            Request::get("/api/auth/me")
                .header(header::COOKIE, ticket)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json = body_json(res).await;
    assert_eq!(json["username"], "root");
}

#[tokio::test]
async fn login_wrong_password_is_401() {
    let res = test_app()
        .oneshot(login_request(r#"{"username":"root","password":"wrong"}"#))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(res).await;
    assert_eq!(json["error"], "Invalid username or password.");
}

#[tokio::test]
async fn login_unknown_realm_is_400() {
    let res = test_app()
        .oneshot(login_request(
            r#"{"username":"root","password":"correct-horse","realm":"ldap"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn login_defaults_to_first_realm() {
    let res = test_app()
        .oneshot(login_request(
            r#"{"username":"root","password":"correct-horse"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json = body_json(res).await;
    assert_eq!(json["realm"], "lumen");
}

#[tokio::test]
async fn me_without_session_is_401() {
    let res = test_app()
        .oneshot(Request::get("/api/auth/me").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_with_garbage_ticket_is_401() {
    let res = test_app()
        .oneshot(
            Request::get("/api/auth/me")
                .header(header::COOKIE, "lumen_auth=not-a-ticket")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn logout_clears_cookie() {
    let res = test_app()
        .oneshot(
            Request::post("/api/auth/logout")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .expect("logout must clear the cookie")
        .to_str()
        .unwrap();
    assert!(cookie.contains("Max-Age=0"));
}

#[tokio::test]
async fn version_reports() {
    let res = test_app()
        .oneshot(Request::get("/api/version").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let json = body_json(res).await;
    assert!(json["version"].as_str().is_some_and(|v| !v.is_empty()));
}
