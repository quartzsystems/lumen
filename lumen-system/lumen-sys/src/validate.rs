//! Pure rules over an account and the node it would live on.
//!
//! `validate` returns **every** problem it finds rather than the first, each
//! carrying a machine-readable `code` the console pins to a field and a human
//! sentence it renders verbatim — the same contract `lumen_net` and
//! `lumen_virt` established, and for the same reason: a form that reports one
//! error at a time is a form you submit five times.

use serde::Serialize;

use crate::model::{valid_user_name, LocalUser, NewUser, MIN_PASSWORD_LEN, NOLOGIN_SHELLS};

/// Why a request was rejected. Stable strings — the console matches on them
/// and tests assert on them, so they are part of the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    InvalidUsername,
    DuplicateUsername,
    ReservedUsername,
    PasswordTooShort,
    InvalidShell,
    /// Doing this to this account would take the operator's own access away.
    WouldLockYouOut,
    /// Removing an account removes its files, and that needs saying out loud.
    UnacknowledgedDestructiveOperation,
    /// A scheduled restart in the past is not a schedule.
    TimeInThePast,
    /// Further ahead than this appliance will hold a schedule for.
    TimeTooFarAhead,
}

impl ValidationCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ValidationCode::InvalidUsername => "invalid_username",
            ValidationCode::DuplicateUsername => "duplicate_username",
            ValidationCode::ReservedUsername => "reserved_username",
            ValidationCode::PasswordTooShort => "password_too_short",
            ValidationCode::InvalidShell => "invalid_shell",
            ValidationCode::WouldLockYouOut => "would_lock_you_out",
            ValidationCode::UnacknowledgedDestructiveOperation => {
                "unacknowledged_destructive_operation"
            }
            ValidationCode::TimeInThePast => "time_in_the_past",
            ValidationCode::TimeTooFarAhead => "time_too_far_ahead",
        }
    }
}

/// One rejection, tied to the field it belongs to so a dialog can render it
/// against the offending input rather than in a banner at the top.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationError {
    pub code: ValidationCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
}

impl ValidationError {
    pub fn new(code: ValidationCode, field: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            code,
            user: None,
            field: field.map(str::to_string),
            message: message.into(),
        }
    }

    pub fn about(mut self, user: &str) -> Self {
        self.user = Some(user.to_string());
        self
    }
}

/// The acknowledgement a destructive operation demands.
///
/// Named after what it means rather than after what it guards, so a caller
/// cannot set it without reading it — the same shape `lumen_virt` uses, and
/// built by hand in the control plane's handler for the same reason.
#[derive(Debug, Clone, Copy, Default)]
pub struct Acknowledgements {
    pub may_lose_data: bool,
}

/// What the node has, for the checks that compare a request against it.
#[derive(Debug, Clone, Default)]
pub struct NodeFacts {
    /// Every account already on the node.
    pub existing: Vec<LocalUser>,
    /// The shells `/etc/shells` lists, plus the nologin ones. Empty means the
    /// file could not be read, and then the check is **skipped** rather than
    /// failing every account — the same rule `lumen_virt` applies to a
    /// subsystem it cannot see.
    pub shells: Vec<String>,
    /// Who is asking. An operator may not take their own access away.
    pub acting_as: Option<String>,
}

/// Names this appliance will not create, whatever the node currently has.
///
/// `root` because it is the recovery account; the rest because they are
/// package-owned on every EL system and an account that shadows one of them
/// breaks something quietly rather than loudly.
const RESERVED: [&str; 8] = [
    "root",
    "daemon",
    "bin",
    "sys",
    "nobody",
    "systemd-network",
    "dbus",
    "qemu",
];

/// Check an account that does not exist yet.
pub fn validate_new(request: &NewUser, facts: &NodeFacts) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let name = request.name.trim();

    if !valid_user_name(name) {
        errors.push(ValidationError::new(
            ValidationCode::InvalidUsername,
            Some("name"),
            format!(
                "\"{name}\" is not a usable account name. Start with a lower-case letter or an \
                 underscore, then use lower-case letters, digits, underscores, and dashes — up to \
                 32 characters."
            ),
        ));
    } else if RESERVED.contains(&name) {
        errors.push(ValidationError::new(
            ValidationCode::ReservedUsername,
            Some("name"),
            format!("\"{name}\" belongs to the operating system and cannot be created here."),
        ));
    } else if facts
        .existing
        .iter()
        .any(|user| user.name.eq_ignore_ascii_case(name))
    {
        errors.push(ValidationError::new(
            ValidationCode::DuplicateUsername,
            Some("name"),
            format!("This node already has an account called \"{name}\"."),
        ));
    }

    errors.extend(check_password(&request.password));
    if let Some(shell) = request.shell.as_deref() {
        errors.extend(check_shell(shell, facts));
    }

    errors.into_iter().map(|error| error.about(name)).collect()
}

