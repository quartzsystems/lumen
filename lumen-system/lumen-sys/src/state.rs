//! Observed state: what the node's own files say about the accounts on it.
//!
//! Deliberately separate from [`crate::model`]: an account somebody made with
//! `useradd` at the keyboard is here too, and so is the login state, which no
//! request can tell you.
//!
//! ## The files, not `getent`
//!
//! `/etc/passwd`, `/etc/shadow`, and `/etc/group` are read directly. They are
//! plain, colon-separated, documented, and stable — parsing them is thirty
//! lines and no process. `getent` would be a subprocess per read on a page that
//! polls, and it answers for NSS as a whole: LDAP, SSSD, and anything else the
//! node is joined to. That sounds like a feature until the console offers a
//! Remove button next to an account that lives in a directory server, so the
//! narrower answer is the right one. **Local accounts are what this page
//! manages, and local accounts are what these files hold.**

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::{
    LocalUser, LoginState, ADMIN_GROUP, FIRST_HUMAN_UID, LAST_HUMAN_UID, NOLOGIN_SHELLS,
};

/// Where the account database lives. Overridable so tests read a directory of
/// their own rather than the machine running them.
#[derive(Debug, Clone)]
pub struct AccountFiles {
    pub passwd: PathBuf,
    pub shadow: PathBuf,
    pub group: PathBuf,
    pub shells: PathBuf,
}

impl Default for AccountFiles {
    fn default() -> Self {
        Self {
            passwd: "/etc/passwd".into(),
            shadow: "/etc/shadow".into(),
            group: "/etc/group".into(),
            shells: "/etc/shells".into(),
        }
    }
}

impl AccountFiles {
    /// The same four files under another root, for a test.
    pub fn under(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            passwd: root.join("passwd"),
            shadow: root.join("shadow"),
            group: root.join("group"),
            shells: root.join("shells"),
        }
    }
}

/// Everything the node says about its accounts, read in one pass.
#[derive(Debug, Clone, Default)]
pub struct Accounts {
    pub users: Vec<LocalUser>,
    /// The login shells `/etc/shells` offers. Empty when it could not be read,
    /// which the validator treats as "skip that check" rather than as "no
    /// shells exist".
    pub shells: Vec<String>,
}

/// Read the account database.
///
/// `/etc/shadow` is readable only by root, which the appliance's daemon is. A
/// node where it cannot be read still lists its accounts — with
/// [`LoginState::Unknown`] rather than a guess — because an account list is
/// useful even when the login state is not knowable.
pub async fn read(files: &AccountFiles) -> Accounts {
    let passwd = tokio::fs::read_to_string(&files.passwd)
        .await
        .unwrap_or_default();
    let group = tokio::fs::read_to_string(&files.group)
        .await
        .unwrap_or_default();
    let shadow = tokio::fs::read_to_string(&files.shadow).await.ok();
    let shells = tokio::fs::read_to_string(&files.shells).await.ok();

    Accounts {
        users: parse(&passwd, &group, shadow.as_deref()),
        shells: parse_shells(shells.as_deref().unwrap_or_default()),
    }
}

/// Turn the three files into accounts. Pure, so it is the thing under test.
pub fn parse(passwd: &str, group: &str, shadow: Option<&str>) -> Vec<LocalUser> {
    let hashes = shadow.map(parse_shadow);
    let (members, primary_names) = parse_group(group);

    let mut users: Vec<LocalUser> = passwd
        .lines()
        .filter_map(|line| {
            // A comment or a blank line is not a record, and a short line is a
            // damaged one — neither is worth inventing an account from.
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() < 7 {
                return None;
            }
            let name = fields[0].to_string();
            let uid: u32 = fields[2].parse().ok()?;
            let gid: u32 = fields[3].parse().ok()?;
            let shell = fields[6].to_string();

            let mut groups: Vec<String> = Vec::new();
            if let Some(primary) = primary_names.get(&gid) {
                groups.push(primary.clone());
            }
            for (group_name, group_members) in &members {
                if group_members.contains(&name) && !groups.contains(group_name) {
                    groups.push(group_name.clone());
                }
            }

            let login = login_state(&name, &shell, hashes.as_ref());

            Some(LocalUser {
                administrator: groups.iter().any(|g| g == ADMIN_GROUP),
                // The GECOS field is comma-separated — name, office, phones —
                // and only the first part is a person's name.
                full_name: fields[4]
                    .split(',')
                    .next()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                home: fields[5].to_string(),
                password_changed_days: hashes
                    .as_ref()
                    .and_then(|h| h.get(&name))
                    .and_then(|entry| entry.changed_days),
                system: !(FIRST_HUMAN_UID..=LAST_HUMAN_UID).contains(&uid),
                name,
                uid,
                gid,
                shell,
                groups,
                login,
            })
        })
        .collect();

    // People first, then the package-owned accounts, each by identifier. An
    // operator opening this page is looking for a person, and `root` sorting
    // above the twenty accounts nobody made is worth the special case.
    users.sort_by_key(|user| (user.system, user.uid));
    users
}

