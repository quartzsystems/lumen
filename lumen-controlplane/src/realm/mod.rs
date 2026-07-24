//! Authentication realms.
//!
//! A realm answers exactly one question: are these credentials valid? The
//! stock realm is `lumen` — the auth built into the OS itself (PAM against
//! the appliance's local accounts). Future realms (LDAP, OIDC, …) implement
//! the same trait and register alongside it.

pub mod lumen;
pub mod pam_ffi;

use async_trait::async_trait;
use serde::Serialize;

/// What backs a realm — surfaced to the UI so it can annotate the dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RealmKind {
    /// The OS's own account database, via PAM.
    Builtin,
}

/// A realm as the login page sees it (GET /api/auth/realms).
#[derive(Debug, Clone, Serialize)]
pub struct RealmInfo {
    pub id: String,
    pub name: String,
    pub kind: RealmKind,
    /// Preselected in the login dropdown.
    pub default: bool,
}

/// Why an authentication attempt failed. `Denied` covers every credential
/// problem (unknown user, wrong password, locked account) so callers can't
/// tell them apart; `Backend` is an operational fault worth logging.
#[derive(Debug)]
pub enum AuthFailure {
    Denied,
    Backend(anyhow::Error),
}

#[async_trait]
pub trait Realm: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn kind(&self) -> RealmKind;
    /// Verify credentials. Blocking backends (PAM) must offload internally.
    async fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthFailure>;
}

/// The ordered set of configured realms. The first registered realm is the
/// default (preselected on the login page).
pub struct RealmRegistry {
    realms: Vec<Box<dyn Realm>>,
}

impl RealmRegistry {
    pub fn new() -> Self {
        Self { realms: Vec::new() }
    }

    pub fn register(mut self, realm: Box<dyn Realm>) -> Self {
        self.realms.push(realm);
        self
    }

    pub fn get(&self, id: &str) -> Option<&dyn Realm> {
        self.realms.iter().find(|r| r.id() == id).map(AsRef::as_ref)
    }

    pub fn default_realm(&self) -> Option<&dyn Realm> {
        self.realms.first().map(AsRef::as_ref)
    }

    pub fn list(&self) -> Vec<RealmInfo> {
        self.realms
            .iter()
            .enumerate()
            .map(|(i, r)| RealmInfo {
                id: r.id().to_string(),
                name: r.name().to_string(),
                kind: r.kind(),
                default: i == 0,
            })
            .collect()
    }
}

impl Default for RealmRegistry {
    fn default() -> Self {
        Self::new()
    }
}
