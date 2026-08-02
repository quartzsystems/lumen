# Lumen updates

How an installed appliance gets newer software: where the packages come from,
what may be installed without asking, and the one thing that must never happen
by accident.

```
lumen-system/
└── lumen-update/                   the update domain (Rust library crate)
lumen-controlplane/
├── src/api/updates.rs              thin HTTP handlers over lumen-update
├── src/updates.rs                  the transaction as a watchable job
└── src/cluster_updates.rs          the same, walked across every member
lumen-webui/
└── app/(console)/system/updates    the console page
packages/
├── lumen-repo.spec                 the repository definition + public key
└── lumen.repo.in                   the definition itself, templated
.github/workflows/
├── release.yml                     signs the packages at build time
└── pages.yml                       publishes the repository index
```

## The one rule everything else follows

**An ordinary update must never move the kernel.**

Lumen boots from ZFS, and ZFS on this appliance is an out-of-tree kernel module
that tracks the kernel ABI. `iso/pins.env` already records what that means
for the ISO build — the kernel and kmod-zfs move together as one pinned
set — and notes that the module has a real history of lagging AlmaLinux
point-release kernels.

A node that ran an unguarded `dnf upgrade` would, sooner or later, install a
kernel with no matching `kmod-zfs`, reboot, and fail to import its root pool.
There is no console to fix that from. It is a drive to the rack, and it would
happen to whichever customers happened to press the button during the window
where the kernel had shipped and the module had not.

So the domain splits every pending update in two:

| | what it is | how it is installed |
|---|---|---|
| **Ordinary** | Lumen's own packages, and userland | one button, no acknowledgement |
| **Platform** | `kernel*`, `kmod-*`, `zfs*` | together or not at all, and only when the solver says they resolve |

The ordinary transaction cannot touch the platform set because it is built with
`--exclude` for every one of those prefixes. That is asserted at three layers —
in `model.rs`, in the service, and over the real router in
`tests/update_flow.rs` — because it is the property the feature exists for.

### The gate is the solver's answer, not a rule of ours

Before the platform set is offered at all, the domain asks the package manager
to resolve it as one transaction without applying it (`dnf --assumeno upgrade
kernel-core kmod-zfs-2.3 …`). If it will not resolve, the console says so,
quotes the solver, and offers no button.

This is deliberately not a version comparison Lumen maintains. The solver is
right about its own repositories; a hand-maintained compatibility rule is right
until the next point release. It is the same decision the ISO build already
made for its offline-resolution gate, down to reading the printed transaction
summary rather than the exit status — `--assumeno` exits nonzero after
succeeding.

A dry run that could not be performed at all counts as **blocked**, not as
permission. The cost of being wrong in that direction is an unbootable node.

## Component split

The fifth domain, laid out exactly as the others:

```
model.rs        what an update is, which kind it belongs to, what a plan is
backend/        the package manager, plus mock/ and unavailable/
service.rs      the one entry point the control plane calls
```

`lumen-update` depends on `lumen-sys` and on nothing else in the tree — it
borrows the privileged-command runner and needs nothing more. The packages
waiting for a node are a fact about the node, not about its machines or its
cluster.

```
lumen-sys  <-  lumen-update
           <-  lumen-zfs  <-  lumen-virt
               lumen-net  <-------^
```

### Everything goes through the privileged runner, including the reads

The daemon runs with `ProtectSystem=strict`. Refreshing repository metadata
writes a cache, taking the package manager's lock writes another directory, and
a transaction writes the whole system. None of it is permitted inside the
sandbox, so every invocation is handed to systemd as a transient unit, exactly
as `useradd` and `zpool create` are (see [system.md](system.md)).

The one difference is the deadline. `lumen_sys::exec` defaults to two minutes,
which is generous for `useradd` and nowhere near enough for a transaction that
downloads a kernel — so the update backend is constructed with a runner of its
own set to an hour. Sharing one would mean choosing between a pointless wait on
a failed `useradd` and an update killed half-way through.

### Reboots are reported, never taken

Nothing in this domain restarts anything.

Whether a restart is outstanding is computed by comparing the running kernel
release against the newest installed one. That needs no package-manager plugin
— `needs-restarting` lives in a package an appliance has no other reason to
carry, and a node that lacked it would silently report "no reboot needed",
which is the dangerous direction to be wrong in.

