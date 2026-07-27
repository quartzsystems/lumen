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

### A volume only grows

`zfs set volsize` can shrink, and a shrunk block device under a running
guest is data loss with extra steps, so the refusal lives at every layer:
the drbd service, the zfs service, and the console's dialog, which only
offers growth. A grow runs every member's backing zvol first, the resource
once after all of them, then the record — a failure partway leaves some
zvols with spare room and the volume untouched, so the operator simply
retries.

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

The peer surface gains `/api/peer/volume/{prepare,prime,teardown,`
`resize-backing,grow}` — peer-ticket authenticated like the rest of
`/api/peer`, never for a browser.

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

## Out of scope for this stage

In the order it lands: machines on replicated disks and live migration with
the scoped two-primaries window (`lumen-virt` integration); snapshots and
the transactional rollback; the guided split-brain recovery; the HA manager
and the events stream it needs; packaging and ISO pins for the DRBD kmod
(ELRepo, kABI-gated exactly like kmod-zfs). No external storage export, no
thin provisioning under replication, and no scale past 3 replicas — stated
non-goals, not omissions.
