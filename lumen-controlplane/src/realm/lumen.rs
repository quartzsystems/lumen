//! The built-in `lumen` realm: the appliance's own accounts, via PAM.
//!
//! Authentication goes through the `lumen-controlplane` PAM service (the
//! pam/ dir in this crate ships the /etc/pam.d file, installed by packaging),
//! so the OS — shadow, faillock, SELinux — remains the source of truth.
//! Today the appliance has a single account (root, password chosen in the
//! installer); any local account PAM accepts can sign in.

use anyhow::anyhow;
use async_trait::async_trait;

use super::{AuthFailure, Realm, RealmKind};

pub const REALM_ID: &str = "lumen";

pub struct LumenRealm {
    pam_service: String,
}

impl LumenRealm {
    pub fn new(pam_service: String) -> Self {
        Self { pam_service }
    }
}

/// Reject anything that isn't a plausible local username before it reaches
/// PAM: empty names, whitespace/control characters, or oversized input.
fn valid_username(username: &str) -> bool {
    !username.is_empty()
        && username.len() <= 64
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[async_trait]
impl Realm for LumenRealm {
    fn id(&self) -> &str {
        REALM_ID
    }

    fn name(&self) -> &str {
        "Lumen"
    }

    fn kind(&self) -> RealmKind {
        RealmKind::Builtin
    }

    async fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthFailure> {
        if !valid_username(username) || password.is_empty() {
            return Err(AuthFailure::Denied);
        }
        let service = self.pam_service.clone();
        let username = username.to_string();
        let password = password.to_string();
        // libpam calls block (and may sleep on failure); keep them off the
        // async runtime.
        tokio::task::spawn_blocking(move || {
            // Wrong credentials and disabled/expired accounts both end up
            // Denied; the distinction stays inside PAM's logs.
            match super::pam_ffi::authenticate(&service, &username, &password) {
                Ok(()) => Ok(()),
                Err(super::pam_ffi::PamError::Denied) => Err(AuthFailure::Denied),
                Err(super::pam_ffi::PamError::Setup(msg)) => {
                    Err(AuthFailure::Backend(anyhow!("PAM setup failed: {msg}")))
                }
            }
        })
        .await
        .map_err(|e| AuthFailure::Backend(anyhow!("PAM task panicked: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_validation() {
        assert!(valid_username("root"));
        assert!(valid_username("svc-backup_2.prod"));
        assert!(!valid_username(""));
        assert!(!valid_username("has space"));
        assert!(!valid_username("evil\nuser"));
        assert!(!valid_username(&"a".repeat(65)));
    }

    #[tokio::test]
    async fn empty_credentials_denied_without_pam() {
        let realm = LumenRealm::new("lumen-controlplane".into());
        assert!(matches!(
            realm.authenticate("", "x").await,
            Err(AuthFailure::Denied)
        ));
        assert!(matches!(
            realm.authenticate("root", "").await,
            Err(AuthFailure::Denied)
        ));
    }
}