The restart itself goes through the existing power route, which already refuses
to take down a node its cluster cannot spare and already points the operator at
maintenance mode, where the machines are moved off first. The Updates page
links there rather than growing a second, weaker copy of that guard.

## Where the packages come from

`lumen-repo` ships `/etc/yum.repos.d/lumen.repo` and the public signing key.
Repository configuration is its own package rather than part of
`lumen-release`, for the same reason the distributions split theirs: the
address an appliance fetches from and the identity it reports are unrelated
decisions, and a site running its own mirror replaces one and not the other.

```
https://lumen.quartz.systems/repo/stable/$releasever/$basearch/
```

That address is a name Quartz Systems owns, and that is the whole point. It is
baked into every appliance ever installed and dnf will not follow a redirect to
a new one, so the only way to move the repository later is to repoint DNS. A
`github.io` address would have married the product to GitHub Pages
permanently; this way Pages is merely today's backend.

Both `gpgcheck` and `repo_gpgcheck` are on. They are different claims: a
package signature says *Quartz Systems built this*, and an index signature says
*and this is the current set of them*. Without the second, someone who took the
hostname could serve a genuine but superseded package in place of a fixed one.
`skip_if_unavailable=1` is set so that one unreachable host does not turn every
`dnf` invocation on the node into a failure.

### Generating the signing key

`lumen-repo` cannot be built without the public half, and
`packages/build-rpms.sh` skips that one package — loudly — when it is absent,
so a contributor can still build and test everything else without being handed
key material.

```
gpg --batch --full-generate-key <<'EOF'
Key-Type: RSA
Key-Length: 4096
Name-Real: Quartz Systems Lumen
Name-Email: engineering@quartz.systems
Expire-Date: 5y
Passphrase: <choose one>
EOF

# The public half is committed; it ships to every appliance.
gpg --export --armor engineering@quartz.systems \
    > packages/keys/RPM-GPG-KEY-lumen

# The private half goes into the repository secrets and nowhere else.
gpg --export-secret-keys --armor engineering@quartz.systems
```

Store the private half as the `LUMEN_GPG_PRIVATE_KEY` secret and its passphrase
as `LUMEN_GPG_PASSPHRASE`. Nothing in the checkout should ever contain it, and
a release that reaches the repository unsigned is a release no appliance can
install — which is why both workflows fail loudly rather than continuing when
the secret is missing.

## How the repository is published

`release.yml` signs every package at the moment it is built, verifies each one
against the public half exactly as an appliance would, and attaches them to the
GitHub release. `pages.yml` then assembles the site: the landing page, the
public key, and a repository built from the most recent releases' assets, with
`createrepo_c` writing the index and `gpg --detach-sign` covering it.

Nothing built is committed. Committing RPMs would grow the git pack by tens of
megabytes per release, permanently, to store bytes that already exist as
release assets — and the Pages "deploy from a branch" mode, which can only
publish the repository root or `/docs` and only committed content, is the
reason the Actions deployment is used instead.

The published site is bounded: Pages serves about a gigabyte, a release is
roughly 35 MB of packages, so the newest few are mirrored and older ones stay
on the releases page they came from. dnf needs the current build and, for a
downgrade, the one before it. If that ever stops being enough, the escape hatch
is `createrepo_c --location-prefix`, which writes absolute package addresses
into the metadata so the index can live on Pages while the packages stream from
the releases page; it needs one run per release tag and a `mergerepo_c` pass,
which is why it is not the starting point.

## API

Every route needs a session.

```
GET    /api/system/updates            what was last found (never asks the network)
POST   /api/system/updates/check      ask the repositories now
POST   /api/system/updates/apply      start installing -> 202 + the job
GET    /api/system/updates/progress   the job, running or finished (null if none)

GET    /api/environment/updates            every member's answer, concurrently
POST   /api/environment/updates/check      every member asks its repositories
POST   /api/environment/updates/apply      start the walk -> 202 + the walk
GET    /api/environment/updates/progress   the walk, running or finished
```

The environment set is separate routes rather than a widening of the node
ones, for the reason [cluster.md](cluster.md) gives about the inventory read:
`/api/system/...` means *this appliance*, and every write already on that
prefix is written against that meaning.

The read and the refresh are separate routes on purpose. Opening the console
must not block on repository metadata over a link that may be slow or absent,
and a node whose repository is unreachable still has to render the page —
carrying the reason the last check failed, and the outstanding-restart notice
it read from the node itself, which is often exactly what such an operator
needs to see.

