//! The system domain's one entry point.
//!
//! Everything the control plane's HTTP handlers do goes through here: listing
//! accounts, creating and changing them, and restarting the node. The handlers
//! deserialize, call one method, serialize — no `/etc/passwd`, no `useradd`,
//! and no validation above this line.
//!
//! ## Nothing is cached
//!
//! Every read goes back to the node's own files. There is no in-memory account
//! list to fall out of step with `useradd` at the keyboard, and no invalidation
//! to get wrong; the files are small and the page that reads them is not on a
//! hot path. This is the same reasoning `lumen_virt` uses for keeping the
//! domain document as the database.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::backend::{PowerBackend, ScheduledPower};
use crate::error::{Result, SysError};
use crate::exec::{Exec, Request};
use crate::model::{LocalUser, NewUser, PowerAction, UserPatch, ADMIN_GROUP, DEFAULT_SHELL};
use crate::state::{self, AccountFiles};
use crate::validate::{
    validate_delete, validate_new, validate_patch, validate_schedule, Acknowledgements, NodeFacts,
    ValidationError,
};

/// The furthest ahead a restart may be scheduled. Past a week it is a
/// maintenance window somebody should put in a calendar, not a countdown a
/// console should be holding.
pub const SCHEDULE_HORIZON_SECS: u64 = 7 * 86_400;

/// Where the account tools live. Absolute, because `execve` does not consult
/// `PATH` and a daemon should not be picking programs out of one anyway.
const USERADD: &str = "/usr/sbin/useradd";
const USERMOD: &str = "/usr/sbin/usermod";
const USERDEL: &str = "/usr/sbin/userdel";
const CHPASSWD: &str = "/usr/sbin/chpasswd";

/// Whether an operation is available on an account, and why not when it is
/// not — so a disabled control explains itself rather than being silently
/// grey. The same shape `lumen_virt::service::Action` established.
#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub requires_acknowledgement: bool,
}

impl Action {
    fn yes() -> Self {
        Self {
            allowed: true,
            reason: None,
            requires_acknowledgement: false,
        }
    }

