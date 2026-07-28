# Lumen storage

Two domains share this directory. `lumen-zfs` is the node's own storage —
pools, datasets, and the local volumes a machine's disks live on — and has
carried the appliance since the beginning. `lumen-drbd` is this stage's
addition: **replicated volumes**, one synchronously replicated block device
per volume, built from a DRBD 9 resource over a thick zvol on each of 2–3
members of one cluster. This document covers the replicated half; the
topology decisions it builds on (regimes, quorum, fencing) are
docs/cluster.md's and are not repeated here.

```
lumen-storage/
├── lumen-zfs/    pools, datasets, local zvols (existing)
├── lumen-cluster/ membership, quorum, fencing — and the volume records
└── lumen-drbd/   replicated volumes: DRBD 9 resources over zvols
```

## Component split

```
model.rs    names, byte-exact sizing, port and minor allocation
render.rs   the resource file, rendered whole from the record
state.rs    what `drbdsetup status --json` actually reports
peers.rs    what one control plane asks another to do to its replicas
backend/    the supported command line (cli/), plus mock/ and unavailable/
service.rs  the one entry point the control plane calls
```

The stored shape — `VolumeRecord` — deliberately lives in `lumen-cluster`,
riding the environment's membership record. Volume placement is membership's
business: a node holding replicas cannot leave its cluster, and every
console must describe every volume whether or not its cluster answers. What
lives here is everything operational about one. The dependency direction is
`lumen-cluster ← lumen-drbd` and `lumen-zfs ← lumen-drbd`: the record and
the replication policy come from below, the backing zvols go through the
storage domain — this crate never runs a `zfs` command — and the service is
the only place the three meet.

---

## Decisions

### The volume is sized byte-exactly, before anything exists anywhere

DRBD requires the backing devices to agree on size, and "roughly the same"
is a resync waiting to happen. So the backing size is computed **once**, on
the coordinator, before anything is created on any node: the usable bytes
the operator asked for, plus DRBD's internal metadata (the bitmap grows with
the device and the peer count, so the answer is a fixed point, reached in a
round or two), rounded up to the 16 KiB volblocksize every VM disk already
uses. Every member receives that exact number in its prepare payload —
identical replicas by construction, not by reconciliation. The sizing
invariants are tested across sizes and replica counts in
`model::tests::the_backing_size_carries_the_data_the_metadata_and_nothing_less`.

### Thick zvols, never sparse

A sparse backing device turns "the pool filled up" into "one replica started
failing writes the others accepted" — divergence manufactured out of thin
provisioning. Backing zvols are thick (`zfs create -V`, no `-s`): the space
is genuinely reserved at creation, and a pool without room refuses the
volume up front instead of corrupting it later. Thin provisioning under
replication is a stated non-goal.

### auto-promote, and Pacemaker manages no promotion

The resource carries `auto-promote yes`: the device promotes to Primary when
libvirt opens it on the node starting the machine, demotes when closed, and
DRBD itself refuses a second writer. There is no Pacemaker master/slave
resource, no promotion constraint, no agent — a VM volume's writer is
decided by who opened it, which is exactly the fact libvirt already owns.
What makes that safe is the topology regime's policy, rendered into the
resource file by the engine that owns the regimes: two-node volumes carry
`fencing resource-and-stonith` with the crm-fence-peer handlers (a lost peer
suspends I/O until it is outdated through Pacemaker), and three-replica
volumes in the quorum regime carry `quorum majority` + `on-no-quorum
suspend-io`. Two-replica volumes in a quorum cluster carry neither — the
cluster's own quorum and fencing protect them. Integrity over availability,
always, and `render.rs` is tested for all three shapes.

### The initial sync is skipped, deliberately

A fresh zvol reads as zeros on every member — ZFS returns zeros for blocks
never written, thick or not. Copying those zeros across the Core network for
hours to make two identical devices "identical" would be ritual, not
engineering. So after every member is up, the workflow runs `drbdadm
new-current-uuid --clear-bitmap` once: the current (empty) state is declared
the one true state, and the volume is usable the moment the create answers.

