# Lumen system

The node itself: its local accounts, its power state, and the one mechanism
every other domain borrows — a way to run a privileged command **outside** the
control plane's sandbox.

```
lumen-system/
└── lumen-sys/         the system domain (Rust library crate)
lumen-controlplane/
└── src/api/system.rs  thin HTTP handlers over lumen-sys
lumen-webui/
└── app/(console)/system/{general,authentication,maintenance}
```

## Component split

A fourth domain, laid out exactly as the other three:

```
model.rs        what a local account is; what a power request is
state.rs        what /etc/passwd, /etc/shadow, and /etc/group say
validate.rs     pure rules over an account and the node it would live on
exec.rs         running a privileged command outside this daemon's sandbox
backend/        logind over the system bus, plus mock/ and unavailable/
service.rs      the one entry point the control plane calls
```

### The dependency direction

`lumen-sys` is the **most basic** domain in the tree. It depends on none of the
others, and `lumen-zfs` depends on it:

```
lumen-sys  <-  lumen-zfs  <-  lumen-virt
               lumen-net  <-------^
```

Storage depends on system because creating a pool is a privileged command on
the node before it is anything to do with storage. That is the *only* thing it
borrows — `lumen_sys::exec` — and it borrows it for exactly two operations.

---

## Decisions

### `lumen-execd` was not needed; systemd already is one

docs/compute.md left a placeholder: a small privileged helper that would exist
to perform the operations the control plane's sandbox forbids. It turns out the
appliance already has one.

Two operations genuinely cannot happen inside `ProtectSystem=strict`:

- **Creating a local account** writes `/etc/passwd`, `/etc/shadow`, and
  `/etc/group`, and `useradd` makes lock files next to them — so it needs
  `/etc` writable, not four individual files.
- **`zpool create` and `zpool destroy`** write `/etc/zfs/zpool.cache`. Every
  other storage operation reaches the kernel through `/dev/zfs` and needs
  nothing, which docs/compute.md reproduced.

`ReadWritePaths=/etc` would make both work in about four characters, and would
also hand a network-facing daemon write access to `sudoers`, every PAM stack,
and every unit file on the box. That is a far larger thing to give away than
the two commands that need it.

**Chosen: hand the command to systemd.** `systemd-run --quiet --collect --wait
--pipe --service-type=oneshot -- /usr/sbin/useradd …` starts it as a transient
unit — a child of PID 1, outside this daemon's namespace, with none of this
daemon's restrictions and none of its privileges either. The sandbox is
unchanged.

This is the arrangement every other domain already has, spelled out one more
time:

| Domain | Privileged work happens in |
| ------ | -------------------------- |
| networking | NetworkManager, over the system bus |
| machines | `virtqemud`, over its socket |
| accounts, pools | a transient unit, started by systemd |

In each case the privileged work happens in another process with its own API,
which is precisely why none of the hardening has to move.

### `systemd-run`, not `StartTransientUnit` over the bus

The bus call would avoid a process, at the cost of hand-building an `a(sv)`
property list, subscribing to `JobRemoved`, reading `ExecMainStatus` back off
the unit, and garbage-collecting it afterwards — roughly two hundred lines to
reimplement what four flags already do correctly.

Those four flags are the whole of the contract, and `exec::tests::
the_flags_are_the_contract_with_systemd` asserts each of them:

| Flag | Why it is not optional |
| ---- | ---------------------- |
| `--wait` | Without it the call returns as soon as the job is queued and **every failure becomes silence**. |
| `--pipe` | How the output comes back, and how a password reaches `chpasswd` without ever being an argument. It is also the one flag with a cost — see the SELinux rule below. |
| `--collect` | Unloads the unit even when it failed, so a node does not accumulate one dead unit per mistyped account name. |
| `--quiet` | The only thing on standard error should be what the command itself said. |

This is the same trade `lumen-zfs` made against `libzfs`, and it carries the
same rule: **typed argument arrays, never an interpolated shell string**. There
is nothing to escape because there is no string to escape into — every
invocation is an array handed to `execve`, and
`an_argument_is_an_argument_and_never_part_of_a_sentence` pins it with an
argument that would be a disaster in a shell.

### `--wait` is bounded, because the alternative is a page that never answers

`--wait` blocks until the unit is over, and nothing here legitimately takes
minutes: `useradd` writes three files and `zpool create` labels its disks and
returns. So the wait carries a two-minute deadline
(`exec::DEADLINE`, pinned by `a_command_that_never_returns_is_given_up_on`).