/// Check a change to an account that already exists.
///
/// Takes the account rather than its name, because every rule here is about
/// what the account *is*: whether it is root, whether it is the one the
/// operator is signed in as, whether it is the last administrator.
pub fn validate_patch(
    user: &LocalUser,
    password: Option<&str>,
    shell: Option<&str>,
    locking: bool,
    dropping_admin: bool,
    facts: &NodeFacts,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    if user.is_protected() {
        errors.push(ValidationError::new(
            ValidationCode::ReservedUsername,
            None,
            format!(
                "\"{}\" is this node's recovery account and is not changed from the console. Use \
                 passwd while signed in to the node itself.",
                user.name
            ),
        ));
    }

    if let Some(password) = password {
        errors.extend(check_password(password));
    }
    if let Some(shell) = shell {
        errors.extend(check_shell(shell, facts));
    }

    // The rule this appliance keeps everywhere: nothing the console offers may
    // cut the operator off from the console. Networking has a checkpoint that
    // rolls itself back for exactly this reason; here it is simply refused,
    // because there is nothing to roll back to once you cannot sign in.
    let is_self = facts
        .acting_as
        .as_deref()
        .is_some_and(|who| who == user.name);
    if is_self && (locking || dropping_admin) {
        errors.push(ValidationError::new(
            ValidationCode::WouldLockYouOut,
            None,
            format!(
                "You are signed in as \"{}\". {} would take your own access to this console away.",
                user.name,
                if locking {
                    "Locking it"
                } else {
                    "Removing its administrator rights"
                }
            ),
        ));
    }

    // The last administrator is the same failure a step removed: an appliance
    // whose every administrator is locked out has to be recovered at the
    // keyboard.
    if (locking || dropping_admin) && user.administrator && sole_administrator(user, facts) {
        errors.push(ValidationError::new(
            ValidationCode::WouldLockYouOut,
            None,
            format!(
                "\"{}\" is the only account that can administer this appliance. Give another \
                 account administrator rights first.",
                user.name
            ),
        ));
    }

    errors
        .into_iter()
        .map(|error| error.about(&user.name))
        .collect()
}

/// Check removing an account.
pub fn validate_delete(
    user: &LocalUser,
    remove_home: bool,
    acknowledged: bool,
    facts: &NodeFacts,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    if user.is_protected() {
        errors.push(ValidationError::new(
            ValidationCode::ReservedUsername,
            None,
            format!(
                "\"{}\" cannot be removed — it is this node's recovery account.",
                user.name
            ),
        ));
    }
    if facts
        .acting_as
        .as_deref()
        .is_some_and(|who| who == user.name)
    {
        errors.push(ValidationError::new(
            ValidationCode::WouldLockYouOut,
            None,
            format!(
                "You are signed in as \"{}\" and cannot remove the account you are using.",
                user.name
            ),
        ));
    }
    if user.administrator && sole_administrator(user, facts) {
        errors.push(ValidationError::new(
            ValidationCode::WouldLockYouOut,
            None,
            format!(
                "\"{}\" is the only account that can administer this appliance.",
                user.name
            ),
        ));
    }
    // Removing an account is one decision; removing everything it owns is
    // another, and it is the one with no undo.
    if remove_home && !acknowledged {
        errors.push(ValidationError::new(
            ValidationCode::UnacknowledgedDestructiveOperation,
            None,
            format!(
                "Removing \"{}\" with its home directory destroys everything in {}. Confirm that \
                 you understand this may lose data.",
                user.name, user.home
            ),
        ));
    }

    errors
        .into_iter()
        .map(|error| error.about(&user.name))
        .collect()
}

