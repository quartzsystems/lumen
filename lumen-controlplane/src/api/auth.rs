//! Session endpoints. Credentials are verified by a realm; success sets an
//! httpOnly session-ticket cookie (JS never sees it), mirroring the Quartz
//! Command model. The response body is the signed-in principal, for display.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::{Deserialize, Serialize};
use time::Duration;

use crate::error::ApiError;
use crate::realm::{AuthFailure, RealmInfo};
use crate::security::{self, SESSION_COOKIE};
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    /// Realm id from the login dropdown; the default realm when omitted.
    pub realm: Option<String>,
}

/// The signed-in principal, `user@realm` style split into parts.
#[derive(Debug, Serialize)]
pub struct SessionUser {
    pub username: String,
    pub realm: String,
}

/// GET /api/auth/realms — for the login page's Realm dropdown. Public.
pub async fn realms(State(state): State<Arc<AppState>>) -> Json<Vec<RealmInfo>> {
    Json(state.realms.list())
}

/// POST /api/auth/login
pub async fn login(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
    Json(req): Json<LoginRequest>,
) -> Result<(CookieJar, Json<SessionUser>), ApiError> {
    let realm = match &req.realm {
        Some(id) => state
            .realms
            .get(id)
            .ok_or_else(|| ApiError::BadRequest(format!("Unknown realm \"{id}\".")))?,
        None => state
            .realms
            .default_realm()
            .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("no realms configured")))?,
    };

    match realm.authenticate(&req.username, &req.password).await {
        Ok(()) => {}
        Err(AuthFailure::Denied) => {
            tracing::info!(
                username = %req.username,
                realm = %realm.id(),
                "login denied"
            );
            return Err(ApiError::Unauthorized);
        }
        Err(AuthFailure::Backend(err)) => return Err(ApiError::Internal(err)),
    }

    let ticket = security::issue_ticket(
        &state.jwt_secret,
        &req.username,
        realm.id(),
        state.config.session_ttl_secs,
    )?;
    tracing::info!(username = %req.username, realm = %realm.id(), "login ok");

    let user = SessionUser {
        username: req.username,
        realm: realm.id().to_string(),
    };
    Ok((jar.add(session_cookie(&state, ticket)), Json(user)))
}

/// POST /api/auth/logout — clears the cookie. Always succeeds.
pub async fn logout(State(state): State<Arc<AppState>>, jar: CookieJar) -> CookieJar {
    let mut removal = session_cookie(&state, String::new());
    removal.make_removal();
    jar.add(removal)
}

/// GET /api/auth/me — the current session's principal, or 401.
pub async fn me(
    State(state): State<Arc<AppState>>,
    jar: CookieJar,
) -> Result<Json<SessionUser>, ApiError> {
    let claims = jar
        .get(SESSION_COOKIE)
        .and_then(|c| security::verify_ticket(&state.jwt_secret, c.value()))
        .ok_or(ApiError::Unauthorized)?;
    Ok(Json(SessionUser {
        username: claims.sub,
        realm: claims.realm,
    }))
}

/// The session cookie, attributes shared by set and removal so they always
/// address the same cookie. `Secure` is dropped only in the plain-HTTP dev
/// mode, where the browser would otherwise refuse to store it.
fn session_cookie(state: &AppState, value: String) -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE, value);
    cookie.set_http_only(true);
    cookie.set_secure(!state.config.no_tls);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path("/");
    cookie.set_max_age(Duration::seconds(state.config.session_ttl_secs as i64));
    cookie
}