Without it a command that blocks — a disk that will not settle, a bus that took
the job and never answered — leaves the HTTP request that asked for it pending
for as long as the browser is willing to wait. What an operator sees is a
dialog stuck on *Creating…*: no error, no progress, and nothing to do but
reload the page and guess whether the pool was made. That is strictly worse
than a slow answer.

Giving up ends `systemd-run`, but **not** the transient unit — it is a child of
PID 1 and outlives the process that asked for it. The message says so and names
`systemctl list-units 'run-*'`, because "gave up waiting" must not be read as
"nothing happened".

The same bound is on the storage domain's *reads* (`lumen_zfs`'s
`backend::cli::DEADLINE`, thirty seconds). `zpool list` answers in milliseconds
on a healthy node and not at all on one with a disk that has stopped
responding, and several of those run before a pool is ever created — so an
unbounded read there hangs the create request without a single command having
been delegated, which looks exactly like a hung `zpool create` and is not one.

### `--pipe` needs an SELinux rule, and the missing one is invisible

`--pipe` is the flag with a cost. Passing the daemon's standard input, output,
and error to the bus means passing **file descriptors**, and those descriptors
are pipes carrying the daemon's own label — `unconfined_service_t`, which is
what a service binary with no policy of its own gets. `dbus-broker` has to
accept them to relay the call, and stock EL policy does not let it read a pipe
labelled that way.

So the broker refuses the message. Every privileged command the daemon
delegates fails, together: creating a pool, creating an account, and — because
the same daemon reaches logind over the same bus — the restart controls.

**It has two shapes, and only one of them is an error.** Sometimes the broker
drops the connection and `systemd-run` reports `Failed to start transient
service unit: Connection reset by peer`. Sometimes the message is simply never
relayed and no reply ever comes, and then `systemd-run` waits — forever. The
second shape is the one that is hard to recognise, because nothing fails:

```
$ ps -eLo tid,stat,wchan:32,comm -p $(pidof lumen-controlplane)
 2844 S    poll_schedule_timeout.constprop. systemd-run      # blocked on the bus
$ systemctl list-units 'run-*' --all                          # no transient unit
$ pgrep -a zpool                                              # nothing ever started
```

That triple is the signature: **a `systemd-run` that exists, a unit that does
not, and no command running.** `systemd_refused` cannot catch it — there is no
message to match on — which is why the wait is bounded instead. See "`--wait`
is bounded" above.

**The denial is `dontaudit`ed upstream**, which is the part worth writing down.
Under `Enforcing` it produces no audit record at all. `getenforce` says
`Enforcing`, `ausearch -m AVC` says nothing, `systemctl is-active polkit` says
`active`, and PID 1's journal is empty for the moment of the failure — because
PID 1 never received the message. What an operator has to work with instead is
`systemd-run` reporting `Failed to start transient service unit: Connection
reset by peer`, and a console that says `Internal server error`. Nothing in
that chain contains the word SELinux.

The rule that surfaces it:

```sh
semodule -DB                              # disable the dontaudit rules
# reproduce — create a pool from the console
ausearch -m AVC,USER_AVC -ts recent
semodule -B                               # put them back
```

`lumen-controlplane/selinux/lumen-controlplane.te` carries the grants, and
`lumen-controlplane.spec` builds it into the package and loads it at priority
200. The daemon gains nothing: it already holds the descriptors, and the grants
are to `dbus-broker` and PID 1, for accepting pipes they are being handed on
purpose. The unit's `ProtectSystem=strict` sandbox does not move.

Two grants per recipient, because receiving a passed descriptor is one kernel
hook making two decisions: `fd use` on the descriptor itself, and the
`fifo_file` permission matching the mode it was opened for. Version 1.0 of the
module carried only the fifo half, and the symptom did not change — a refusal
of either check looks identical from outside, and both denials are
dontaudit'ed. And the broker is not the last recipient: it relays the
descriptors to PID 1, which receives them the same way, so `init_t` gets the
same pair rather than being assumed covered by stock policy.

The target type is broader than it should be — `unconfined_service_t` is every
service without a policy of its own, not just this one. Narrowing it means
giving the control plane a domain of its own, `lumen_controlplane_t`, with file
contexts, a transition, and rules for PAM, `/dev/zfs`, the hypervisor socket,
and its state directory. That is the right end state and it is a great deal
more work than the rule above; it is listed under *Out of scope for this stage*.