/// Check a scheduled restart or shutdown.
///
/// `at` and `now` are both seconds since the epoch.
pub fn validate_schedule(at: u64, now: u64, horizon_secs: u64) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    if at <= now {
        errors.push(ValidationError::new(
            ValidationCode::TimeInThePast,
            Some("at"),
            "That moment has already passed. Choose a time in the future.",
        ));
    } else if at - now > horizon_secs {
        errors.push(ValidationError::new(
            ValidationCode::TimeTooFarAhead,
            Some("at"),
            format!(
                "A restart can be scheduled up to {} days ahead. Further out than that is a \
                 maintenance window, not a countdown.",
                horizon_secs / 86_400
            ),
        ));
    }
    errors
}

fn check_password(password: &str) -> Vec<ValidationError> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return vec![ValidationError::new(
            ValidationCode::PasswordTooShort,
            Some("password"),
            format!("A password must be at least {MIN_PASSWORD_LEN} characters."),
        )];
    }
    Vec::new()
}

fn check_shell(shell: &str, facts: &NodeFacts) -> Vec<ValidationError> {
    // An unreadable /etc/shells skips the check rather than rejecting every
    // account: refusing to create one because a list could not be read would
    // turn one broken thing into two.
    if facts.shells.is_empty() {
        return Vec::new();
    }
    let known = facts.shells.iter().any(|s| s == shell) || NOLOGIN_SHELLS.contains(&shell);
    if known {
        return Vec::new();
    }
    vec![ValidationError::new(
        ValidationCode::InvalidShell,
        Some("shell"),
        format!(
            "\"{shell}\" is not a login shell on this node. It has: {}.",
            facts.shells.join(", ")
        ),
    )]
}

