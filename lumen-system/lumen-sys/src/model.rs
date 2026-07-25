//! What a local account is, in Lumen's terms, and what a power request is.
//!
//! There is no database here either. The account database *is* `/etc/passwd`,
//! `/etc/shadow`, and `/etc/group` — the same three files every other tool on
//! the node reads — so nothing is cached and nothing can disagree with `getent`.
//!
//! Every request type is `deny_unknown_fields`: a typo in an API request is a
//! 400, not a silently ignored setting.

use serde::{Deserialize, Serialize};

/// Below this, an account belongs to a package rather than to a person. It is
/// `UID_MIN` from `/etc/login.defs` on every distribution this appliance is
/// built from, and `useradd` uses the same number when it allocates one.
pub const FIRST_HUMAN_UID: u32 = 1000;

/// The group that grants administrative rights. The `lumen` realm authenticates
/// against PAM, which is to say against the node's own accounts, so "can
/// administer this appliance" is a question about this group and not about a
/// role table Lumen keeps.
pub const ADMIN_GROUP: &str = "wheel";

/// The shell an account gets when nothing else is asked for.
pub const DEFAULT_SHELL: &str = "/bin/bash";

/// A shell that exists to refuse. An account with one can hold files and own a
/// service but cannot sign in — which is a real thing to want and worth showing
/// as such rather than as an ordinary account.
pub const NOLOGIN_SHELLS: [&str; 3] = ["/sbin/nologin", "/usr/sbin/nologin", "/bin/false"];

/// Whether an account can be signed in as, and why not when it cannot.
///
/// Three separate mechanisms say "no" on a Unix system and they are not
/// interchangeable — a locked password is undone by setting one, and a nologin
/// shell is not. Collapsing them into a single "disabled" flag would make the
/// console offer the wrong remedy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginState {
    /// A usable password and a real shell.
    Enabled,
    /// The password hash is prefixed with `!` — `usermod -L`. Reversible, and
    /// the password underneath is preserved.
    Locked,
    /// There is no password hash at all, so password authentication cannot
    /// succeed. Normal for an account that only ever uses a key.
    NoPassword,
    /// The shell refuses. Not something a password change fixes.
    ///
    /// Spelled the way the shell is — `nologin`, one word — rather than the
    /// `no_login` the derive would produce, so the wire format and
    /// [`LoginState::as_str`] cannot drift apart.
    #[serde(rename = "nologin")]
    NoLogin,
    /// `/etc/shadow` could not be read, so this is genuinely unknown rather
    /// than assumed to be fine.
    Unknown,
}

impl LoginState {
    pub fn as_str(self) -> &'static str {
        match self {
            LoginState::Enabled => "enabled",
            LoginState::Locked => "locked",
            LoginState::NoPassword => "no_password",
            LoginState::NoLogin => "nologin",
            LoginState::Unknown => "unknown",
        }
    }
}

/// One account, as the node's own files describe it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// The GECOS field's first comma-separated component, which is where every
    /// tool on a Unix system has always kept a person's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_name: Option<String>,
    pub home: String,
    pub shell: String,
    /// Every group the account is in, primary first.
    #[serde(default)]
    pub groups: Vec<String>,
    /// In [`ADMIN_GROUP`], and therefore able to administer this appliance.
    pub administrator: bool,
    pub login: LoginState,
    /// Belongs to a package rather than to a person: a UID below
    /// [`FIRST_HUMAN_UID`]. `root` is one of these and is shown anyway, because
    /// it is the account an operator is most likely to be signed in as.
    pub system: bool,
    /// Days since the epoch when the password was last set, from
    /// `/etc/shadow`. Absent when there is no password or the file is
    /// unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_changed_days: Option<u64>,
}

impl LocalUser {
    /// Whether this account is one the console will let anybody change.
    ///
    /// `root` is not: it is the account the appliance recovers with, the one
    /// the installer set a password for, and the one an operator is most likely
    /// signed in as right now. Locking or removing it from a web page is the
    /// single most effective way to lose a node, and there is no reason to
    /// offer it — `passwd` on the console still works, and someone who is at
    /// the console has not lost anything.
    pub fn is_protected(&self) -> bool {
        self.uid == 0 || self.name == "root"
    }
}

/// An account to create.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewUser {
    pub name: String,
    /// Never logged, never an argument, never returned. It reaches `chpasswd`
    /// over a pipe — see [`crate::exec`].
    pub password: String,
    #[serde(default)]
    pub full_name: Option<String>,
    /// Absent means [`DEFAULT_SHELL`].
    #[serde(default)]
    pub shell: Option<String>,
    /// Put the account in [`ADMIN_GROUP`].
    #[serde(default)]
    pub administrator: bool,
    /// Give it a home directory. On by default, because an account without one
    /// cannot hold an SSH key and surprises everybody who signs in to it.
    #[serde(default = "default_true")]
    pub create_home: bool,
}

fn default_true() -> bool {
    true
}

