# LumenFS — pooled, deduplicating storage

A design for a from-scratch distributed storage engine: the storage of every
cluster member pooled into one namespace, every block held on two nodes,
global inline deduplication, end-to-end checksums with self-healing, storage
tiers, and copy-on-write snapshots and clones. The model is VergeOS's
VergeFS; the implementation is a Rust engine that consumes the cluster
machinery this appliance already trusts.

This document supersedes the recommendation in docs/storage-scaleout.md —
that document weighed LINSTOR against Ceph and recommended Ceph for clusters
of three or more; the decision recorded here is to build the engine instead.
docs/storage.md (DRBD replicated volumes) remains what exists in production
and keeps carrying machines until LumenFS has earned them.

**Status: phase 1 has begun.** The `lumen-fs` crate exists with the
simulation harness and the write path — the deterministic disk with crash
and torn-write injection, the v1 on-disk format (superblock, dual anchor
slots, WAL area, segment incarnations, self-validating block records), the
single-brick extent store with scan-based recovery, the write-ahead ring,
the COW map trees whose nodes are ordinary pool blocks, and the pool layer
that ties them into vdisks: write, read, flush-to-acknowledge, and the
two-flush checkpoint that folds dirty maps into trees and retires WAL
history. Two crash suites replay seeded power-loss histories under
`cargo test`: the brick-level contract (an acknowledged block survives
intact) and the vdisk-level one (an acknowledged write survives; an
unacknowledged write lands whole or not at all, never as garbage). Still
design: dedupe refcounts and GC, scrub, snapshots-as-API, NBD export.

## Why not Ceph, revisited

The scale-out document's own costs section is the argument. Ceph brings a
second quorum system beside corosync, a realistic floor of three (really
five) nodes, cluster-wide failure modes foreign to everything else on the
appliance, and a daemon fleet with its own upgrade orchestration. It cannot
run the two-node appliance that is the product today, so it could never
*replace* the DRBD path — it could only sit beside it, which is the "two
replication engines, two quorum systems" configuration that document itself
forbids.

A purpose-built engine inverts every one of those costs, at the price of
building it:

- **One quorum system.** LumenFS holds no elections and counts no votes. It
  consumes membership, quorum, and fencing verdicts from the cluster domain
  — corosync and Pacemaker, through the same topology regimes docs/cluster.md
  defines. The rule "corosync and Pacemaker own what must be correct" is not
  weakened; it is extended to a second consumer.
- **Two nodes are the first-class case**, not a degenerate deployment of a
  five-node design. The placement layer is designed for N and trivial at 2.
- **One failure model.** A dead peer is a fence-confirmed dead peer,
  identical to the HA manager's rule. Degraded I/O, resync, and split-brain
  prevention all hang off the verdicts the operator already understands from
  the cluster page.
- **The features that were the point.** Global inline dedupe, pool-native
  snapshots, and per-block placement are VergeFS's semantics. Ceph provides
  pooling but not this shape; DRBD provides neither.

What the engine costs is stated plainly at the end: it is the largest single
piece of software in the appliance, it holds the data, and it must be tested
like it knows that.

---

## The shape of the thing

```
┌────────────────────── one node ──────────────────────┐
│  QEMU ──vhost-user-blk──┐                            │
│  QEMU ──vhost-user-blk──┤                            │
│                     lumen-fsd  ←──peer replication──→│── other members
│                    ┌────┴─────┐        (Core network)│
│                 WAL + maps   bricks                  │
│                 (tier 0)   (one per disk, per tier)  │
└──────────────────────────────────────────────────────┘
```

- **Brick** — one physical disk given to the pool, carrying an append-only,
  log-structured segment store. Disks are owned raw: no ZFS underneath, no
  double COW, no double checksum. ZFS keeps the boot pool and local volumes;
  the drive wizard assigns each data disk to exactly one owner.
- **Block** — the unit of dedupe, checksum, and placement: a fixed-size
  extent (default 16 KiB, matching the volblocksize every VM disk uses
  today; chosen per pool at creation) addressed by its BLAKE3-256 hash.
  The content address *is* the checksum — dedupe and end-to-end integrity
  are one mechanism, not two.
- **Slice** — a shard of the hash space (256 per tier). Placement is a small
  map from slice → an ordered pair of members. Every block's home is decided
  by its hash: hash → slice → the two nodes that store it. At two members
  every slice lives on both and the map is degenerate, exactly as
  docs/storage-scaleout.md predicted; at three, slices spread
  (A,B),(B,C),(C,A) and the pool's capacity genuinely exceeds any one node.
  The slice map rides the membership record — versioned, gossiped, changed
  only by workflows.