/// Whether this account is the only one that can administer the appliance.
fn sole_administrator(user: &LocalUser, facts: &NodeFacts) -> bool {
    !facts
        .existing
        .iter()
        .any(|other| other.administrator && other.name != user.name && other.uid != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LoginState;

    fn user(name: &str, uid: u32, administrator: bool) -> LocalUser {
        LocalUser {
            name: name.into(),
            uid,
            gid: uid,
            full_name: None,
            home: format!("/home/{name}"),
            shell: "/bin/bash".into(),
            groups: vec![name.into()],
            administrator,
            login: LoginState::Enabled,
            system: uid < 1000,
            password_changed_days: None,
        }
    }

    fn facts() -> NodeFacts {
        NodeFacts {
            existing: vec![user("root", 0, true), user("alice", 1000, true)],
            shells: vec!["/bin/bash".into(), "/bin/sh".into()],
            acting_as: Some("alice".into()),
        }
    }

    fn new_user(name: &str) -> NewUser {
        NewUser {
            name: name.into(),
            password: "correct-horse".into(),
            full_name: None,
            shell: None,
            administrator: false,
            create_home: true,
        }
    }

    fn codes(errors: &[ValidationError]) -> Vec<ValidationCode> {
        errors.iter().map(|e| e.code).collect()
    }

    #[test]
    fn a_reasonable_account_is_accepted() {
        assert!(validate_new(&new_user("bob"), &facts()).is_empty());
    }

    #[test]
    fn every_problem_is_reported_not_just_the_first() {
        let request = NewUser {
            name: "Bad Name".into(),
            password: "x".into(),
            shell: Some("/bin/nope".into()),
            ..new_user("x")
        };
        let errors = validate_new(&request, &facts());
        assert!(errors.len() >= 3, "{errors:#?}");
        assert!(codes(&errors).contains(&ValidationCode::InvalidUsername));
        assert!(codes(&errors).contains(&ValidationCode::PasswordTooShort));
        assert!(codes(&errors).contains(&ValidationCode::InvalidShell));
        // Each one names the field the console pins it to.
        assert!(errors.iter().all(|e| e.field.is_some()));
    }

    #[test]
    fn an_account_that_already_exists_is_refused_by_name() {
        let errors = validate_new(&new_user("alice"), &facts());
        assert_eq!(codes(&errors), vec![ValidationCode::DuplicateUsername]);
        assert_eq!(errors[0].user.as_deref(), Some("alice"));
    }

    #[test]
    fn names_the_operating_system_owns_are_refused() {
        for name in ["root", "daemon", "nobody"] {
            let errors = validate_new(&new_user(name), &facts());
            assert_eq!(
                codes(&errors),
                vec![ValidationCode::ReservedUsername],
                "{name}"
            );
        }
    }

    /// The rule the whole appliance is built around: nothing the console
    /// offers may take the console away from the operator using it.
    #[test]
    fn you_cannot_lock_yourself_out() {
        let facts = facts();
        let alice = user("alice", 1000, true);

        let locking = validate_patch(&alice, None, None, true, false, &facts);
        assert!(codes(&locking).contains(&ValidationCode::WouldLockYouOut));

        let demoting = validate_patch(&alice, None, None, false, true, &facts);
        assert!(codes(&demoting).contains(&ValidationCode::WouldLockYouOut));

        // Removing the account you are signed in as is the same mistake.
        let removing = validate_delete(&alice, false, false, &facts);
        assert!(codes(&removing).contains(&ValidationCode::WouldLockYouOut));

        // Changing your own password is not — that is the ordinary case.
        assert!(
            validate_patch(&alice, Some("a-new-password"), None, false, false, &facts).is_empty()
        );
    }

    /// One step removed from the same failure: locking out somebody else's
    /// only administrator leaves the node recoverable only at the keyboard.
    #[test]
    fn the_last_administrator_cannot_be_locked_out_either() {
        let facts = NodeFacts {
            existing: vec![user("root", 0, true), user("bob", 1001, true)],
            shells: vec!["/bin/bash".into()],
            acting_as: Some("someone-else".into()),
        };
        let bob = user("bob", 1001, true);
        assert!(
            codes(&validate_patch(&bob, None, None, true, false, &facts))
                .contains(&ValidationCode::WouldLockYouOut)
        );

        // With a second administrator, it is allowed.
        let mut two = facts.clone();
        two.existing.push(user("carol", 1002, true));
        assert!(validate_patch(&bob, None, None, true, false, &two).is_empty());
    }

    #[test]
    fn root_is_not_changed_from_a_web_page() {
        let facts = facts();
        let root = user("root", 0, true);
        assert!(codes(&validate_patch(
            &root,
            Some("hunter2000"),
            None,
            false,
            false,
            &facts
        ))
        .contains(&ValidationCode::ReservedUsername));
        assert!(codes(&validate_delete(&root, false, true, &facts))
            .contains(&ValidationCode::ReservedUsername));
    }

    #[test]
    fn destroying_a_home_directory_needs_saying_out_loud() {
        let facts = NodeFacts {
            acting_as: None,
            ..facts()
        };
        let bob = user("bob", 1001, false);
        let unacknowledged = validate_delete(&bob, true, false, &facts);
        assert!(
            codes(&unacknowledged).contains(&ValidationCode::UnacknowledgedDestructiveOperation)
        );
        assert!(unacknowledged[0].message.contains("/home/bob"));

        assert!(validate_delete(&bob, true, true, &facts).is_empty());
        // Keeping the home directory needs no acknowledgement at all.
        assert!(validate_delete(&bob, false, false, &facts).is_empty());
    }

    /// An unreadable list is not an empty list — the same rule the compute
    /// domain applies to a subsystem it cannot see.
    #[test]
    fn an_unreadable_shell_list_skips_its_check_rather_than_failing_everything() {
        let blind = NodeFacts {
            shells: Vec::new(),
            ..facts()
        };
        let request = NewUser {
            shell: Some("/opt/weird/shell".into()),
            ..new_user("bob")
        };
        assert!(validate_new(&request, &blind).is_empty());
        // And with a list, the same request is refused with the list in it.
        let errors = validate_new(&request, &facts());
        assert_eq!(codes(&errors), vec![ValidationCode::InvalidShell]);
        assert!(errors[0].message.contains("/bin/bash"));
    }

    #[test]
    fn a_schedule_has_to_be_in_the_future_and_not_absurdly_far_into_it() {
        const WEEK: u64 = 7 * 86_400;
        assert!(validate_schedule(2_000, 1_000, WEEK).is_empty());
        assert_eq!(
            codes(&validate_schedule(500, 1_000, WEEK)),
            vec![ValidationCode::TimeInThePast]
        );
        assert_eq!(
            codes(&validate_schedule(1_000, 1_000, WEEK)),
            vec![ValidationCode::TimeInThePast],
            "now is not the future"
        );
        assert_eq!(
            codes(&validate_schedule(1_000 + WEEK + 1, 1_000, WEEK)),
            vec![ValidationCode::TimeTooFarAhead]
        );
    }
}