### A password is never an argument

A password on a command line is visible to every process on the box through
`/proc`, and systemd records a unit's command in the journal besides. So
`chpasswd` reads `name:password` from **standard input**, which `--pipe`
connects straight through to the transient unit.

`Request::args` is never secret and `Request::stdin` never reaches a log —
`Request::display`, which is what the log and the error message use, does not
include it. Two tests pin this, one in the domain and one over HTTP.

Two commands rather than one, incidentally, because `useradd --password` takes
a *hash*: hashing in this daemon would mean choosing an algorithm and a cost
that the node's own `/etc/login.defs` already chose. `chpasswd` uses the node's
settings and applies its `pwquality` stack, and **that stack's refusal is
passed through verbatim** rather than summarised — it is the node's policy
talking, and it is the only thing that can explain itself.

An account that is created and then cannot have a password set is **removed
again**. An account with no password is one anybody can be invited to guess at,
and leaving one behind is worse than the failure that produced it.

### The files, not `getent`

`/etc/passwd`, `/etc/shadow`, and `/etc/group` are read directly. They are
plain, colon-separated, documented, and stable; parsing them is thirty lines
and no process.

`getent` would be a subprocess per read, and it answers for NSS as a whole:
LDAP, SSSD, and anything else the node is joined to. That sounds like a feature
until the console offers a Remove button next to an account that lives in a
directory server. **Local accounts are what this page manages, and local
accounts are what these files hold.**

Nothing is cached, so nothing can disagree with `getent`, and an account
somebody made at the keyboard appears here without anything being told about
it. Same reasoning as `lumen-virt` keeping the domain document as its database.

### Three ways to say no, and they are not interchangeable

A Unix account can refuse a sign-in three different ways, and collapsing them
into one "disabled" flag would make the console offer the wrong remedy:

| `login` | What it is | The fix |
| ------- | ---------- | ------- |
| `locked` | `!` in front of the password hash — `usermod -L` | Unlock it; the password underneath is preserved |
| `no_password` | no hash at all, or `*` | Set a password |
| `nologin` | the shell refuses | Change the shell — a password does nothing |
| `unknown` | `/etc/shadow` could not be read | Genuinely unknown, not assumed fine |

The order matters when they overlap: a nologin shell wins, because setting a
password on such an account changes nothing an operator would notice.

### Nothing may take the console away from the operator using it

This is the rule the whole appliance is built around. Networking has a
checkpoint that rolls itself back for exactly this reason; here it is simply
refused, because there is nothing to roll back to once you cannot sign in.

Every account route passes the session's own principal down as `acting_as`, and
the validator refuses:

