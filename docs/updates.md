# Lumen updates

How an installed appliance gets newer software: where the packages come from,
what may be installed without asking, and the one thing that must never happen
by accident.

```
lumen-system/
└── lumen-update/                   the update domain (Rust library crate)
lumen-controlplane/
├── src/api/updates.rs              thin HTTP handlers over lumen-update
└── src/updates.rs                  the transaction as a watchable job
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
that tracks the kernel ABI. So is DRBD. `iso/pins.env` already records what
that means for the ISO build — *"Kernel, kmod-zfs, and kmod-drbd9x move
together as one pinned set: bump one, re-verify all three"* — and notes that
the modules have a real history of lagging AlmaLinux point-release kernels.

A node that ran an unguarded `dnf upgrade` would, sooner or later, install a
kernel with no matching `kmod-zfs`, reboot, and fail to import its root pool.
There is no console to fix that from. It is a drive to the rack, and it would
happen to whichever customers happened to press the button during the window
where the kernel had shipped and the module had not.

So the domain splits every pending update in two:

| | what it is | how it is installed |
|---|---|---|
| **Ordinary** | Lumen's own packages, and userland | one button, no acknowledgement |
| **Platform** | `kernel*`, `kmod-*`, `zfs*`, `drbd*` | together or not at all, and only when the solver says they resolve |

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
```

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

`System → Updates`, two panels for the two decisions, never joined into one
button. The platform panel carries the solver's verdict: a green note when the
set can move, and when it cannot, the solver's own words plus a sentence
explaining that this is ordinary for a few days after a point release.

The acknowledgement in front of a platform install is a checkbox rather than
typing the node's name — a deliberate difference from the restart dialog.
Installing the packages is not the dangerous step; the node carries on running
its current kernel afterwards. The restart that makes them live has its own
confirmation, its own quorum guard, and its own drain.

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

- **Cluster-wide rolling updates.** Updating a cluster one node at a time —
  drain, install, restart, wait for rejoin, next — is the natural next step,
  and every piece it needs already exists (maintenance mode, the drain, the
  quorum guard, peer calls). It is deliberately not here: it is a distributed
  reboot orchestration, and shipping one that has never run against a real
  cluster would be worse than not shipping it. Today an operator updates a
  cluster by doing one node at a time through Maintenance, which is the same
  sequence with a human driving it.
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