/// One `/etc/shadow` record, reduced to the two things worth reading.
struct ShadowEntry {
    hash: String,
    changed_days: Option<u64>,
}

fn parse_shadow(shadow: &str) -> HashMap<String, ShadowEntry> {
    shadow
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() < 3 {
                return None;
            }
            Some((
                fields[0].to_string(),
                ShadowEntry {
                    hash: fields[1].to_string(),
                    changed_days: fields[2].parse().ok(),
                },
            ))
        })
        .collect()
}

/// Every group's name and its listed members, in file order.
type GroupMembers = Vec<(String, Vec<String>)>;

/// Group membership, and the name of each group by identifier.
fn parse_group(group: &str) -> (GroupMembers, HashMap<u32, String>) {
    let mut members = Vec::new();
    let mut names = HashMap::new();
    for line in group.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 4 {
            continue;
        }
        let name = fields[0].to_string();
        if let Ok(gid) = fields[2].parse::<u32>() {
            names.insert(gid, name.clone());
        }
        let list = fields[3]
            .split(',')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .collect();
        members.push((name, list));
    }
    (members, names)
}

fn parse_shells(shells: &str) -> Vec<String> {
    let mut out: Vec<String> = shells
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('/'))
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Which of the three ways a Unix account says "no" applies here.
///
/// The order matters: a nologin shell beats everything, because setting a
/// password on such an account changes nothing an operator would notice.
fn login_state(
    name: &str,
    shell: &str,
    hashes: Option<&HashMap<String, ShadowEntry>>,
) -> LoginState {
    if NOLOGIN_SHELLS.contains(&shell) {
        return LoginState::NoLogin;
    }
    let Some(hashes) = hashes else {
        return LoginState::Unknown;
    };
    let Some(entry) = hashes.get(name) else {
        return LoginState::Unknown;
    };
    let hash = entry.hash.as_str();
    // `!` is `usermod -L`, and `!!` is an account that has never had a
    // password set. Both are locked; neither is the same as `*`, which is a
    // hash no input can ever produce.
    if hash.starts_with('!') {
        LoginState::Locked
    } else if hash.is_empty() || hash == "*" {
        LoginState::NoPassword
    } else {
        LoginState::Enabled
    }
}

/// This node's name, read the way `lumen_net` reads it.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "lumen".to_string())
}