    fn no(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            reason: Some(reason.into()),
            requires_acknowledgement: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UserActions {
    pub edit: Action,
    pub lock: Action,
    pub unlock: Action,
    pub delete: Action,
}

/// One row of the Authentication table, and everything a dialog opened from it
/// needs — so neither makes a second request.
#[derive(Debug, Clone, Serialize)]
pub struct UserView {
    #[serde(flatten)]
    pub user: LocalUser,
    /// This is the account the request was made by.
    pub is_you: bool,
    pub actions: UserActions,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsersResponse {
    pub node: String,
    pub users: Vec<UserView>,
    /// The login shells this node offers, for the create dialog's picker. Empty
    /// when `/etc/shells` could not be read, and then the console offers a free
    /// text field rather than an empty drop-down.
    pub shells: Vec<String>,
    /// The group that grants administrative rights on this appliance, named
    /// rather than assumed by the console.
    pub admin_group: String,
}

/// What the node's power state is, and what may be done to it.
///
/// Deserializable as well as serializable, unlike the account views beside it,
/// and for one reason: the environment's Maintenance page reads every member's
/// power state, and the other members' answers arrive here over the peer
/// surface as exactly this shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerView {
    pub node: String,
    /// Seconds since the node booted. Absent when `/proc/uptime` is
    /// unreadable, which is not something to invent a number for.
    pub uptime_secs: Option<u64>,
    /// Seconds since the epoch, so the console can render a countdown against
    /// its own clock rather than trusting a duration computed here.
    pub now: u64,
    pub scheduled: Option<ScheduledView>,
    /// The furthest ahead this appliance will accept a schedule.
    pub horizon_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledView {
    pub action: PowerAction,
    pub at: u64,
}

impl From<ScheduledPower> for ScheduledView {
    fn from(value: ScheduledPower) -> Self {
        Self {
            action: value.action,
            at: value.at,
        }
    }
}

/// The system domain.
pub struct SysService {
    power: Arc<dyn PowerBackend>,
    exec: Arc<dyn Exec>,
    files: AccountFiles,
    node: String,
    /// One account change at a time. `useradd` takes `/etc/passwd`'s lock
    /// itself, but two requests racing would still read the same account list
    /// and both decide there is another administrator left.
    gate: Mutex<()>,
}

impl SysService {
    pub fn new(power: Arc<dyn PowerBackend>, exec: Arc<dyn Exec>) -> Self {
        Self {
            power,
            exec,
            files: AccountFiles::default(),
            node: state::hostname(),
            gate: Mutex::new(()),
        }
    }

    /// Read the account database from somewhere else — a test's own directory
    /// rather than the machine running it.
    pub fn with_account_files(mut self, files: AccountFiles) -> Self {
        self.files = files;
        self
    }

    pub fn with_node(mut self, node: impl Into<String>) -> Self {
        self.node = node.into();
        self
    }

    // --- accounts ---------------------------------------------------------

    /// Everything the node's own files say, plus what may be done to each
    /// account by the operator asking.
    pub async fn users(&self, acting_as: Option<&str>) -> Result<UsersResponse> {
        let accounts = state::read(&self.files).await;
        let facts = NodeFacts {
            existing: accounts.users.clone(),
            shells: accounts.shells.clone(),
            acting_as: acting_as.map(str::to_string),
        };

        let users = accounts
            .users
            .iter()
            .map(|user| self.view_of(user, &facts))
            .collect();

        Ok(UsersResponse {
            node: self.node.clone(),
            users,
            shells: accounts.shells,
            admin_group: ADMIN_GROUP.to_string(),
        })
    }

    /// One account by name.
    pub async fn user(&self, name: &str, acting_as: Option<&str>) -> Result<UserView> {
        self.users(acting_as)
            .await?
            .users
            .into_iter()
            .find(|view| view.user.name == name)
            .ok_or_else(|| {
                SysError::NotFound(format!("This node has no account called \"{name}\"."))
            })
    }

    /// Create it, then set its password over a pipe.
    ///
    /// Two commands rather than one because `useradd --password` takes a
    /// *hash*, and hashing here would mean choosing an algorithm and a cost
    /// that the node's own `/etc/login.defs` already chose. `chpasswd` uses the
    /// node's settings and applies its `pwquality` stack, whose refusal is
    /// reported verbatim.
    pub async fn create_user(&self, request: NewUser) -> Result<UserView> {
        let _guard = self.gate.lock().await;

        let accounts = state::read(&self.files).await;
        let facts = NodeFacts {
            existing: accounts.users,
            shells: accounts.shells,
            acting_as: None,
        };
        let errors = validate_new(&request, &facts);
        if !errors.is_empty() {
            return Err(SysError::Invalid(errors));
        }

        let name = request.name.trim().to_string();
        let shell = request
            .shell
            .clone()
            .unwrap_or_else(|| DEFAULT_SHELL.into());

        let mut args: Vec<String> = Vec::new();
        // A home directory or explicitly none — never whatever the node's
        // CREATE_HOME default happens to be, because "did this account get a
        // home" is a question the operator answered on the form.
        args.push(if request.create_home { "-m" } else { "-M" }.into());
        args.extend(["-s".to_string(), shell]);
        if let Some(full_name) = request
            .full_name
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            args.extend(["-c".to_string(), full_name.to_string()]);
        }
        if request.administrator {
            args.extend(["-G".to_string(), ADMIN_GROUP.to_string()]);
        }
        args.push(name.clone());

        let outcome = self
            .exec
            .run(Request::new(format!("create the account \"{name}\""), USERADD).args(args))
            .await
            .map_err(SysError::Backend)?;
        if !outcome.ok() {
            return Err(SysError::Conflict(format!(
                "Could not create \"{name}\": {}",
                outcome.failure()
            )));
        }

        // From here the account exists, so a failure has to undo itself:
        // leaving an account with no password is leaving one anybody can be
        // asked to guess at.
        if let Err(err) = self.set_password(&name, &request.password).await {
            let _ = self
                .exec
                .run(
                    Request::new(format!("remove the half-made account \"{name}\""), USERDEL)
                        .args(["-r", &name]),
                )
                .await;
            return Err(err);
        }

        tracing::info!(user = %name, administrator = request.administrator, "account created");
        self.user(&name, None).await
    }

    /// Change one. Absent fields are left alone.
    pub async fn update_user(
        &self,
        name: &str,
        patch: UserPatch,
        acting_as: Option<&str>,
    ) -> Result<UserView> {
        let _guard = self.gate.lock().await;

        let accounts = state::read(&self.files).await;
        let user = accounts
            .users
            .iter()
            .find(|user| user.name == name)
            .cloned()
            .ok_or_else(|| {
                SysError::NotFound(format!("This node has no account called \"{name}\"."))
            })?;
        let facts = NodeFacts {
            existing: accounts.users,
            shells: accounts.shells,
            acting_as: acting_as.map(str::to_string),
        };

        let dropping_admin = patch.administrator == Some(false) && user.administrator;
        let errors = validate_patch(
            &user,
            patch.password.as_deref(),
            patch.shell.as_deref(),
            patch.locked == Some(true),
            dropping_admin,
            &facts,
        );
        if !errors.is_empty() {
            return Err(SysError::Invalid(errors));
        }

        // One usermod for everything it can do at once, so an account is never
        // half-changed by a second command failing.
        let mut args: Vec<String> = Vec::new();
        if let Some(full_name) = patch.full_name.as_deref() {
            args.extend(["-c".to_string(), full_name.trim().to_string()]);
        }
        if let Some(shell) = patch.shell.as_deref() {
            args.extend(["-s".to_string(), shell.to_string()]);
        }
        match patch.administrator {
            // -a -G adds without disturbing the groups it is already in;
            // plain -G would replace the whole list.
            Some(true) if !user.administrator => {
                args.extend(["-a".to_string(), "-G".to_string(), ADMIN_GROUP.to_string()])
            }
            Some(false) if user.administrator => {
                let kept: Vec<&str> = user
                    .groups
                    .iter()
                    .map(String::as_str)
                    .filter(|group| *group != ADMIN_GROUP && *group != user.name)
                    .collect();
                args.extend(["-G".to_string(), kept.join(",")]);
            }
            _ => {}
        }
        if !args.is_empty() {
            args.push(name.to_string());
            let outcome = self
                .exec
                .run(Request::new(format!("change the account \"{name}\""), USERMOD).args(args))
                .await
                .map_err(SysError::Backend)?;
            if !outcome.ok() {
                return Err(SysError::Conflict(format!(
                    "Could not change \"{name}\": {}",
                    outcome.failure()
                )));
            }
        }

        // The password goes on its own, over a pipe, and never as an argument.
        if let Some(password) = patch.password.as_deref() {
            self.set_password(name, password).await?;
        }

        // Locking last, so "set a password and lock it" ends locked. Setting a
        // password unlocks the account as a side effect of there being a hash
        // again, which is why the explicit instruction has to win.
        if let Some(locked) = patch.locked {
            let flag = if locked { "-L" } else { "-U" };
            let outcome = self
                .exec
                .run(
                    Request::new(
                        format!(
                            "{} the account \"{name}\"",
                            if locked { "lock" } else { "unlock" }
                        ),
                        USERMOD,
                    )
                    .args([flag, name]),
                )
                .await
                .map_err(SysError::Backend)?;
            if !outcome.ok() {
                return Err(SysError::Conflict(format!(
                    "Could not {} \"{name}\": {}",
                    if locked { "lock" } else { "unlock" },
                    outcome.failure()
                )));
            }
        }

        tracing::info!(user = %name, "account changed");
        self.user(name, acting_as).await
    }

    /// Remove it. Its home directory stays unless asked for, and asking needs
    /// the acknowledgement.
    pub async fn delete_user(
        &self,
        name: &str,
        remove_home: bool,
        ack: Acknowledgements,
        acting_as: Option<&str>,
    ) -> Result<DeleteUserResponse> {
        let _guard = self.gate.lock().await;

        let accounts = state::read(&self.files).await;
        let user = accounts
            .users
            .iter()
            .find(|user| user.name == name)
            .cloned()
            .ok_or_else(|| {
                SysError::NotFound(format!("This node has no account called \"{name}\"."))
            })?;
        let facts = NodeFacts {
            existing: accounts.users,
            shells: accounts.shells,
            acting_as: acting_as.map(str::to_string),
        };

        let errors = validate_delete(&user, remove_home, ack.may_lose_data, &facts);
        if !errors.is_empty() {
            return Err(SysError::Invalid(errors));
        }

        let mut args: Vec<String> = Vec::new();
        if remove_home {
            args.push("-r".into());
        }
        args.push(name.to_string());

        let outcome = self
            .exec
            .run(Request::new(format!("remove the account \"{name}\""), USERDEL).args(args))
            .await
            .map_err(SysError::Backend)?;
        if !outcome.ok() {
            return Err(SysError::Conflict(format!(
                "Could not remove \"{name}\": {}",
                outcome.failure()
            )));
        }

        tracing::info!(user = %name, removed_home = remove_home, "account removed");
        Ok(DeleteUserResponse {
            name: name.to_string(),
            // An operator who did not ask for the files to go is told where
            // they still are — the same contract the machine delete keeps.
            removed_home: remove_home.then(|| user.home.clone()),
            kept_home: (!remove_home).then(|| user.home.clone()),
        })
    }

    /// `chpasswd` over a pipe: `name:password` on standard input, so the
    /// secret is never an argument and never reaches a log or `ps`.
    async fn set_password(&self, name: &str, password: &str) -> Result<()> {
        let outcome = self
            .exec
            .run(
                Request::new(format!("set the password for \"{name}\""), CHPASSWD)
                    .stdin(format!("{name}:{password}\n")),
            )
            .await
            .map_err(SysError::Backend)?;
        if !outcome.ok() {
            // pam_pwquality's refusal is the useful part and it is the node's
            // policy talking, so it is passed through rather than summarised.
            return Err(SysError::Conflict(format!(
                "Could not set the password for \"{name}\": {}",
                outcome.failure()
            )));
        }
        Ok(())
    }

    fn view_of(&self, user: &LocalUser, facts: &NodeFacts) -> UserView {
        let is_you = facts
            .acting_as
            .as_deref()
            .is_some_and(|who| who == user.name);

        // Every control's answer comes from the same validator the request
        // will run through, so the console and the node can never disagree
        // about what is possible.
        let first = |errors: Vec<ValidationError>| errors.into_iter().next().map(|e| e.message);
        let action = |errors: Vec<ValidationError>| match first(errors) {
            Some(reason) => Action::no(reason),
            None => Action::yes(),
        };

        let locked = user.login == crate::model::LoginState::Locked;
        UserActions {
            edit: action(validate_patch(user, None, None, false, false, facts)),
            lock: if locked {
                Action::no(format!("\"{}\" is already locked.", user.name))
            } else {
                action(validate_patch(user, None, None, true, false, facts))
            },
            unlock: if locked {
                action(validate_patch(user, None, None, false, false, facts))
            } else {
                Action::no(format!("\"{}\" is not locked.", user.name))
            },
            delete: {
                let mut delete = action(validate_delete(user, false, false, facts));
                // Removing the account is offered; removing its files is what
                // needs saying out loud, and the dialog collects that.
                delete.requires_acknowledgement = delete.allowed;
                delete
            },
        }
        .into_view(user.clone(), is_you)
    }

    // --- power ------------------------------------------------------------

    /// What the node's power state is, and what is already scheduled.
    pub async fn power(&self) -> Result<PowerView> {
        Ok(PowerView {
            node: self.node.clone(),
            uptime_secs: state::uptime_secs(),
            now: now_unix(),
            scheduled: self.power.scheduled().await?.map(Into::into),
            horizon_secs: SCHEDULE_HORIZON_SECS,
        })
    }

    /// Restart or shut down, now.
    ///
    /// There is no acknowledgement field here and that is deliberate: unlike
    /// stopping a machine, this is not something the console can undo *or*
    /// report the result of — the answer is the connection dropping. The
    /// dialog in front of it is where the confirmation belongs, and the
    /// backend refusing an unconfirmed one would be a second, weaker copy of
    /// the same guard.
    pub async fn power_now(&self, action: PowerAction) -> Result<()> {
        tracing::warn!(action = ?action, "the node was asked to {} now", action.as_sentence());
        self.power.power(action).await
    }

    /// Restart or shut down at a moment in the future.
    pub async fn power_at(&self, action: PowerAction, at: u64) -> Result<PowerView> {
        let errors = validate_schedule(at, now_unix(), SCHEDULE_HORIZON_SECS);
        if !errors.is_empty() {
            return Err(SysError::Invalid(errors));
        }
        self.power.schedule(action, at).await?;
        tracing::info!(action = ?action, at, "scheduled");
        self.power().await
    }

    /// Call off whatever is scheduled.
    pub async fn cancel_power(&self) -> Result<PowerView> {
        if !self.power.cancel().await? {
            return Err(SysError::Conflict(
                "Nothing is scheduled on this node.".into(),
            ));
        }
        tracing::info!("a scheduled restart was cancelled");
        self.power().await
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DeleteUserResponse {
    pub name: String,
    /// The directory that was destroyed, when one was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_home: Option<String>,
    /// The directory left behind, which is the default — an operator who did
    /// not ask for the files to go needs to be told where they still are.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_home: Option<String>,
}

impl UserActions {
    fn into_view(self, user: LocalUser, is_you: bool) -> UserView {
        UserView {
            user,
            is_you,
            actions: self,
        }
    }
}

/// Seconds since the epoch.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockPower;
    use crate::exec::MockExec;
    use crate::model::LoginState;

    struct Harness {
        service: SysService,
        exec: Arc<MockExec>,
        power: Arc<MockPower>,
        root: std::path::PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A node with root, one administrator, and one ordinary account.
    ///
    /// The account database is four files in a directory of this test's own,
    /// so nothing here reads or writes the accounts on the machine running it.
    fn harness(tag: &str) -> Harness {
        let root = std::env::temp_dir().join(format!(
            "lumen-sys-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("passwd"),
            "root:x:0:0:root:/root:/bin/bash\n\
             bin:x:1:1:bin:/bin:/sbin/nologin\n\
             alice:x:1000:1000:Alice:/home/alice:/bin/bash\n\
             bob:x:1001:1001::/home/bob:/bin/bash\n",
        )
        .unwrap();
        std::fs::write(
            root.join("group"),
            "root:x:0:\nbin:x:1:\nwheel:x:10:alice\n\
             alice:x:1000:\nbob:x:1001:\n",
        )
        .unwrap();
        std::fs::write(
            root.join("shadow"),
            "root:$6$a$b:20000:0:99999:7:::\n\
             alice:$6$c$d:20100:0:99999:7:::\n\
             bob:!$6$e$f:20050:0:99999:7:::\n",
        )
        .unwrap();
        std::fs::write(root.join("shells"), "/bin/sh\n/bin/bash\n").unwrap();

        let exec = Arc::new(MockExec::new());
        let power = Arc::new(MockPower::appliance());
        let service = SysService::new(power.clone(), exec.clone())
            .with_account_files(AccountFiles::under(&root))
            .with_node("lumen");

        Harness {
            service,
            exec,
            power,
            root,
        }
    }

    fn new_user(name: &str) -> NewUser {
        NewUser {
            name: name.into(),
            password: "correct-horse".into(),
            full_name: Some("Carol Danvers".into()),
            shell: None,
            administrator: false,
            create_home: true,
        }
    }

    #[tokio::test]
    async fn the_node_lists_what_its_own_files_say() {
        let h = harness("list");
        let response = h.service.users(Some("alice")).await.unwrap();

        assert_eq!(response.node, "lumen");
        assert_eq!(response.admin_group, "wheel");
        assert_eq!(response.shells, ["/bin/bash", "/bin/sh"]);

        let alice = response
            .users
            .iter()
            .find(|u| u.user.name == "alice")
            .unwrap();
        assert!(alice.user.administrator);
        assert!(alice.is_you);
        assert_eq!(alice.user.login, LoginState::Enabled);

        let bob = response
            .users
            .iter()
            .find(|u| u.user.name == "bob")
            .unwrap();
        assert_eq!(bob.user.login, LoginState::Locked);
        assert!(!bob.is_you);
        // Already locked, so the control that would lock it explains itself.
        assert!(!bob.actions.lock.allowed);
        assert!(bob.actions.unlock.allowed);
    }

    /// The whole point of the exec seam: the password reaches `chpasswd` over
    /// a pipe and appears in no argument list anywhere.
    #[tokio::test]
    async fn creating_an_account_never_puts_the_password_in_an_argument() {
        let h = harness("create");
        h.service.create_user(new_user("carol")).await.ok();

        let ran = h.exec.ran().await;
        assert!(
            ran.iter().all(|r| !r.display().contains("correct-horse")),
            "the password must never be an argument: {:#?}",
            ran.iter().map(|r| r.display()).collect::<Vec<_>>()
        );
        assert_eq!(
            h.exec.last_stdin().await.as_deref(),
            Some("carol:correct-horse\n")
        );
    }

    #[tokio::test]
    async fn creating_an_account_runs_the_command_an_operator_would_have() {
        let h = harness("create-args");
        let request = NewUser {
            administrator: true,
            shell: Some("/bin/sh".into()),
            ..new_user("carol")
        };
        h.service.create_user(request).await.ok();

        assert!(
            h.exec
                .ran_with(
                    "/usr/sbin/useradd",
                    &[
                        "-m",
                        "-s",
                        "/bin/sh",
                        "-c",
                        "Carol Danvers",
                        "-G",
                        "wheel",
                        "carol"
                    ]
                )
                .await,
            "{:#?}",
            h.exec
                .ran()
                .await
                .iter()
                .map(|r| r.display())
                .collect::<Vec<_>>()
        );
    }

    /// An account with no password is one anybody can be invited to guess at,
    /// so a failure half-way through undoes itself rather than leaving one.
    #[tokio::test]
    async fn an_account_whose_password_will_not_set_is_removed_again() {
        let h = harness("rollback");
        // useradd succeeds; chpasswd does not.
        h.exec.fail_next(0, "").await;
        h.exec
            .fail_next(1, "chpasswd: (user carol) pam_chauthtok() failed: Authentication token manipulation error")
            .await;

        let err = h.service.create_user(new_user("carol")).await.unwrap_err();
        assert!(matches!(err, SysError::Conflict(_)), "{err:?}");
        // The node's own words, not a summary of them.
        assert!(err.to_string().contains("pam_chauthtok"), "{err}");
        assert!(
            h.exec.ran_with("/usr/sbin/userdel", &["-r", "carol"]).await,
            "the half-made account must be removed again"
        );
    }

    #[tokio::test]
    async fn a_rejected_account_never_reaches_the_node_at_all() {
        let h = harness("rejected");
        let err = h
            .service
            .create_user(NewUser {
                password: "x".into(),
                ..new_user("alice")
            })
            .await
            .unwrap_err();

        let SysError::Invalid(errors) = err else {
            panic!("expected a validation failure");
        };
        assert!(errors.len() >= 2, "{errors:#?}");
        assert!(h.exec.ran().await.is_empty(), "nothing may have been run");
    }

    #[tokio::test]
    async fn administrator_rights_are_added_without_disturbing_other_groups() {
        let h = harness("promote");
        h.service
            .update_user(
                "bob",
                UserPatch {
                    administrator: Some(true),
                    ..UserPatch::default()
                },
                Some("alice"),
            )
            .await
            .ok();
        // -a -G adds; a bare -G would replace every group the account is in.
        assert!(
            h.exec
                .ran_with("/usr/sbin/usermod", &["-a", "-G", "wheel", "bob"])
                .await,
            "{:#?}",
            h.exec
                .ran()
                .await
                .iter()
                .map(|r| r.display())
                .collect::<Vec<_>>()
        );
    }

    /// Setting a password gives an account a hash again, which unlocks it — so
    /// an explicit lock has to be applied afterwards or "reset and lock" would
    /// quietly leave the account open.
    #[tokio::test]
    async fn locking_is_applied_after_a_password_so_the_explicit_instruction_wins() {
        let h = harness("order");
        h.service
            .update_user(
                "bob",
                UserPatch {
                    password: Some("a-new-password".into()),
                    locked: Some(true),
                    ..UserPatch::default()
                },
                Some("alice"),
            )
            .await
            .ok();

        let ran = h.exec.ran().await;
        let chpasswd = ran.iter().position(|r| r.program == CHPASSWD).unwrap();
        let lock = ran
            .iter()
            .position(|r| r.program == USERMOD && r.args == ["-L", "bob"])
            .unwrap();
        assert!(chpasswd < lock, "the lock must come last");
    }

    #[tokio::test]
    async fn you_cannot_remove_the_account_you_are_signed_in_as() {
        let h = harness("self");
        let err = h
            .service
            .delete_user("alice", false, Acknowledgements::default(), Some("alice"))
            .await
            .unwrap_err();
        assert!(matches!(err, SysError::Invalid(_)), "{err:?}");
        assert!(h.exec.ran().await.is_empty());
    }

    #[tokio::test]
    async fn removing_an_account_says_where_its_files_went_or_did_not() {
        let h = harness("delete");
        let kept = h
            .service
            .delete_user("bob", false, Acknowledgements::default(), Some("alice"))
            .await
            .unwrap();
        assert_eq!(kept.kept_home.as_deref(), Some("/home/bob"));
        assert_eq!(kept.removed_home, None);
        assert!(h.exec.ran_with("/usr/sbin/userdel", &["bob"]).await);
    }

    #[tokio::test]
    async fn destroying_a_home_directory_needs_the_acknowledgement() {
        let h = harness("delete-home");
        let err = h
            .service
            .delete_user("bob", true, Acknowledgements::default(), Some("alice"))
            .await
            .unwrap_err();
        assert!(matches!(err, SysError::Invalid(_)), "{err:?}");
        assert!(h.exec.ran().await.is_empty());

        let removed = h
            .service
            .delete_user(
                "bob",
                true,
                Acknowledgements {
                    may_lose_data: true,
                },
                Some("alice"),
            )
            .await
            .unwrap();
        assert_eq!(removed.removed_home.as_deref(), Some("/home/bob"));
        assert!(h.exec.ran_with("/usr/sbin/userdel", &["-r", "bob"]).await);
    }

    #[tokio::test]
    async fn a_schedule_is_the_nodes_own_and_reads_back_as_one() {
        let h = harness("schedule");
        let now = now_unix();

        let before = h.service.power().await.unwrap();
        assert!(before.scheduled.is_none());
        assert_eq!(before.horizon_secs, SCHEDULE_HORIZON_SECS);

        let after = h
            .service
            .power_at(PowerAction::Reboot, now + 1800)
            .await
            .unwrap();
        let scheduled = after.scheduled.expect("a schedule");
        assert_eq!(scheduled.action, PowerAction::Reboot);
        assert_eq!(scheduled.at, now + 1800);

        // Cancelling twice is a conflict rather than a silent success: the
        // second one did nothing, and saying so is more useful than agreeing.
        assert!(h.service.cancel_power().await.is_ok());
        assert!(matches!(
            h.service.cancel_power().await.unwrap_err(),
            SysError::Conflict(_)
        ));
    }

    #[tokio::test]
    async fn a_schedule_in_the_past_never_reaches_the_node() {
        let h = harness("past");
        let err = h
            .service
            .power_at(PowerAction::Reboot, now_unix() - 60)
            .await
            .unwrap_err();
        assert!(matches!(err, SysError::Invalid(_)), "{err:?}");
        assert!(h.power.scheduled().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_node_that_refuses_to_restart_says_so_in_its_own_words() {
        let h = harness("refused");
        h.power.refuse("Interactive authentication required").await;
        let err = h.service.power_now(PowerAction::Reboot).await.unwrap_err();
        assert!(
            err.to_string().contains("Interactive authentication"),
            "{err}"
        );
    }

    /// Somebody at the keyboard ran `shutdown -r +30`; the console must show
    /// it, because there is only one schedule and it is the node's.
    #[tokio::test]
    async fn a_schedule_somebody_else_set_shows_up_here() {
        let h = harness("foreign");
        h.power
            .preset_schedule(PowerAction::PowerOff, now_unix() + 900)
            .await;
        let view = h.service.power().await.unwrap();
        assert_eq!(view.scheduled.unwrap().action, PowerAction::PowerOff);
    }
}