### The replication secret travels like the authkey

Each resource file carries a fresh shared-secret (`cram-hmac-alg sha256`),
generated at create and pushed to each member inside the rendered file over
the peer channel — the corosync authkey's exact path. It is deliberately not
in the volume record: the record is gossiped to every environment node, and
a gossiped secret is a distributed one. The file lands in `/etc/drbd.d/` as
the standard input of `install -m 0600 /dev/stdin` — content over the pipe,
nothing secret in argv, the rule since docs/system.md.

### The record is written last, again

A volume create prepares each member whole — zvol, resource file, metadata,
up — then primes, **then** writes the record through the cluster domain. Any
failure unwinds exactly the touched members and the record never knew the
volume existed; a destroy requires every replica to clean up before the
record forgets it, or it reports which member is stuck and changes nothing.
The same finishes-or-never-happened rule as a cluster create, pinned by
`a_failed_create_unwinds_the_touched_members_and_records_nothing`.

### Twelve volumes per cluster, and the number is honest

Replication rides TCP ports 7788–7799 on the Core network — the exact range
the firewalld service opens there — allocated per cluster, first free port
wins. Twelve ports is twelve volumes per cluster: a v1 ceiling that matches
the appliance's scale (a handful of VMs per cluster, one or two disks each),
stated here rather than discovered in production. Resource names are
prefixed with the cluster (`<cluster>-<volume>`), so two clusters sharing a
management network never collide in anyone's tooling or logs. Minors are
per-cluster too, which is enough — a node is in at most one cluster.

### Rate from two snapshots, not an event stream — for now

Replication state is read from `drbdsetup status --json` — the
machine-readable form, parsed against captured fixtures — on the console's
ordinary poll, and the resync rate is derived from the counter delta between
two reads, which is all a progress pill needs. `drbdsetup events2`
streaming is deliberately deferred to the stage that first needs to *react*
to a state change rather than render it (the HA manager); wiring a
long-running event follower into the daemon to save a poll the console
makes anyway would be machinery ahead of its need.

One honesty rule, same as unreachable clusters: a node only sees the
resources it participates in, so a console on a member with no replica says
"the state is known on beta-1, beta-2" rather than guessing — and a DRBD
that cannot be asked at all leaves the volumes listed from the record, each
carrying the reason.

### A machine's replicated disk is the same document everywhere

The compute domain consumes replicated storage through one deliberately
narrow trait (`lumen_drbd::VmVolumes`): make a disk, recognise one, destroy
one, know where the machine can run, hold the two-primaries window. It never
sees records, renderers, peers, or ports — and its tests run against an
in-memory implementation of the same five verbs.

Two rules the interface enforces rather than assumes. **The machine's own
node always holds a replica**: `/dev/drbd<minor>` exists only where a
replica does, so a placement that skips the machine's node is refused, not
accommodated with a diskless client. And **the volume's identity is not
written into the domain document**. The spec called for identity and
membership in `<metadata>`, and that is deviated from deliberately: the
membership record already carries both — gossiped, durable, and readable
when a node is dead, which is exactly the property HA needs — and
`/dev/drbd<minor>` is stable on every member, so the record is resolved from
the device path and a second copy would be a second source of truth to keep
honest. The same stability is what makes one domain document valid wherever
the machine runs.

### The two-primaries window opens for one migration and closes on every path out

DRBD's one deliberate exception to "one writer" is a live migration: for the
moment of the handover, source and destination both hold the device open. The
window (`--allow-two-primaries`) is opened on every member just before the
migration and closed on **every** path out — the guard is two loops around
one backend call, not a flag something remembers to reset. If opening fails,
the migration is never started. If the migration fails, the window closes
and the machine is exactly where it was. If the migration succeeds but a
close fails, the operation reports an error *demanding* the close — a
successful move with an open window is more dangerous than a failed move —
and `a_migration_holds_the_window_for_exactly_as_long_as_it_takes` and its
failure-path siblings pin all of it.