| Code | Rejected because |
| ---- | ---------------- |
| `would_lock_you_out` | locking, demoting, or removing the account you are signed in as — or the **last** administrator, which is the same failure one step removed |
| `reserved_username` | `root` is the account this appliance is recovered with, and is not changed from a web page. `passwd` at the console still works, and somebody at the console has not lost anything |
| `invalid_username` | not a usable account name (`shadow-utils`' own rule) |
| `duplicate_username` | this node already has one |
| `password_too_short` | below the minimum; the node's `pwquality` applies on top |
| `invalid_shell` | not a login shell on this node — an unreadable `/etc/shells` **skips** the check rather than failing every account |
| `unacknowledged_destructive_operation` | removing an account *with* its home directory |
| `time_in_the_past`, `time_too_far_ahead` | a scheduled restart that is not a schedule |

The same rule governs storage: the pool the appliance is installed on is never
destroyable. `state::root_pool` reads it out of `/proc/mounts` — the ZFS
filesystem mounted at `/` — so it is the node's answer rather than a name
written down here.

### Power goes through logind, and the schedule is the node's

`ScheduleShutdown` is what `shutdown -r +30` calls, and using it rather than a
timer of Lumen's own buys three things:

- it **survives the control plane restarting**, because logind is holding it;
- **every signed-in session is warned** on its terminal, on logind's schedule;
- `shutdown -c` at the keyboard cancels it, because there is only one schedule
  and it is the node's — which also means one somebody else set **shows up on
  this page**.

Reboot-now goes through the same interface for consistency, and because
`Reboot(false)` is a policy-checked call rather than a signal: a node that
refuses says so.

An immediate restart answers **`202 Accepted` with no body**. The connection is
about to go away, and a JSON object claiming success would be a promise this
daemon cannot keep. The confirmation lives in the dialog in front of it — where
the operator types the node's name — rather than as an acknowledgement field,
because unlike stopping a machine this is something the console can neither
undo nor report the result of.

### The disk picker exists so a pool is never built on the wrong disk

`zpool create` destroys whatever was on the disks it is given. A picker that
lists `/dev/sda` and `/dev/sdb` with nothing more is a picker that will
eventually be used to reformat the disk the appliance is running from.

So `lumen_zfs::devices` reports, for every disk, **what is already on it** — in
words: "mounted at /", "in use as swap", "3 partitions" — read from
`/sys/block`, `/proc/mounts`, and `/proc/swaps`, all of which the sandbox
leaves readable. A disk that is spoken for cannot be chosen without the
acknowledgement, and the refusal names what is on it.

Two details worth spelling out:

- **`zd*` is excluded.** Those are ZFS's own volumes — a guest's disks. Offering
  one as a candidate for a new pool would destroy a virtual machine.
- **A pool is built on `/dev/disk/by-id/…`, never on `/dev/sdb`.** The kernel
  name is whatever was enumerated second *this boot*; a pool built on it can
  come back after a reboot pointing at a different disk. The service resolves
  whatever the request named to the stable path the node reported before
  anything is run.

`ashift=12` is fixed rather than detected. A great many drives still report
512-byte sectors for compatibility, the value **cannot be changed after
creation**, and a pool built at `ashift=9` on a 4 KiB disk is slow forever. 12
costs a little space on a genuinely-512-byte disk and is what every ZFS guide
has recommended for a decade — and what the installer already uses.

The pool root is created `canmount=off mountpoint=none`, so it never appears at
`/<name>`. That matters more since the root pool was renamed `boot`: without
it, a pool called `boot` would try to mount over `/boot`.

---

## API

All routes require a valid session and use the existing error envelope
`{ "error": … }`. A rejected request adds an `errors` array alongside it, each
entry carrying `code`, `field`, and `message`.

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/system/users` | Every local account, and what may be done to each |
| GET | `/api/system/users/:name` | One account |
| POST | `/api/system/users` | Create one |
| PATCH | `/api/system/users/:name` | Change one; absent fields are left alone |
| DELETE | `/api/system/users/:name` | Remove one; `remove_home` off by default |
| GET | `/api/system/power` | Uptime, the node's clock, and anything scheduled |
| POST | `/api/system/power` | Restart or shut down — now, or at `at` |
| DELETE | `/api/system/power` | Call off whatever is scheduled |
| GET | `/api/storage/devices` | Every disk, and what is already on each |
| POST | `/api/storage/pools` | Build a pool |
| DELETE | `/api/storage/pools/:pool` | Destroy one, and everything on it |

---

## Web UI

**General** is a stub: hostname, timezone, and the appliance's own certificate
belong there and none of them is in this stage.

**Authentication** is the accounts table. The `lumen` realm authenticates
against PAM — that is, against this node's own accounts — so there is no
separate console user list to keep in step: an account made here is an account
at the keyboard, over SSH, and on this page. Every control's `allowed` and
`reason` come from the same validator the request will run through, so the
console and the node can never disagree about what is possible.

Deliberately **not polled**. The account files change only when somebody
changes them, and a table that reorders itself under a cursor for no reason is
worse than one that needs Refresh.

**Maintenance** is restart, schedule, and shut down. The countdown runs against
the **node's** clock — `PowerView.now` — offset by how long ago the console
read it, so a workstation a minute out does not show a countdown the node
disagrees with.

Both destructive controls ask for the node's or the pool's **name to be typed**
rather than a checkbox ticked. A checkbox is ticked without reading; typing the
name cannot be done to the wrong node by accident, which is the mistake that
actually happens.

---

## Walkthrough

```sh
HOST=https://192.168.10.5:8443
JAR=$(mktemp)
curl -sk -c "$JAR" -X POST "$HOST/api/auth/login" \
     -H 'Content-Type: application/json' \
     -d '{"username":"root","password":"…","realm":"lumen"}' | jq

# 1. Who is on this node, and what may be done to them.
curl -sk -b "$JAR" "$HOST/api/system/users" |
  jq '.users[] | {name, uid, administrator, login, is_you}'

# 2. Make an account. The password is in the body and nowhere else.
curl -sk -b "$JAR" -X POST "$HOST/api/system/users" \
     -H 'Content-Type: application/json' \
     -d '{"name":"alice","password":"correct-horse-battery",
          "full_name":"Alice Kowalski","administrator":true}' | jq '{name, uid, groups}'