/// How long the node has been up, in seconds.
///
/// From `/proc/uptime`, which `ProtectKernelTunables` makes read-only rather
/// than unreadable — the same distinction `lumen_net` relies on for the
/// hostname.
pub fn uptime_secs() -> Option<u64> {
    let raw = std::fs::read_to_string("/proc/uptime").ok()?;
    raw.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .map(|seconds| seconds as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
bin:x:1:1:bin:/bin:/sbin/nologin
qemu:x:107:107:qemu user:/:/sbin/nologin
alice:x:1000:1000:Alice Kowalski,Rack 4,,:/home/alice:/bin/bash
bob:x:1001:1001::/home/bob:/bin/bash
carol:x:1002:1002:Carol:/home/carol:/bin/bash
nobody:x:65534:65534:Kernel Overflow User:/:/usr/sbin/nologin
";

    const GROUP: &str = "\
root:x:0:
bin:x:1:
wheel:x:10:alice,bob
qemu:x:107:
alice:x:1000:
bob:x:1001:
carol:x:1002:
nobody:x:65534:
";

    const SHADOW: &str = "\
root:$6$abc$def:20000:0:99999:7:::
bin:*:19000:0:99999:7:::
alice:$6$xyz$uvw:20100:0:99999:7:::
bob:!$6$locked$hash:20050:0:99999:7:::
carol::20060:0:99999:7:::
";

    #[test]
    fn the_three_files_become_the_accounts_they_describe() {
        let users = parse(PASSWD, GROUP, Some(SHADOW));
        let by_name = |name: &str| {
            users
                .iter()
                .find(|u| u.name == name)
                .unwrap_or_else(|| panic!("{name} should be present"))
                .clone()
        };

        let alice = by_name("alice");
        assert_eq!(alice.uid, 1000);
        assert_eq!(alice.home, "/home/alice");
        // Only the first GECOS component is a person's name; the rest is the
        // office and the phone numbers nobody has filled in since 1978.
        assert_eq!(alice.full_name.as_deref(), Some("Alice Kowalski"));
        assert!(alice.administrator, "alice is in wheel");
        assert!(!alice.system);
        assert_eq!(alice.login, LoginState::Enabled);
        assert_eq!(alice.password_changed_days, Some(20_100));
        assert!(alice.groups.contains(&"wheel".to_string()));
        assert!(alice.groups.contains(&"alice".to_string()));

        // An empty GECOS field is nothing, not an empty name.
        assert_eq!(by_name("bob").full_name, None);

        let root = by_name("root");
        assert!(root.is_protected());
        assert!(root.system);
    }

    /// Three different noes, and the console needs to tell them apart: a
    /// locked account is unlocked, an account with no password is given one,
    /// and a nologin account needs its shell changed.
    #[test]
    fn the_three_ways_an_account_says_no_are_not_the_same_way() {
        let users = parse(PASSWD, GROUP, Some(SHADOW));
        let state = |name: &str| users.iter().find(|u| u.name == name).unwrap().login;

        assert_eq!(state("alice"), LoginState::Enabled);
        assert_eq!(state("bob"), LoginState::Locked, "usermod -L prefixes !");
        assert_eq!(state("carol"), LoginState::NoPassword, "no hash at all");
        assert_eq!(state("bin"), LoginState::NoLogin, "the shell refuses first");
    }

    /// The daemon is root and can read it, but a control plane that answers
    /// with less is better than one that refuses to answer.
    #[test]
    fn an_unreadable_shadow_file_is_unknown_rather_than_assumed_fine() {
        let users = parse(PASSWD, GROUP, None);
        assert!(users
            .iter()
            .filter(|u| u.shell == "/bin/bash")
            .all(|u| u.login == LoginState::Unknown));
        // The shell still settles the ones it settles, with no shadow at all.
        assert_eq!(
            users.iter().find(|u| u.name == "bin").unwrap().login,
            LoginState::NoLogin
        );
    }

    #[test]
    fn people_sort_above_the_accounts_nobody_made() {
        let users = parse(PASSWD, GROUP, Some(SHADOW));
        let order: Vec<&str> = users.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            order,
            ["alice", "bob", "carol", "root", "bin", "qemu", "nobody"]
        );
    }

    /// The person test is a range, not a floor. `nobody` sits at 65534 — the
    /// kernel overflow UID, above `useradd`'s allocation ceiling — and a
    /// floor-only test files it among the people of every node.
    #[test]
    fn the_kernel_overflow_account_is_not_a_person() {
        let users = parse(PASSWD, GROUP, Some(SHADOW));
        let nobody = users.iter().find(|u| u.name == "nobody").unwrap();
        assert!(nobody.system, "{nobody:?}");
    }

    #[test]
    fn a_damaged_line_is_skipped_rather_than_invented_from() {
        let users = parse(
            "alice:x:1000:1000::/home/alice:/bin/bash\n\
             # a comment\n\
             \n\
             truncated:x:1001\n\
             notanumber:x:abc:def::/home/x:/bin/sh\n",
            GROUP,
            None,
        );
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].name, "alice");
    }

    #[test]
    fn only_real_paths_count_as_shells() {
        let shells = parse_shells("# /etc/shells\n/bin/sh\n/bin/bash\n\n/bin/bash\nnonsense\n");
        assert_eq!(shells, ["/bin/bash", "/bin/sh"]);
    }

    #[tokio::test]
    async fn a_node_with_no_account_files_at_all_reads_as_empty_rather_than_failing() {
        let accounts = read(&AccountFiles::under("/nonexistent-lumen-test-root")).await;
        assert!(accounts.users.is_empty());
        assert!(accounts.shells.is_empty());
    }
}