/// A change to an existing account. Absent fields are left alone.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserPatch {
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub administrator: Option<bool>,
    /// Setting a password also unlocks the account: a password nobody can use
    /// is not what anybody means when they set one.
    #[serde(default)]
    pub password: Option<String>,
    /// Lock or unlock. Applied after any password change, so the two can be
    /// sent together and the explicit one wins.
    #[serde(default)]
    pub locked: Option<bool>,
}

/// What to do to the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    Reboot,
    PowerOff,
}

impl PowerAction {
    /// The word logind uses for a scheduled shutdown of this kind.
    pub fn as_schedule_kind(self) -> &'static str {
        match self {
            PowerAction::Reboot => "reboot",
            PowerAction::PowerOff => "poweroff",
        }
    }

    pub fn parse_schedule_kind(value: &str) -> Option<Self> {
        match value {
            "reboot" => Some(PowerAction::Reboot),
            "poweroff" | "halt" => Some(PowerAction::PowerOff),
            _ => None,
        }
    }

    /// How it reads in a sentence written for an operator.
    pub fn as_sentence(self) -> &'static str {
        match self {
            PowerAction::Reboot => "restart",
            PowerAction::PowerOff => "shut down",
        }
    }
}

/// A usable account name.
///
/// `NAME_REGEX` from `shadow-utils`: a lower-case letter or underscore, then
/// letters, digits, underscores, and dashes, optionally ending in `$` for a
/// machine account. Capped at 32 characters, which is the limit `useradd`
/// itself enforces. Nothing here may be a separator: the name becomes a
/// directory under `/home` and a field in three colon-separated files.
pub fn valid_user_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if name.len() > 32 || !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    let body: Vec<char> = chars.collect();
    let (body, _trailing) = match body.split_last() {
        Some((&'$', rest)) => (rest, true),
        _ => (&body[..], false),
    };
    body.iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
}

/// The shortest password this appliance will set.
///
/// Deliberately a length and nothing else. A composition rule ("one capital,
/// one digit, one symbol") makes passwords harder to remember without making
/// them harder to guess, and every published guideline has said so for a
/// decade. PAM's own stack still applies whatever the node's `pwquality`
/// configuration says on top of this — that is the node's policy to set, and
/// its refusal is reported verbatim.
pub const MIN_PASSWORD_LEN: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_would_become_something_else_on_disk_are_refused() {
        assert!(valid_user_name("alice"));
        assert!(valid_user_name("_svc"));
        assert!(valid_user_name("build-agent1"));
        assert!(valid_user_name("host$"));

        assert!(!valid_user_name(""));
        assert!(!valid_user_name("Alice"), "an upper-case first character");
        assert!(!valid_user_name("1st"), "a leading digit");
        assert!(!valid_user_name("has space"));
        assert!(!valid_user_name("has/slash"));
        assert!(!valid_user_name("has:colon"), "the field separator itself");
        assert!(!valid_user_name("-rf"));
        assert!(!valid_user_name("$(reboot)"));
        assert!(!valid_user_name(&"x".repeat(33)));
    }

    #[test]
    fn root_is_never_something_the_console_changes() {
        let root = LocalUser {
            name: "root".into(),
            uid: 0,
            gid: 0,
            full_name: None,
            home: "/root".into(),
            shell: "/bin/bash".into(),
            groups: vec!["root".into()],
            administrator: true,
            login: LoginState::Enabled,
            system: true,
            password_changed_days: None,
        };
        assert!(root.is_protected());

        let alice = LocalUser {
            name: "alice".into(),
            uid: 1000,
            ..root.clone()
        };
        assert!(!alice.is_protected());
    }

    #[test]
    fn a_schedule_kind_survives_the_round_trip_logind_puts_it_through() {
        for action in [PowerAction::Reboot, PowerAction::PowerOff] {
            assert_eq!(
                PowerAction::parse_schedule_kind(action.as_schedule_kind()),
                Some(action)
            );
        }
        // logind will also report a halt, which is a power-off as far as
        // anybody looking at the console is concerned.
        assert_eq!(
            PowerAction::parse_schedule_kind("halt"),
            Some(PowerAction::PowerOff)
        );
        assert_eq!(PowerAction::parse_schedule_kind("dry-reboot"), None);
    }

    #[test]
    fn a_typo_in_a_request_is_rejected_rather_than_ignored() {
        assert!(serde_json::from_str::<NewUser>(
            r#"{"name":"alice","password":"correct-horse","administratorr":true}"#
        )
        .is_err());
    }

    /// The console matches on these strings, so the wire format and the
    /// crate's own spelling have to be the same words.
    #[test]
    fn a_login_state_reads_the_same_on_the_wire_as_it_does_here() {
        for state in [
            LoginState::Enabled,
            LoginState::Locked,
            LoginState::NoPassword,
            LoginState::NoLogin,
            LoginState::Unknown,
        ] {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::Value::String(state.as_str().into()),
                "{state:?}"
            );
        }
    }
}