The transfer itself is libvirt's: peer-to-peer to
`qemu+tcp://<core-address>/system`, persistent on the destination, undefined
at the source — when the call answers, the machine has one home. The memory
rides the cluster's Core network, the same dedicated link its disks already
replicate over; Management keeps carrying the console. A migration is
refused, by name, for a machine with a local zvol disk, attached
installation media, no disks at all, or a target outside the replica set.
(Making libvirt listen on the Core network is packaging's business and lands
with the packaging stage.)

### Definitions replicate at define time, because a dead node cannot be asked

An HA restart needs the machine's domain document on the survivor, and
libvirt on the dead node is exactly what stopped answering. So every define
— create, update, attach, detach — pushes the stored document to the
cluster's other members, kept under the environment state directory
(`<state_dir>/environment/definitions/<vmid>.json`; the spec named
`/var/lib/lumen/`, deviated from because the state directory is writable
without a transient unit and already holds the environment's other
replicated state). The stored shape is the XML plus the machine's **home
node** — re-set at every define, so a migration or an HA adoption re-homes
the definition on every member without a separate bookkeeping step. The
push is best-effort beyond the local copy: a peer that is down misses it
and is caught up by the next define, and failing an operator's action over
HA prep would make the preparation more important than the machine. A
delete withdraws the definition everywhere — a stored definition for a
machine that no longer exists is a machine waiting to be wrongly
resurrected.

### HA acts only on a fence-confirmed death, and nobody coordinates the election

The HA manager is a 15-second sweep inside each control plane, not a
Pacemaker resource and not a daemon of its own. Its rule for "dead" is
exactly the cluster's: a node that is offline **and clean** has been fenced
(or break-glass confirmed) — Pacemaker has vouched nothing there is still
writing. A node that is *unclean* is lost but not yet fenced, and the sweep
waits: restarting a machine whose old copy may still be running is the one
mistake this design exists to make impossible. The sweep also requires the
cluster quorate from where it stands — a minority partition restarting
machines is the same mistake wearing quorum's clothes.

Which survivor restarts a machine is decided without any coordination:
every member runs the same sweep over the same gossiped record, computes
the same eligible set — survivors holding **all** of the machine's replicas
— and the lowest-named member elects itself; the rest do nothing. No
leader, no lock, no message. If two nodes ever did race through a stale
view, DRBD's refusal of a second writer on an auto-promote device is the
backstop: the loser's start fails, the machine runs once. A machine
restarts only if its **HA flag** is set (a per-machine choice on the
Options page, off by default, carried in the domain document's metadata),
only from a replicated-disk definition, and lands in the task log as
`ha-restart` by `ha@lumen` — the events trail the spec asked for, in the
log the console already has. There is deliberately **no automatic
failback**: the machine has a working home; moving it again is an
operator's call, made with the live-migration button that already exists.

### Snapshots are per-member zvol snapshots, taken all-or-none

A replicated volume's snapshot is a ZFS snapshot of the backing zvol on
**every** member, taken by one workflow that either lands everywhere or is
unwound where it landed — a snapshot set missing a member is not a partial
success, it is a rollback source that may not exist when needed. The list
the console shows is deliberately this node's own vantage (the snapshots
its own replica holds), because a snapshot set is per-member and pretending
to a merged view would hide exactly the asymmetry a rollback forces the
operator to think about.

Rollback is a transaction with one source of truth: refused while the
device is open anywhere (the machine must be off — rolling back under a
live guest is corruption by design), the resource is taken down on every
member, **one** member's zvol is rolled back (`zfs rollback -r`, later
snapshots on that member go with it — ZFS's own rule, surfaced in the
dialog rather than hidden), every member comes back up, and the source
invalidates its peers so DRBD resyncs the rolled-back state outward. The
"up everywhere" step runs even when the rollback failed, because a volume
left down everywhere is an outage the operator did not ask for.

### Split-brain recovery reconnects the victim first