`apply` answers `202` because a transaction runs for minutes. It validates
synchronously first, so a refusal — nothing waiting, the set will not resolve,
the acknowledgement is missing — arrives on the request the operator made
rather than as a job that fails a moment later.

### There are no steps in the progress feed

A drain publishes one step per machine because it moves them one at a time and
knows which one it is on. A transaction is not like that: the package manager
is handed one set, and nothing it prints comes back through the privileged
runner until it has finished. Per-package steps marked done at the end would be
a progress bar that lies. The console gets what is honest instead — whether it
is running, how long for, and afterwards what changed, what the package manager
said, and whether a restart is now outstanding.

## Web UI

`System → Updates`, written from the environment down rather than from this node
out. An operator asking what is waiting is asking about the appliance they run,
which is every node in it; a page that answered for whichever node happened to
be serving the console made them visit each one and assemble the real answer
themselves.

**Every Node** is the first panel: one row per member with what it has waiting,
and the two environment-wide buttons. It is there on a single appliance too —
`GET /api/environment/updates` answers with this node alone when there is no
environment, so the same table renders either way and the one-node case needs no
second code path.

Below it are the two decisions, never joined into one button, and both tables
are the whole environment's. One row is one package at one version naming the
nodes waiting on it, so a package waiting everywhere reads as one line rather
than as one line per node. Rows are keyed by what would be installed *and* what
is installed now — a node two versions behind is not the same fact as a node one
behind, and merging them would quietly hide the one that is further back. The
counts in `ClusterCounts` are deliberately not built this way and are not to be
reconciled with these tables: those count the work, these list what the work
installs.

**Kernel and Storage Modules** carries the solver's verdict for the nodes that
are blocked — the solver's own words on each node's badge, plus a sentence
explaining that this is ordinary for a few days after a point release — and
normally offers no install button at all. That set takes effect only on a
restart, so it moves through a rolling update rather than being installed
everywhere and running nowhere.

The exception is a node that has not joined a cluster. A rolling update has
nowhere to move its machines, refuses to take it, and would leave it with no way
to take its kernel from the console at all — so that node, and only that node,
installs its platform set from this panel and restarts from Maintenance. The
console decides that from `GET /api/environment`, by whether this node is in
`unassigned`, which is exactly the condition `visiting_order` refuses the roll
for.

The acknowledgement in front of that install is a checkbox rather than typing
the node's name — a deliberate difference from the restart dialog. Installing
the packages is not the dangerous step; the node carries on running its current
kernel afterwards. The restart that makes them live has its own confirmation,
its own quorum guard, and its own drain.

While a walk is running, a member's row shows its step rather than its own
answer — during a walk the step is the more current of the two, and it is what
lets a member that has stopped answering read as *restarting* rather than as a
plain failure. The badge is the member's **stage**, so "moving machines off"
and "coming back" are legible as progress rather than as a stall.

The two buttons get the two dialogs their consequences deserve. *Update All
Nodes* has no checkbox: nothing there replaces a kernel and nothing restarts. It
says the order, that a failure stops the rest, and that the console the operator
is reading it on belongs to the node that goes last. *Rolling Update* does have
one — the same acknowledgement the backend requires — because that is where
machines are interrupted and nodes go down. It names the nodes that will
restart, says that nodes with only userland updates are not taken down for them,
and calls out the coordinating node before the operator starts rather than
leaving it to be discovered at the end.

### Every install is written down

A transaction that has finished leaves an entry in the node's own log — the one
`crate::tasks` keeps, which until now held only what was asked of machines. An
update belongs to no machine, so `TaskRecord::vmid` became optional and
`TaskLog::event` is how something that happened *to the node* is recorded; a
record already on disk carries its `vmid` and keeps meaning what it meant.

One entry, written when it is over rather than when it begins. A transaction
that is running is already on this page, with its elapsed time and the node it
is on; what a log is for is the question asked afterwards — what was installed
here, when, and by whom — and that has no answer until the package manager has
one.

An environment-wide update writes one more, on the node that drove it, saying
the thing no member can say for itself: that somebody updated the environment,
and how it ended. Each member has already recorded its own install, because the
peer apply runs through that member's own `crate::updates::begin`. The walk's
entry is deliberately written *before* the refusal is published, because the
log outlives the record: the walk's progress lives in one process's memory and
the log is on disk, so this is what an operator still has after the coordinator
restarts itself.

