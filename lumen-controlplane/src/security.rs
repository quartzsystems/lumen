use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Name of the httpOnly session cookie.
pub const SESSION_COOKIE: &str = "lumen_auth";

/// Session ticket claims. `sub` is the OS/realm username; together with
/// `realm` it forms the Proxmox-style principal `user@realm`.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionClaims {
    pub sub: String,
    pub realm: String,
    pub iat: u64,
    pub exp: u64,
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
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret),
    )
    .context("signing session ticket")
}

/// Verify a ticket and return its claims; `None` for anything invalid or
/// expired (jsonwebtoken validates `exp` by default).
pub fn verify_ticket(secret: &[u8], token: &str) -> Option<SessionClaims> {
    decode::<SessionClaims>(
        token,
        &DecodingKey::from_secret(secret),
        &Validation::default(),
    )
    .map(|data| data.claims)
    .ok()
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