DRBD's refusal to merge divergent histories is correct — someone has to
decide which history survives, and that someone is the operator. The
guided recovery takes the victim's name (typed back, with the
acknowledgement), reconnects the **victim first** with `--discard-my-data`,
and the survivors after: a victim reconnected last would sit StandAlone
waiting for peers that already gave up on it, and the order is the
difference between a recovery and a second outage. The victim's divergent
writes are gone for good; that is the point, and the dialog says so in
those words.

### A volume only grows

`zfs set volsize` can shrink, and a shrunk block device under a running
guest is data loss with extra steps, so the refusal lives at every layer:
the drbd service, the zfs service, and the console's dialog, which only
offers growth. A grow runs every member's backing zvol first, the resource
once after all of them, then the record — a failure partway leaves some
zvols with spare room and the volume untouched, so the operator simply
retries.

### A regime change re-renders every resource, and the secret never travels

When a cluster leaves the two-node regime (the 2→3 scale-out,
docs/cluster.md), every existing volume's replication policy is wrong: the
resource files carry `fencing resource-and-stonith` where volume quorum
should be. The flip re-renders each resource from the record under the new
topology policy and applies it live with `drbdadm adjust` — no down, no
resync, no I/O interruption. The one thing the coordinator cannot render is
each file's shared-secret, which exists only in `/etc/drbd.d/` on the
members — deliberately, see above. So the re-rendered file travels with a
placeholder where the secret belongs, and **each member substitutes its own
secret from its existing file** before writing. The secret never crosses
the wire a second time, and the placeholder pattern is pinned by the
refresh tests.

---

## API

All routes require a session and use the standard error envelope. Volumes
are grouped by cluster — the repo's grouped-by-node shape one level up,
matching `/api/environment`.

| Method | Path | Purpose |
| ------ | ---- | ------- |
| GET | `/api/storage/replicated` | Every cluster's volumes: definition, replicas, health pill, sync progress |
| POST | `/api/storage/replicated` | Create — members chosen (or least-utilized default), record written last |
| DELETE | `/api/storage/replicated/:cluster/:name` | Destroy every replica; `i_understand_this_may_lose_data` required |
| POST | `/api/storage/replicated/:cluster/:name/resize` | Grow only |
| GET | `/api/storage/replicated/:cluster/:name/snapshots` | This node's own replica's snapshots |
| POST | `/api/storage/replicated/:cluster/:name/snapshots` | Snapshot every member, all-or-none |
| DELETE | `/api/storage/replicated/:cluster/:name/snapshots/:snapshot` | Drop the snapshot on every member |
| POST | `/api/storage/replicated/:cluster/:name/rollback` | The transactional rollback — source member named, `i_understand_this_may_lose_data` required |
| POST | `/api/storage/replicated/:cluster/:name/resolve-split-brain` | The guided recovery — victim named, `i_understand_this_may_lose_data` required |
| POST | `/api/vms/:vmid/migrate` | Live-migrate to another member of the disks' replica set, under the two-primaries guard |

Machine disks gain the replicated shape on the existing routes: a
`DiskCreate` with `"replicated": true` and its member seats, on both
`POST /api/vms` and `POST /api/vms/:vmid/disks`. The HA flag is `"ha"` on
the same machine create/patch shapes.

The peer surface gains `/api/peer/volume/{prepare,prime,teardown,`
`resize-backing,grow,two-primaries,snapshot,rollback-backing,`
`drop-snapshot,down,up,invalidate-remote,reconnect,apply-policy}` and
`/api/peer/definition/{store,drop}` — peer-ticket authenticated like the
rest of `/api/peer`, never for a browser.

## Web UI

**Storage** keeps its pools exactly as they were and gains **Replicated
Volumes** below them — present only when the node is in an environment with
clusters, absent (not empty) otherwise. The table carries the state pill
(`UpToDate` / `Syncing 42.7%` with rate / `Outdated` / `Suspended — no
quorum` / `StandAlone`), the stable device path, and per-replica dots with
each side's disk state. Create is a dialog: cluster, name, size, and the
replica seats — preselected on a two-node cluster, because there is exactly
one possible placement and asking would be ceremony. Destroy is the
typed-name confirmation; the volume's own name, because replication does not
protect against the operator.