- **Vdisk** — a virtual disk: a copy-on-write map from guest LBA to block
  hash. The map's own nodes are content-addressed blocks in the pool, so
  replication, checksums, and snapshots cover metadata by construction. A
  snapshot pins a map root; a clone is a new vdisk pointing at a shared
  root. Space reclaim is by refcount, with deltas batched through the WAL
  and compacted asynchronously — never a synchronous refcount write per I/O.
- **WAL** — a small per-node write-ahead log on the fastest tier,
  synchronously replicated to the peer holding the same slices. A guest
  write is acknowledged when the block and its WAL entry are durable on
  both members; map trees are folded forward in batches. Recovery is WAL
  replay; a torn write is a hash mismatch and is discarded.
- **lumen-fsd** — the data-plane daemon: io_uring against the bricks, the
  block export toward the guests (ublk, below), the replication stream
  toward peers over the Core network. This is deliberately not the CLI-wrapping backend
  shape the domain crates use — an engine is not an orchestration of
  somebody else's engine. The orchestration crate (below) keeps the
  house style.

### Write and read, end to end

A guest write is chunked into blocks and hashed. Each block's hash names its
slice; the slice map names its two homes. If the hash already exists there,
the write is a refcount delta — that is the whole of inline dedupe, falling
out of content-addressed placement rather than bolted beside it. If it does
not, the block is appended to a segment on each home and the vdisk map
update rides the WAL. The ack returns when both members have persisted it:
synchronous replication, same guarantee DRBD protocol C gives today.

A read walks the map, prefers the local replica, and verifies the hash on
every read. A mismatch is repaired from the peer replica and rewritten
locally — self-healing is the read path plus a background scrub that walks
segments continuously at a rate the Core network can spare.

At two members every read is local. At three, a block whose slice excludes
the reading node crosses the Core network — the same property Ceph has, and
the price of dedupe-driven placement. The VM's node always holds a replica
of *most* of its blocks only by accident of hashing; the two-node case, where
locality is total, remains the product's center of gravity.

### Failure, exactly once

The engine adds no new answers to "is the peer dead" — it asks the cluster.

- **Two-node regime**: a lost peer suspends writes to affected slices until
  Pacemaker confirms the peer fenced (or the operator uses the existing
  break-glass). Then the survivor bumps the slice epoch, continues at one
  replica, and logs dirty blocks per slice. Integrity over availability,
  identical to the DRBD `fencing resource-and-stonith` behavior, enforced
  by the same `fence_ipmilan` devices.
- **Return**: the fenced node comes back with a stale epoch, is refused as
  a replica target until it resyncs from the dirty log, and rejoins. A
  divergent history cannot form, because no node writes without either its
  peer or a fence verdict — split-brain is prevented by the same mechanism
  that prevents it today, not by a merge tool.
- **Three-node regime**: corosync majority replaces the fence-first rule
  for continuing, exactly as the topology engine already flips DRBD to
  volume quorum. The new capability at three is **re-protection**: a slice
  down to one replica re-replicates onto the third member, so the pool
  heals back to two copies instead of running degraded until repair — the
  thing neither two nodes nor DRBD can ever do.
- **HA restarts** are unchanged: the existing sweep, the fence-confirmed
  rule, the lowest-named-survivor election. What changes is eligibility —
  a pooled vdisk is startable on any member that can reach a quorate pool,
  not only on named replica holders. The single-writer guarantee moves from
  DRBD's auto-promote refusal to a per-vdisk writer lease in the WAL,
  held by the running node, handed over inside the live-migration window,
  and revoked by epoch when a holder is fence-confirmed dead.

### Tiers

Every brick carries a tier number at assignment (0 = NVMe downward), each
tier has its own slice map, and a vdisk names its tier at creation — the
VergeOS model. Tier 0 also hosts the WAL and the map working set. The
dedupe index is per-tier and slice-sharded: at 16 KiB blocks it costs about
4 GiB of index per TiB of unique data, held as an LSM on tier 0 with bloom
filters in memory — stated now, sized in phase 1, so the RAM budget is a
design number rather than a discovery.

### Snapshots, clones, rollback

A snapshot is a retained map root: instant, per-vdisk, crash-consistent,
identical on every member because the map is replicated pool data. Clones
are writable vdisks sharing blocks through refcounts. Rollback keeps the
existing contract — refused while the vdisk is open anywhere — but loses
its worst edge: there is no per-member snapshot set, no "one member's zvol
rolled back and resynced outward," and no ZFS `-r` destroying later
snapshots. The console's snapshot dialog gets simpler because the truth did.

---

## What it consumes, unchanged

Membership, quorum, and fencing from `lumen-cluster` and its topology
regimes. The Core network and its MTU handling for replication and the
firewalld service pattern for its ports (a new `lumen-pool` service
definition, bound at prepare exactly as `lumen-replication` is). The peer
channel for orchestration verbs. The membership record for the slice map
and pool definitions — written last, workflows transactional with unwind,
same as every create in the appliance. The HA manager's sweep and rules.
The maintenance drain. Secrets over pipes, never argv.