#    A rejected one answers with the codes the console pins to fields:
curl -sk -b "$JAR" -X POST "$HOST/api/system/users" \
     -H 'Content-Type: application/json' \
     -d '{"name":"Bad Name","password":"x"}' | jq '.errors[] | {code, field}'

# 3. The rule the appliance is built around, over HTTP:
curl -sk -b "$JAR" -X PATCH "$HOST/api/system/users/root" \
     -H 'Content-Type: application/json' -d '{"locked":true}' | jq -r .error
# "root" is this node's recovery account and is not changed from the console. …

# 4. Restart in half an hour, and change your mind.
NOW=$(curl -sk -b "$JAR" "$HOST/api/system/power" | jq .now)
curl -sk -b "$JAR" -X POST "$HOST/api/system/power" \
     -H 'Content-Type: application/json' \
     -d "{\"action\":\"reboot\",\"at\":$((NOW + 1800))}" | jq .scheduled
#    It is logind's, so the node agrees:
#    $ shutdown --show
curl -sk -b "$JAR" -X DELETE "$HOST/api/system/power" | jq .scheduled

# 5. What could a pool be built on?
curl -sk -b "$JAR" "$HOST/api/storage/devices" |
  jq '.devices[] | {name, path, size, in_use, used_by}'

# 6. Build one. A disk with something on it is refused until it is said out loud.
curl -sk -b "$JAR" -X POST "$HOST/api/storage/pools" \
     -H 'Content-Type: application/json' \
     -d '{"name":"tank","vdev":"raidz1","disks":["sdb","sdc","sdd"]}' |
  jq '{name, size, health}'

curl -sk -b "$JAR" -X DELETE "$HOST/api/storage/pools/boot" \
     -H 'Content-Type: application/json' \
     -d '{"i_understand_this_may_lose_data":true}' | jq -r .error
# "boot" is the pool this appliance is installed on and cannot be destroyed …
```

---

## Manual test script

Automated tests run against the in-memory backends and the fake exec: they
cover the refusals, the ordering, and the fact that a password never becomes an
argument. **They cannot cover systemd actually starting a unit, logind actually
restarting a node, or `zpool` actually reformatting a disk.** Run these on real
hardware.

### 0. The claim everything else rests on

```sh
# The transient unit runs outside the daemon's sandbox. This must succeed
# while /etc is read-only inside it.
systemd-run --quiet --collect --wait --pipe --service-type=oneshot \
  -- /usr/bin/test -w /etc && echo "writable outside the sandbox (expected)"
systemctl show lumen-controlplane -p ProtectSystem   # must still be strict

# And the security module, without which none of the above works *from the
# daemon* even though it works from this shell — the shell is a different
# domain, so passing this test proves nothing on its own.
semodule -l | grep -q '^lumen-controlplane' \
  && echo "security module loaded (expected)"