`System → Logs` is where they are read, every member's at once — see
`GET /api/environment/tasks`.

---

## Updating every node

`src/cluster_updates.rs`. Reading is a concurrent fan-out; **installing is a
walk** — one member at a time, in a fixed order, this node last.

### Why a walk and not a fan-out

Three reasons, in order of what they cost when ignored:

1. **A failure repeats.** A transaction that fails on one node usually fails on
   the next for the same reason — a mirror that has gone away, an index signed
   with a key the nodes do not have, a package that will not resolve. Pressing
   on turns one node that did not update into six. So the walk stops at the
   first failure, names the member, and says what finished before it.
2. **Lumen's own packages restart the control plane.**
   `%systemd_postun_with_restart` in `packages/lumen-controlplane.spec` is not
   incidental — it is how a new console reaches an operator. Six consoles
   restarting at once is an environment with no console at all.
3. **It is the shape a rolling update needs anyway.** Drain, install, restart,
   wait for rejoin, next is this same walk with more steps per member.

### This node goes last

Sharpening reason 2: the node running the walk is the one driving it. Updating
itself first would restart its control plane, and the walk — which lives in
that process's memory — would stop with members still untouched.

Which means the walk's own record is **not** the source of truth, and this is
not worked around because it cannot honestly be: there is no database here, and
a record that claimed to survive a process that did not would be worse than one
that plainly does not. What survives is better — every member knows what is
installed on it, and `GET /api/environment/updates` asks all of them. The
console reads the truth from the nodes rather than from a job record, the same
discipline the node-local feature already applies to the reboot state.

### A member that stops answering is not a member that failed

It follows that "did that member finish?" cannot be answered from its
transaction feed alone. A member installing `lumen-controlplane` restarts the
daemon holding that feed and comes back reporting no transaction at all.
Treating that as a failure would fail every member that updated Lumen — which
is the whole point of the feature.

So a member is watched two ways and either answers:

| what the member reports | what it means |
|---|---|
| its transaction, `complete` | finished, and it says what changed |
| its transaction, `failed` | failed, and it says why |
| stopped answering, then no transaction at all | its control plane restarted — ask the package database |
| nothing left waiting afterwards | the transaction did what it was asked |
| packages still waiting afterwards | it did **not** finish; the walk stops and names them |

The last row is the one that matters: a missing feed is not permission to
assume success. `dnf` runs as a transient systemd unit and outlives the process
that started it, so the evidence of what happened is the package database, and
that is what is read.

### The kernel does not move without a restart

`POST /api/environment/updates/apply` with `platform: true` and no `rolling`
is refused outright. The platform set does not take effect until a node
restarts, and a kernel installed on every member and running on none is a
cluster that looks updated and is one power cut away from finding out it is
not.

## Rolling updates

`{"rolling": true, "i_understand_each_node_restarts": true}`. The same walk
with more stages per member:

```
check → drain → install (ordinary, then platform) → restart → rejoin → back into service
```

Ordered so nothing irreversible happens before the reversible parts have
succeeded. The node is emptied before anything is installed, so a drain that
strands a machine costs nothing but a return to service. The ordinary updates
go in before the kernel, so a mirror that fails has not left a half-moved
platform set. The restart is last, because it is the only step that cannot be
taken back.

### It rolls the nodes that need rolling

A rolling update does not drain every member on principle. From that member's
own fresh check:

| what the member has | what it gets |
|---|---|
| platform waiting (and it resolves), or a restart already owed | the full cycle |
| only ordinary updates | installed in place — nothing drains, nothing restarts |
| nothing | stepped over |

Taking a node down to install `openssl-libs` would be a cost with no purpose.
The second row of that table is also why an *outstanding* restart counts:
that is what a cluster looks like the morning after somebody installed the
platform set on every node by hand.

### Three things must be true before the next node goes down

`await_rejoin` waits for all of them, and each rules out a different way of
being wrong:

- **it answers** — the daemon is up;
- **it reports no outstanding restart** — the new kernel is the one running,
  which is the entire reason it was rebooted, and catches a node that came up
  on its old kernel;
- **this node's cluster view has it online** — corosync has it back, so the
  next member can go down without the cluster losing two at once.

The third is read from the coordinator rather than asked of the member,
deliberately: whether the *rest of the cluster* can see it is the question that
matters, and a node cannot answer that about itself. A cluster that cannot be
read answers "not online" and the deadline decides — "the cluster could not be
asked" is not a yes to "may the next node go down".