Each row carries the snapshots dialog — the local replica's list, taking
one (on every member at once), deleting one, and the guided rollback with
its source-member choice and acknowledgement inline under the chosen
snapshot. A volume reading `StandAlone` gains the split-brain recovery
action beside it: victim chosen, typed back, acknowledged. The machine's HA
toggle lives where the machine's other options do — the VM's Options tab —
not here; a volume does not know which machines sit on it.

## Development

```sh
make test    # installer + six domain crates + control plane
make lint    # shellcheck, rpmlint, fmt/clippy for eight manifests
```

`lumen-drbd` links nothing and needs no DRBD anywhere near the tests: the
whole lifecycle — create with unwind, destroy, grow, placement defaults —
runs against `MockVolumePeers` and the mock backends, and the status parser
is tested against captured `drbdsetup status --json` shapes (healthy,
mid-resync, quorum lost). The control plane's `tests/volume_flow.rs` drives
the real router over the same mocks.

What in-memory tests cannot cover is DRBD itself replicating — that is the
two-node manual test: create a volume, write on one node, read it on the
other, pull the Core cable and watch I/O suspend rather than diverge.

## Clearing a disk

`zpool create` refuses a disk that already has something on it, and the
picker refuses it first, with what is on it in words. That is right, and
until recently it was also a dead end: a disk carrying nothing but an old
partition table reads as spoken for, and the only way out was a shell on the
node. **Storage → Disks** is the way out — every member's disks in one
table, and a clear on the ones that can take it.

The offer is made in exactly one state: something is on the disk, and
nothing live is using it. Two facts decide that, and they come from
different places:

- **`/proc/mounts` and `/proc/swaps`**, through the same `/sys` scan that
  fills the picker. A disk with a mount or swap on it — or on any of its
  partitions — is claimed, and no acknowledgement overrides that.
- **`zpool list -vHP`**, which is the half the scan cannot answer. A live
  pool's members carry a couple of partitions and appear in no mount table,
  so they look *exactly* like a disk somebody finished with. Only `zpool`
  knows, so it is asked; a `zpool` that cannot be asked leaves every disk
  unclearable rather than every disk clearable. The console loses a button;
  the alternative loses a pool.

What the clear does is remove the labels, not the data: `zpool labelclear`
on every partition and the disk (ZFS keeps four labels per device, two at
each end, which no signature scanner finds all of), then `wipefs -a` over
the same list, then `blockdev --rereadpt`. Partitions first and the disk
second — clearing the table first makes the partitions vanish as devices
and strands the labels on them. util-linux only, so no new package
dependency. The console says as much in the dialog: the blocks stay until
something writes over them, and what changes is that the node now reports
the disk as empty.

It works across the environment. `POST
/api/environment/nodes/{node}/disks/{disk}/wipe` reaches the owning member
through the peer channel, and the acknowledgement deliberately does not
travel with it — consent was given to the console the operator is looking
at, and a peer route that accepted "yes, erase it" from a body would be a
second, quieter way to clear a node's disks. Every guard is still the owning
node's, because it is the only thing that can see what is actually on the
disk.

## Out of scope

The replicated-storage program is complete: the HA manager, snapshots and
the transactional rollback, the guided split-brain recovery, the 2→3
scale-out with its policy flip, and the packaging (ELRepo kmod-drbd9x,
kABI-gated exactly like kmod-zfs; the HighAvailability stack; presets that
keep every cluster daemon off until a cluster exists) have all landed. One
loose end is deliberate: the migration URI assumes libvirt listening on the
Core network, and the packaging ships the firewalld service definition for
it but does **not** enable the listener — virtproxyd's TCP authentication
is its own security decision, and turning it on silently for every
appliance would make it nobody's. No external storage export, no thin
provisioning under replication, and no scale past 3 replicas — stated
non-goals, not omissions.

The last of those is where the model ends rather than where it fails.
Pooled capacity across a cluster, placement without naming members, and
volumes larger than any one member are a different architecture, not an
extension of this one; docs/storage-scaleout.md sets out what each of those
three actually costs and why the answer differs either side of two nodes.