```

Do this one from the console rather than from a shell, because a shell cannot
reproduce it: create a pool, create an account, and open the Maintenance page.
All three go through the same delegation, and all three fail together when the
module is missing. If any of them reports `Internal server error`, run
`semodule -DB`, reproduce, `ausearch -m AVC,USER_AVC -ts recent`, `semodule -B`
— the denial is `dontaudit`ed and there is otherwise nothing in any log.

`journalctl -u lumen-controlplane` is the other half of that, and it only
became useful in this release: the default log filter named `lumen_controlplane`
and nothing else, so every line from the crates that do the work — `lumen_sys`,
`lumen_zfs`, `lumen_virt`, `lumen_net` — was dropped before it reached the
journal. "running outside the sandbox", "privileged command failed", and
"could not open the console socket" are all in that set, which meant the journal
an operator was sent to was guaranteed to be empty of exactly the lines they
were sent to find. `main.rs` now lists all five; `RUST_LOG` still overrides it.

### 1. An account, end to end

1. Console → **System → Authentication → Create**: `alice`, administrator on.
2. On the node: `getent passwd alice`, `id alice` (must include `wheel`), and
   `ls -ld /home/alice`.
3. `journalctl -u lumen-controlplane | grep 'account created'` — and
   `journalctl | grep -i alice` must **not** contain the password anywhere.
   That is the check this design exists for; look properly.
4. Sign out and sign in as `alice`. The console must accept her, because the
   realm is PAM and PAM is the node.
5. `ssh alice@node` must work too — the same account, not a copy of it.
6. Lock her from the table. `passwd -S alice` must read `L`. Unlock: `P`.
7. Give her a bad password (three characters). The refusal must be the node's
   own `pwquality` sentence, not a summary of it.

### 2. The rule that matters

1. Signed in as `alice`, try to lock `alice`. Refused, and the reason names it.
2. Make `alice` the only administrator, then try to demote her. Refused.
3. Try to change or remove `root`. Refused both times.
4. Confirm afterwards that `alice` can still sign in — a refused request must
   have changed **nothing**.

### 3. Restart, scheduled

1. **System → Maintenance → Schedule…**, half an hour out.
2. On the node: `shutdown --show` must report the same moment. This is the
   check that proves the schedule is logind's and not the console's.
3. `systemctl restart lumen-controlplane`. The countdown must still be there.
4. Cancel it from the console; `shutdown --show` must report nothing.
5. Schedule one with `shutdown -r +45` at the keyboard. It must appear on the
   Maintenance page without anything being told about it.
6. Finally, **Restart now**. The console must say the node is restarting rather
   than showing a connection error — a dropped connection is this request
   succeeding.

### 4. A pool, on real disks

**Do this on a node with a spare disk, not on one you need.**

1. **Storage → Create**. The disk the appliance is running from must be listed
   with what is on it, and must be unselectable without the acknowledgement.
2. Build a mirror on two spare disks.
3. On the node:
   ```sh
   zpool status tank
   zpool get ashift,autotrim tank
   zfs get compression,mountpoint,canmount tank
   # The disks must be by-id paths, NOT /dev/sdX:
   zpool status tank | grep -c 'by-id'
   ```
4. `ls -l --time-style=full-iso /etc/zfs/zpool.cache` — the timestamp **must**
   have moved. That is the proof the operation happened outside the sandbox;
   if it had run inside, it would have failed with `EROFS`.
5. `journalctl -u lumen-controlplane | grep 'pool created'`, and
   `journalctl | grep 'Lumen: create the storage pool'` for systemd's own
   record of the transient unit.
6. Reboot the node. `zpool status tank` must still be healthy and still name
   the disks by identifier — that is the whole reason for the by-id path.
7. Put a machine's disk on it, then try to destroy the pool. `zpool` must
   refuse loudly rather than tearing it out from under the guest.
8. Destroy it properly, with the name typed and the box ticked.
9. Try to destroy `boot`. It must be refused before anything is run.

### 5. The node with nothing

1. Mask `systemd-logind` and restart the control plane. Maintenance must render
   with its controls explaining themselves rather than showing an error where
   the countdown goes.
2. Unmask it again.

---

## Development

```sh
make test    # installer + all four domain crates + control plane
make lint    # shellcheck, rpmlint, fmt/clippy for six manifests
```

`lumen-sys` needs no system libraries: logind is reached through pure-Rust
zbus, and privileged commands are handed to `systemd-run`. Its tests neither
restart the machine running them nor touch its accounts — the account database
is a temporary directory, the power backend records what it was asked to do
instead of doing it, and `MockExec` records commands instead of running them.

`MockExec::backed_by` is worth knowing about: given a set of account files it
*applies* `useradd`, `usermod`, `userdel`, and `chpasswd` to them, which turns
it from a mock into a fake. That is what lets the control plane's tests cover
the round trip the service actually performs — write, then read the answer back
out of the node — which is the one thing a recording mock could not.

## Out of scope for this stage

The General page. Password aging and expiry (`chage`), SSH key management, and
groups other than the administrator one — an account's group list is shown but
not edited. Directory-server realms, which are `RealmRegistry`'s business
rather than this page's. `zpool import`, `export`, `scrub`, `replace`, and
adding a vdev to an existing pool; only create and destroy are here.

An SELinux domain of the daemon's own — `lumen_controlplane_t`, with file
contexts, a transition, and rules for PAM, `/dev/zfs`, the hypervisor socket,
and its state directory. The module shipped today grants one thing to
`dbus-broker` and targets `unconfined_service_t`, which is every service
without a policy rather than this one; a domain is how that gets narrowed, and
it is a piece of work in its own right.