### The one node it will not restart

The node running the update. A control plane cannot drive a rolling update
through its own reboot, and both alternatives are worse than the limit: handing
coordination to a peer mid-walk is a distributed hand-off with its own failure
modes, and rebooting anyway leaves a node out of service with nothing running
to put it back.

So the walk finishes with that node named, plainly `pending`, and a sentence in
`left_to_you` saying what to do — either Maintenance by hand, or **run the same
update again from another member's console**, where every other node is already
current and this one is the only work left. Two passes, no new machinery, and
each pass is the operation that was already tested.

The walk is `complete` rather than `failed` when this happens: it did
everything it set out to do, and a red banner would train operators to ignore
one.

### What stops it

A rolling update stops at the first member that fails, and puts that member
back into service on the way out — leaving a node deliberately idle because an
update failed is a state nobody asked for and nobody would find.

- **The platform set will not resolve there.** Checked on the member, against
  the member's own repositories. This is the failure the whole domain exists to
  prevent, and a rolling update is the one place it would be automated onto
  every node in turn.
- **The node could not be emptied.** A machine that would not move is named.
  Restarting a node with machines still running on it is precisely what
  draining is for.
- **The member refused the drain.** Its own quorum guard said no. The
  coordinator does not second-guess it.
- **It came back still needing a restart.** Restarting it a second time would
  be guessing.

### The peer surface

```
POST   /api/peer/system/updates          what this member has waiting, + its transaction
POST   /api/peer/system/updates/check    ask this member's repositories now
POST   /api/peer/system/updates/apply    start installing here -> the transaction
POST   /api/peer/system/maintenance      out of service, and drain
POST   /api/peer/system/maintenance/progress   the drain, while there is one
POST   /api/peer/system/maintenance/exit       back into service
POST   /api/peer/system/restart          restart now
```

The last four are deliberately **not** a peer copy of the operator-facing
maintenance routes, which still refuse to act on any node but their own ("its
machines can only be moved by the node running them"). That rule is unchanged
and is why these exist: the work happens on the node it is about, through the
same `crate::maintenance` entry points its own console calls. What crosses the
wire is only the instruction to begin.

`/api/peer/system/restart` applies the same quorum guard the operator-facing
power route does, and it is called there rather than trusted to the caller. A
rolling update reaches it having already put the node into maintenance, which
is one of the three documented ways past that guard — so the guard passing is
evidence the sequence was followed, not a formality skipped. There is no
acknowledgement to override it with; an operator who wants to take down a node
their cluster cannot spare can still do it on that node's own power page, where
they are the one being told what it costs.

The acknowledgement is not taken from the wire, exactly as `create_pool` and
`wipe_disk` do not take theirs: consent was given to the console the operator
is looking at. The operator's principal *is* carried, and the difference is the
point — that is a label for the journal, not a permission, and without it a
member's own record would name only the node that relayed the request.

Every guard that matters stays the member's. Whether the platform set resolves
*there* is a question only that node's package manager can answer, and the
coordinator's belief about it carries no weight.

## Periodic checking

The control plane asks its repositories every six hours (`
LUMEN_CP_UPDATE_CHECK_SECS`, `0` to turn it off). Nothing is ever installed by
it. It exists so the console can show what is waiting without every page load
making a network request, and so a security advisory published in the morning
is visible to an operator who was not looking for one.

It is a task in the daemon rather than a systemd timer for the same reason the
membership gossip and the HA sweep are: the answer is wanted in this process's
memory, and a timer would need somewhere to write it down and a second thing to
keep in step.

## Out of scope for this stage

- **Air-gapped updates.** The source is a repository over the network. An
  uploaded, signed bundle for appliances with no outbound path is a second
  backend behind the same trait, and the trait was shaped with that in mind.
- **Signing the ISO's on-media repository.** `iso/build-live-iso.sh` still
  builds its `lumen` repo with `gpgcheck=0` from packages it has just built
  locally. Now that a signing key exists, that repo should be signed and the
  gate turned on; it is a change to the ISO build, which is why it is not
  bundled with this one.
- **Rollback.** `dnf history undo` exists and works, but an appliance that
  offers it also has to reason about what a rolled-back control plane does to
  a cluster that has moved on. Restoring from a snapshot is the supported
  answer for now.
