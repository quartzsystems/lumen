use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Name of the httpOnly session cookie.
pub const SESSION_COOKIE: &str = "lumen_auth";

/// The live session-signing secret, shared by every verifier and swappable
/// at runtime: joining an environment replaces this node's own secret with
/// the environment's, and every holder sees the swap at its next read.
pub type SessionSecret = std::sync::Arc<std::sync::RwLock<Vec<u8>>>;

pub fn session_secret(bytes: Vec<u8>) -> SessionSecret {
    std::sync::Arc::new(std::sync::RwLock::new(bytes))
}

/// Session ticket claims. `sub` is the OS/realm username; together with
/// `realm` it forms the Proxmox-style principal `user@realm`. `kind` is what
/// keeps the two ticket populations apart: a browser session never carries
/// one, a peer ticket always does, and each verifier refuses the other's —
/// the same secret signs both, so the claim is the boundary.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionClaims {
    pub sub: String,
    pub realm: String,
    pub iat: u64,
    pub exp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// Load the session-signing secret, minting one (0600, 64 random bytes) on
/// first boot. Sessions survive daemon restarts but a deleted secret file
/// invalidates every outstanding ticket — the appliance "log everyone out".
pub fn load_or_create_secret(path: &Path) -> Result<Vec<u8>> {
    if let Ok(bytes) = fs::read(path) {
        if bytes.len() >= 32 {
            return Ok(bytes);
        }
    }
    let mut secret = vec![0u8; 64];
    rand::thread_rng().fill_bytes(&mut secret);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating state dir {}", parent.display()))?;
    }
    fs::write(path, &secret).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(secret)
}

/// Persist a replaced secret — the join flow writing the environment's
/// shared secret over this node's own, so a restart keeps the environment
/// sessions rather than resurrecting the old ones.
pub fn store_secret(path: &Path, secret: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating state dir {}", parent.display()))?;
    }
    fs::write(path, secret).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs()
}

/// Issue a session ticket for an authenticated principal.
pub fn issue_ticket(secret: &[u8], username: &str, realm: &str, ttl_secs: u64) -> Result<String> {
    let iat = now_unix();
    let claims = SessionClaims {
        sub: username.to_string(),
        realm: realm.to_string(),
        iat,
        exp: iat + ttl_secs,
        kind: None,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret),
    )
    .context("signing session ticket")
}

/// Verify a ticket and return its claims; `None` for anything invalid or
/// expired (jsonwebtoken validates `exp` by default). A peer ticket is not a
/// session, whatever it is signed with.
pub fn verify_ticket(secret: &[u8], token: &str) -> Option<SessionClaims> {
    decode::<SessionClaims>(
        token,
        &DecodingKey::from_secret(secret),
        &Validation::default(),
    )
    .map(|data| data.claims)
    .ok()
    .filter(|claims| claims.kind.is_none())
}

/// How long a peer ticket lives. It authenticates exactly one call between
/// two control planes that already share a secret; a minute of validity is
/// generous and a stolen ticket is stale before it is interesting.
const PEER_TICKET_TTL_SECS: u64 = 60;

/// Issue a ticket for one control plane calling another. Signed with the
/// same environment-shared secret as browser sessions and separated from
/// them by the `kind` claim.
pub fn issue_peer_ticket(secret: &[u8], node: &str) -> Result<String> {
    let iat = now_unix();
    let claims = SessionClaims {
        sub: node.to_string(),
        realm: "environment".to_string(),
        iat,
        exp: iat + PEER_TICKET_TTL_SECS,
        kind: Some("peer".to_string()),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret),
    )
    .context("signing peer ticket")
}

/// Verify a peer ticket and return the calling node's name. A browser
/// session is not a peer, whatever it is signed with.
pub fn verify_peer_ticket(secret: &[u8], token: &str) -> Option<String> {
    decode::<SessionClaims>(
        token,
        &DecodingKey::from_secret(secret),
        &Validation::default(),
    )
    .ok()
    .filter(|data| data.claims.kind.as_deref() == Some("peer"))
    .map(|data| data.claims.sub)
}

/// A verified session, as an extractor. Any handler that takes one is a
/// protected route: no valid ticket, no handler.
pub struct Session(pub SessionClaims);

impl axum::extract::FromRequestParts<std::sync::Arc<crate::AppState>> for Session {
    type Rejection = crate::error::ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &std::sync::Arc<crate::AppState>,
    ) -> Result<Self, Self::Rejection> {
        use axum_extra::extract::cookie::CookieJar;
        let jar = CookieJar::from_headers(&parts.headers);
        let secret = state.jwt_secret.read().expect("secret lock").clone();
        jar.get(SESSION_COOKIE)
            .and_then(|cookie| verify_ticket(&secret, cookie.value()))
            .map(Session)
            .ok_or(crate::error::ApiError::Unauthorized)
    }
}

/// A verified peer — another control plane in this environment — as an
/// extractor. Carries the calling node's name. Reads the `Authorization`
/// bearer header, never the cookie jar: a browser can never wander onto a
/// peer route, and a peer never holds a session.
pub struct PeerSession(pub String);

impl axum::extract::FromRequestParts<std::sync::Arc<crate::AppState>> for PeerSession {
    type Rejection = crate::error::ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &std::sync::Arc<crate::AppState>,
    ) -> Result<Self, Self::Rejection> {
        let secret = state.jwt_secret.read().expect("secret lock").clone();
        parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|token| verify_peer_ticket(&secret, token))
            .map(PeerSession)
            .ok_or(crate::error::ApiError::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_round_trip() {
        let secret = b"0123456789abcdef0123456789abcdef";
        let token = issue_ticket(secret, "root", "lumen", 60).unwrap();
        let claims = verify_ticket(secret, &token).expect("ticket should verify");
        assert_eq!(claims.sub, "root");
        assert_eq!(claims.realm, "lumen");
    }

    #[test]
    fn expired_ticket_rejected() {
        let secret = b"0123456789abcdef0123456789abcdef";
        // jsonwebtoken's default validation has 60s leeway; go well past it.
        let iat = now_unix() - 600;
        let claims = SessionClaims {
            sub: "root".into(),
            realm: "lumen".into(),
            iat,
            exp: iat + 1,
            kind: None,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap();
        assert!(verify_ticket(secret, &token).is_none());
    }

    #[test]
    fn wrong_secret_rejected() {
        let token = issue_ticket(b"0123456789abcdef0123456789abcdef", "root", "lumen", 60).unwrap();
        assert!(verify_ticket(b"another-secret-another-secret-!!", &token).is_none());
    }

    /// The check the `kind` claim exists for: the two ticket populations
    /// share a secret, and neither may pass as the other.
    #[test]
    fn a_peer_ticket_is_not_a_session_and_a_session_is_not_a_peer() {
        let secret = b"0123456789abcdef0123456789abcdef";
        let peer = issue_peer_ticket(secret, "alpha-2").unwrap();
        assert!(verify_ticket(secret, &peer).is_none());
        assert_eq!(
            verify_peer_ticket(secret, &peer).as_deref(),
            Some("alpha-2")
        );

        let session = issue_ticket(secret, "root", "lumen", 60).unwrap();
        assert!(verify_peer_ticket(secret, &session).is_none());
        assert!(verify_ticket(secret, &session).is_some());
    }

    #[test]
    fn secret_persists_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-secret");
        let first = load_or_create_secret(&path).unwrap();
        let second = load_or_create_secret(&path).unwrap();
        assert_eq!(first, second);
        assert!(first.len() >= 32);
    }
}