The compute domain consumes LumenFS through the same deliberately narrow
shape as `lumen_drbd::VmVolumes` — make a disk, recognise one, destroy one,
know where the machine can run, hold the migration window — implemented a
second time over vdisks and leases. Compute should not know which engine
sits under a disk beyond what those verbs answer.

That seam carries one assumption worth keeping rather than breaking:
`ReplicatedDisk` promises a **stable, identical block-device path on every
member**, and the domain document, the migration path, and the HA adoption
all lean on it. So the guest attach is **ublk** — the daemon serves each
vdisk as `/dev/ublkb<id>` with the same id on every member, through the
ublk driver that is in-tree in EL10's 6.12 kernel: no out-of-tree module,
none of the kABI gating ZFS and DRBD packaging carry, and not one line of
`domain_xml.rs` changes. vhost-user-blk is the recorded performance option
— it skips the kernel round trip, but it changes the disk's shape in the
domain document and requires shared-memory guest RAM — and it lands only
if phase 3's measurements demand it. NBD remains a bootstrap and debugging
export, never the VM path.

One record-shape consequence from the same survey: `VolumeRecord` is
DRBD-shaped (port, minor, zvol bytes) and should stay that way. LumenFS
adds sibling record types on `ClusterRecord` — pool, bricks, slice map,
vdisks — rather than generalizing a record that fits its engine.

A cluster runs **one** replicated-storage engine. The regime picks DRBD or
LumenFS at cluster create (or by explicit migration), never both live on
the same members — the scale-out document's rule, kept.

## Crates

```
lumen-storage/
├── lumen-fs/     the engine: formats, WAL, maps, dedupe, replication, scrub
│                 └── pure core over abstracted disk + network + clock,
│                     driven by deterministic simulation in cargo test
├── lumen-pool/   the orchestration domain, house style:
│                 model / render / state / peers / backend(cli|mock|unavailable) / service
└── lumen-fsd     the daemon binary: io_uring, vhost-user-blk, the real I/O
```

The split is the testing strategy. Everything that can corrupt data lives in
`lumen-fs` as a deterministic state machine over trait-abstracted storage,
network, and time — so `cargo test` on a laptop runs thousands of simulated
histories: crashes at every fsync boundary, torn writes, reordered peers,
partitions mid-resync, each with a seed that replays the failure exactly.
This is how FoundationDB earned trust and it is the only way a from-scratch
engine gets to hold customer data. `lumen-fsd` is a thin binding of that
core to io_uring and sockets; `lumen-pool` tests against `MockPeers` and a
mock daemon exactly as `lumen-drbd` does today.

## Phases

Each lands separately, each is abandonable, and DRBD carries production
until phase 6 says otherwise.

1. **The engine, one node.** On-disk formats (segments, blocks, WAL, map
   trees), dedupe index, snapshots and clones, refcount GC, scrub — under
   the simulation harness, exported over NBD for real-world smoke tests.
   No cluster anywhere. This phase is the majority of the risk and the
   work, and it is where the block-size and index-cost numbers become
   measurements.
2. **Replication.** Slices, the peer WAL stream, epochs bound to
   fence-confirmed verdicts, degraded mode, dirty-log resync, the
   two-node regime end to end. Exit test is docs/storage.md's own: write
   on one node, read on the other, pull the Core cable and watch I/O
   suspend rather than diverge.
3. **Machines.** The ublk export with stable per-member device ids, the
   narrow compute trait, the writer lease and the migration handover, HA
   sweep eligibility, the pool and vdisk pages in the console with the
   snapshot dialog.
4. **Tiers and the drive wizard.** Disk ownership (ZFS pool or LumenFS
   brick, never both), device classes, per-vdisk tier choice, the one
   capacity figure — labelled usable, because dedupe makes "raw" a lie in
   the flattering direction.
5. **Three members.** Slice reassignment on the existing 2→3 scale-out
   (the regime flip already has a home in the topology engine),
   background rebalance, re-protection, vdisks larger than a node.
6. **Migration.** DRBD volume → vdisk, offline first, then a mirrored
   cutover; the DRBD path stays supported for existing clusters until
   there are none.

## Stated costs

This is months of engineering before phase 3 ends, and the engine will be
the most safety-critical code in the product. Three commitments keep that
honest: the simulation harness exists before the daemon does (phase 1
starts with the test bed, not the format); pools are explicitly marked
preview until a burn-in period of scrub-clean operation on real hardware
has passed; and DRBD is not deprecated by roadmap but by evidence.

Non-goals, so they are decisions rather than surprises: no erasure coding
(RF=2 always — EC's small-random-write penalty is exactly the VM workload),
no external export (NFS/iSCSI out), no compression in v1 (the format
reserves a per-block codec byte so it can arrive later without a
reformat), and no dedupe across tiers (a block's tier is part of its home).
