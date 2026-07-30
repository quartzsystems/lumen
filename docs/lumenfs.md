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
simulation harness and the full single-node data path — the deterministic
disk with crash and torn-write injection, the v1 on-disk format
(superblock, dual anchor slots, WAL area, segment incarnations,
self-validating block records), the single-brick extent store with
salvage-scan recovery (one rotted record cannot take its neighbors down),
the epoch-fenced write-ahead ring (stale debris can never chain after
acknowledged history), the COW map trees whose nodes are ordinary pool
blocks, and the pool layer over them: write, read, trim, vdisk create and
delete, flush-to-acknowledge, the two-flush checkpoint, mark-and-sweep
garbage collection with segment compaction (invisible to maps — a block's
identity is its hash, not its location), and the scrub that reports rot
and missing references. Two crash suites replay seeded power-loss
histories under `cargo test`: the brick-level contract (an acknowledged
block survives intact) and the vdisk-level one (an acknowledged write
survives; an unacknowledged write lands whole or not at all, never as
garbage — with trims and collections interleaved through the crashes).

**Phase 1's engine work is complete.** Snapshots, writable clones, and
rollback landed as checkpoint-grade synchronous operations (a snapshot is
a retained root, exactly as designed; a clone shares every block with its
source through dedupe; a vdisk with snapshots refuses to die until they
do), pinned by their own crash suite. The byte-granular view a block
device speaks — read-modify-write edges, zero-fill, short tail blocks,
advisory trim alignment — lives in the library and is model-tested under
the simulation. And `lumen-fs-nbd`, a std-only smoke tool (deliberately
not the daemon), formats a pool on a real file with fsync as the barrier
and serves vdisk 1 over fixed-newstyle NBD, so the engine can take a real
filesystem from a real kernel: `lumen-fs-nbd format <file> <bytes>
<vdisk-bytes>`, `serve <file> <addr>`, `scrub <file>`. What remains before
phase 2 (replication) is burn-in, not construction: real-hardware NBD
smoke runs on the lumen boxes.

**Phase 2's two-node core is in.** `repl.rs` is a sans-IO `ReplNode` — no
socket, no clock — that the simulation drives through failovers,
partitions, and crashes. Only the operation stream and its payloads cross
the wire: because map trees are canonical, shipping ops makes the nodes
converge by construction, and each node keeps its own WAL, checkpoints,
anchor, and collection uncoordinated. A guest flush completes only when
both nodes hold the writes or a fence verdict says there is one node left
— DRBD protocol C's guarantee, restated. **The engine never decides
death**: `set_peer_fenced` is the cluster's verdict arriving from above
(docs/cluster.md), and without it a partitioned node suspends forever,
refusing writes and parking flushes. The verdict is made durable by the
anchor's **era**, bumped before the survivor writes anything new, so a
returning node knows which side is stale.

Resync needs no dirty bitmap: it is a Merkle diff. The source checkpoints
and offers its roots; the target walks down, transferring only subtrees
whose hashes it lacks, then adopts the offer whole — which is also how its
own divergent unacknowledged history is discarded. One subtlety is worth
recording because the suite caught it: a target may *hold* an interior
node whose children never arrived, if an earlier walk was cut short, so
the walk descends into subtrees it already has rather than assuming a
matching hash means a complete subtree. Skipping the *transfer* is the
Merkle win; skipping the *verification* would be adopting a root over a
hole. A second lesson from the same test: messages queued for a link that
dies are dropped with it, or a stale reply arrives inside a fresh session
and is mistaken for an answer to it.

**Phase 3 has begun: the writer lease is durable, and migration has a
window.** Who may write a vdisk used to be a map in one node's memory,
rebuilt from whatever a peer remembered at the next resync — which is a
thin place to keep the one property that stops two nodes writing the same
disk. It now lives in the pool: a WAL entry like any other, folded into
the manifest at each checkpoint, so a node that restarts knows whether it
may write before anyone tells it.

A lease names the era it was granted in, and that is what lets a failover
happen at all: a survivor that has been handed a fence verdict bumps the
era, and leases from the era it survived stop binding. Otherwise a lease
would outlive its holder and leave the vdisk unwritable at precisely the
moment somebody needed to rescue it.

Live migration gets the window DRBD spends `--allow-two-primaries` on,
without ever having two writers. `begin_handover` marks the lease as
passing to the destination — both nodes may hold the disk open, **the
source keeps writing**, the destination may not — and `accept_handover`
moves it in one durable step, so there is no instant in between when both
could write. Every path out closes the window, `abort_handover` included,
because a window left open is how two writers eventually happen.

**A resync no longer stops the guests.** A source with era superiority —
the fence-verdict survivor, the node actually running the machines — keeps
serving and acknowledging through the whole pull. It cuts the offer at a
checkpoint and streams every op from that point on; the target buffers the
stream, adopts the offer, then replays the buffer, landing exactly on the
source's live state with no gap and no overlap. Three things carry the
correctness, each pinned by a test that fails without it:

- **Sync pins.** The offered roots are held live against garbage
  collection on both ends for the duration of the pull — the source's own
  checkpoints orphan the offer it is still serving blocks from, and the
  target's fetched fragment is referenced by nothing in its own manifest
  until adoption. Collection stays legal mid-resync on both ends; the
  pins are volatile because a crash ends the session that needed them.
- **The witnessed-era floor.** A fence-verdict bump now clears every era
  the node has ever heard a peer claim, not just its own. A survivor
  fenced mid-pull still carries its old era; bumping from that alone
  would mint a second copy of the era it was adopting, and the next
  reconnect would tie two divergent lineages — with the tie-break
  pointing at the dead node's disk. (The floor is volatile too; the
  narrow residue — hear an era, crash before adopting any of it, then
  survive a verdict — is accepted and recorded here rather than closed.)
- **The closing handshake.** Adoption hands the target the source's era,
  after which the eras cannot order the two nodes — so a single-copy
  acknowledgement issued after adoption would be a write a later
  equal-era tie could discard. The target therefore asks leave
  (`SyncReady`), the source stops serving first and answers
  (`SyncAdopt`), and only then does the target adopt, replay, flush, and
  confirm (`SyncDone`). The guests' one interruption is that closing
  exchange — a round trip plus the stream still in flight — not the
  pull. The equal-era tie keeps refusing writes for its (cheap) diff, as
  before: without a verdict there is no honest single-copy promise.

**lumen-fsd exists: the engine has a daemon.** The crate the crates
section below promised is real — std-only, threads and blocking sockets,
no async runtime, because the honest first binding of a sans-IO core is
the simplest one that can carry it (io_uring and ublk are Linux
refinements that slot in underneath later, unfelt by the engine). It is
deliberately a *harness*: the same role the test pump plays under
simulation, played against reality.

- **The wire format is the daemon's** (`wire.rs`), exactly as repl.rs
  assigns it: hand-rolled fixed little-endian frames, strict decoding
  (trailing bytes are an error, unknown tags are an error), every count
  checked against the bytes present before anything is allocated. A
  25-byte handshake opens each session — magic, pool uuid, node id — and
  a brick from a different pool is refused before the engine hears a
  word.
- **Effects drain under the engine lock**, so the wire sees one ordered
  stream no matter how many threads call in; **bytes never cross
  connection incarnations** — every session boundary bumps an incarnation
  and clears the outbound queue, the writer thread quits the moment its
  incarnation is stale, and teardown is exactly-once by the same check
  (a late teardown must not knock a Degraded node back to Suspended
  after a verdict landed).
- **Suspension is real**: a guest write against a suspended node blocks,
  a guest flush blocks until its ticket resolves, and a fence verdict
  releases both — DRBD's suspended I/O, reproduced honestly. The verdict
  (`fence-peer` on the control socket, the break-glass until lumen-pool
  wires the cluster's own machinery in) forces the link down before it
  acts, because a pulled cable is silent and the verdict's authority
  must not queue behind a TCP timeout.
- **NBD rides the daemon now**: same bootstrap protocol as the smoke
  tool, but through the replicated guest path, and the attach *claims
  the writer lease at the door* — a vdisk the peer holds refuses the
  client before promising a disk.

The integration suite runs two whole daemons in-process over loopback —
phase 2's exit test in harness form: write on one node and read it on the
other; kill a peer and watch I/O suspend, not error and not diverge;
fence and watch the parked write complete single-copy; restart the dead
node from its own brick and watch it resync, rejoin, and answer reads it
was dead for. Three findings from building it, each now pinned by a test:

- **A survivor's own lease must ride through its own verdict.** The era
  bump retires every lease of the era it closes — right for the dead
  node's leases (that is what lets a failover claim them), wrong for the
  survivor's own: its guest is still running, and its next write found
  the pen gone. `set_peer_fenced` now re-issues the survivor's held
  leases under the new era; a handover window open toward the dead node
  closes in the same stroke. The sim suite had never noticed because its
  tests always re-claimed by habit — it took a daemon behaving like a
  daemon to ask.
- **A thread parked in `accept()` cannot be woken by a promise.** The
  shutdown path signalled the accept thread by connecting to its own
  listener — a one-shot signal that a full backlog silently eats, and
  the backlog *was* full, because a dialer whose handshake was being
  refused (which is not a failed connect) redialled in a tight loop.
  One hang in roughly thirty-five runs. The accept loop is a
  nonblocking poll now, and a dialer pauses after every session ending,
  refusal included.
- Suspension-blocking guest calls and ticket waits all wake on a shared
  condition rather than sleeping on the world: the daemon has no
  polling in its data path, only in its edges.

**The ublk export is real, and it ran on the real kernel.** A vdisk is a
block device now — `/dev/ublkb<id>`, the same id on every member, exactly
the stable-path shape `domain_xml.rs` already leans on. The split honors
what can be verified where: `ublk/uapi.rs` transcribes the 6.12 kernel
interface with layout tests that run on every dev box (a wrong offset is
guest corruption on hardware, caught on a laptop as arithmetic);
`ublk/uring.rs` is a minimal hand-rolled io_uring — setup, three mmaps,
uring_cmd submit, wait — with unsafe confined to one file; the server is
one queue of thirty-two kernel-copied buffers whose every operation lands
on the daemon's guest path, so replication, the flush contract, and
suspension apply to the block device by construction. The device
advertises a volatile cache on purpose: that is what makes the kernel
*send* flushes, and a flush completes only on the engine's two-node (or
verdict-backed) acknowledgement — an export that quietly swallowed
flushes would pass every casual test and lose acknowledged writes at the
first power cut. Discard maps to trim at whole-block granularity;
write-zeroes writes literal zeros, which dedupe makes nearly free and
which trim's advisory semantics could not honestly promise.

Run against lumen1's EL10 kernel, not just written: ext4 on the export,
hundreds of megabytes round-tripped checksum-stable through cache drops
and remounts, fstrim honored, engine scrub clean — and then the whole
failover story as one script: a filesystem written through node A's
device with two-node acknowledgement, A killed uncleanly, B fenced by
verdict, and the same filesystem mounted from B's own brick,
byte-identical, twenty-plus rounds. Three things the kernel taught that
the header file does not say:

- **EL10 ships io_uring disabled** (`kernel.io_uring_disabled=2`), so the
  export dies at setup with EPERM until a sysctl opens it. The appliance
  needs a `sysctl.d` drop-in in the storage package — recorded as a
  packaging follow-on, and the kind of default that would have cost a
  support call in the field.
- ADD_DEV with an explicit device id demands the no-queue sentinel
  (`queue_id = 0xFFFF`) in the control command, or answers EINVAL.
- The driver requires 128-byte SQEs on both rings — including the data
  ring, whose 16-byte commands would fit a plain SQE.

And the failover loop earned its keep: roughly one round in fifteen, the
filesystem mounted from B was missing its acknowledged tail — 2,300-odd
operations that an fsync-backed `dd` and a clean unmount had supposedly
committed two-node. The engine was innocent, and proving that is what
took the time: the failed brick scrubbed clean, hashes matched at every
stage, and content-addressing left nothing to hide behind. The bug was
one anonymous bit in the export: `attrs: 1 << 1` is ROTATIONAL, not
VOLATILE_CACHE (`1 << 2`), so the block layer treated the device as
write-through and **elided every flush** — fsync succeeded without the
durability contract ever engaging, and the "committed" tail was just
whatever had happened to drain across the wire before the kill. What
convicted it was observability added for the hunt: the daemon's stream
counters showed `durable=0` after a "successful" fsync — in the passing
rounds too, which is the tell that a guarantee is being skipped rather
than raced. The attrs are named constants now, the smoke asserts
`durable > 0` after every fsync-backed write as a permanent regression
guard, and forty consecutive failover rounds pass with `sent == durable`
at every unmount. The module doc had warned that an export that quietly
swallows flushes passes every casual test; it was right, it was this
export, and only a kill-mid-stream test could have said so.

**The compute seam, mapped.** The next slice is surveyed and its shape
decided, so it is written here rather than rediscovered. The seam is
`lumen_drbd::VmVolumes` (lumen-drbd/src/vm.rs) — five async methods, and
`VirtService` holds it as `Arc<dyn VmVolumes>`, so a LumenFS-backed
implementation slots into `VirtService::new` without one compute change.
The mapping:

- `create_disk` → create the vdisk through the daemon (replicated op),
  export it as `/dev/ublkb<id>` **on every member** — the stable
  identical path is the contract — and return that device with the
  pool's member list. Vdisk id, ublk id, and the record entry are one
  allocation, recorded as a sibling collection on `ClusterRecord`
  (`#[serde(default)]`, the `volumes: Vec<VolumeRecord>` pattern).
- `disk_of` → parse `/dev/ublkb<N>`, answer from the records; `None`
  for anything else — the predicate the callers lean on.
- `destroy_disk` → detach the exports, delete the vdisk (replicated),
  drop the record. Refused while snapshots pin it, as the engine already
  insists.
- `common_members` → for pooled vdisks: every member that can reach a
  quorate pool — which is the phase-3 HA-eligibility change arriving
  through the seam ha.rs already uses (`ha.rs:114`), with no sweep
  changes at all. Two members while Synced; the survivor while
  Degraded.
- `set_two_primaries(device, allow)` → the lease window, and here the
  seam needs one honest adjustment: DRBD's window is symmetric, but a
  handover names its destination, and `accept` must run on the
  *destination* when the guest starts writing there. So the LumenFS
  implementation takes the window verbs through the daemon —
  `begin_handover` on the source at open; on close, `accept_handover`
  on the destination when the migration succeeded and `abort_handover`
  on the source when it did not — which likely means the trait's window
  method grows a destination parameter (or a sibling method), decided
  when the implementation lands.

Two consequences the export must absorb first: the daemon needs vdisk
lifecycle verbs on its control surface (today it serves exactly vdisk 1
via a boot flag), and the ublk attach must stop claiming the writer
lease unconditionally — a migration destination opens the device
*without* the pen, and writes refuse until the handover's accept. The
claim moves to "first attach outside a window", which the lease state
already distinguishes.

Still ahead in phase 3 beyond that: the console pages (pool and vdisk
views with the snapshot dialog); then the slice map that takes this past
two nodes.

## Burning it in

The simulation decides when a disk loses power. Real hardware decides for
itself, and lies differently — so `burn-in.sh` runs the same durability
contract against a device's own fsync, on the machine that will carry it.

The workload records its progress **inside the vdisk**: block 0 holds a
watermark, the count of operations it has acknowledged. Each round writes a
batch of data blocks, flushes them, then writes the new watermark and
flushes again — in that order, so at every instant the disk satisfies one
rule: *every operation at or below the stored watermark is present and
exact.* The verifier replays the seed and demands exactly that. It needs no
log, no side channel, and no trust in the process it just killed.

```sh
# On a node, with the binary and script copied over:
./burn-in.sh --rounds 20                    # 4 GiB image under /var/tmp
./burn-in.sh --device /dev/sdX --rounds 50  # a real disk; ERASES IT
./burn-in.sh --forever                      # then pull the power, and
./burn-in.sh --verify-only                  # after the reboot, ask it
```

**The harness keeps its own books.** A pool that has lost part of what it
owed cannot be trusted to say how much that was: its watermark shrinks with
the loss, forgiving the very debt under examination. So the highest
watermark ever shown is recorded beside the pool, and a resumed pool below
it fails — whatever it now claims about itself. Without this the burn-in
graded a pool that had silently reverted from 9,912 acknowledged operations
to 192 and called it intact.

Operations above the watermark were never acknowledged, so the verifier
allows each block one alternative value: whatever the in-flight batch
wrote. That window is exactly one batch wide — the workload cannot begin a
batch until the previous one's watermark is durable, and a resumed run
rewrites the same window from the same seed — and every block outside it
must be exact. The report says how many blocks it forgave, because a number
that stops looking like one batch is itself a finding.

SIGKILL is not a power cut: the page cache survives it, so a clean
kill-loop proves the engine's ordering rather than the device's honesty
about flushes. `--forever` plus a pulled cord tests the rest.

**The first run on real hardware survived a genuine power cut** — the cord
pulled mid-workload, 428,304 acknowledged operations, every one intact on
the far side with a clean scrub. It also found, in twenty kill-and-verify
rounds, that collection stalls at high utilization:

- **A collection could free nothing when the brick was nearly full**, which
  is when it matters. Everything was reclaimed at the *end* of the sweep,
  so every evacuation had to fit in the space that existed *before* any of
  it was freed — on a full brick, only the reserve. It ran out, no source
  was ever emptied, and nothing came back. Segments are now released as
  each evacuation lands, so the space compounds and a collection can work
  its way out of a brick with almost nothing free.
- **A fixed "only compact segments under half live" rule made a pool near
  half utilisation uncollectable**, because no segment anywhere qualified.
  Candidates are now taken cheapest-first with no threshold, and only a
  segment that is almost entirely live is skipped — moving that one would
  cost about what it returns. Expensive evacuation stops once enough room
  is back; reclaiming a *dead* segment is always done, since it costs a
  sector.
- **Space pressure was only ever announced by failure.** `Full` is too late
  to be policy: by then every write triggers a collection and the pool
  spends itself collecting. `Pool::space()` now reports the cliff before
  it arrives, and the tool collects on the way down.

Together those turned sixteen do-nothing rounds into sixteen that made
progress. The harness had called them all "survived", which is the next
finding: **a round that acknowledges nothing is not a round that
survived**, and the script says so instead of reporting green forever once
the watermark stops moving.

**Then that check cried wolf, and the measurement is the point.** Later
rounds still went quiet, and the obvious reading — the pool is stuck — was
wrong. Asked directly at the moment it looked wedged, the pool was doing
**~3,900 operations a second**: not stuck, just no longer able to finish a
batch inside a three-second round with a process restart in front of it. A
batch only counts when it completes, so "slower than the round" and
"stopped" look identical from outside. The harness now retries a silent
round with room to breathe and only calls it stuck if that fails too, and
it points at `info` and `gc` when it does.

Two measured costs came out of chasing it, neither yet addressed:

- **Opening a pool is O(what is stored in it)** — 1.78 s against 1.3 GiB
  of live data, versus 26 ms fresh — because the recovery scan reads *and
  rehashes* every record. This is the price of an index rebuilt by
  scanning, which docs above call a deliberate phase-1 trade; what the
  burn-in adds is a number, and the observation that it bounds recovery
  time on a large brick. The persisted index is the answer, and it is
  already the plan.
- **Throughput falls as a pool fills**, which is inherent to a
  copy-on-write store — near full it must move a byte to place a byte —
  but the shape of the curve here has not been characterised, only
  survived. Worth measuring properly before phase 3.

`lumen-fs-nbd gc <file>` and `info <file>` exist because guessing at space
behaviour from the outside is how all of this went unnoticed for so long.

### Where it stands on real hardware

On lumen1, a 4 GiB pool carrying a 2 GiB vdisk — so a fully-written vdisk
occupies **half the brick**, the case that used to stall:

- **20 rounds, 396,648 acknowledged operations, every one intact.** No
  round wrong, corrupt, or missing.
- **Steady state reached and held.** Per-round progress settles to roughly
  15,500 operations by round 8 and stays there for the remaining thirteen
  rounds, with the live-block count oscillating around 140–154k rather
  than climbing. Collection keeps pace with writing at 50% utilisation,
  which is the property the earlier collapse denied.
- **A real power cut, separately: 428,304 operations, all intact.**

That is phase 1's durability contract demonstrated against real hardware
rather than a modelled disk. Two things it does not yet cover: a raw block
device (`--device`, exercised only against a loop device so far), and a
long soak rather than minutes.

One number worth reading carefully: the index holds ~154k blocks where the
vdisk has 131,071 and its map needs ~257. The remainder is dead records
the open scan re-indexed, which the next collection drops — harmless by
design, but it is also what makes opening a worked pool slow, so the two
findings are one finding.

**Two findings from its first local run**, both fixed, neither reachable
from the simulation:

- Nothing collected garbage under pressure. The tool had a policy for a
  full write-ahead ring and none for a full brick, so a long-running export
  would have failed writes with most of its space reclaimable. Space
  pressure is now handled where policy belongs — in the caller.
- Worse, and the reason the reserve exists: **a collection must write
  before it can free.** Its opening checkpoint folds dirty maps into new
  tree nodes, and compaction rewrites live records before releasing their
  segments, so a brick with nothing free could never be collected — the
  state where reclaiming is the only remedy was the one state where it was
  impossible. A slice of the brick (a sixteenth, never less than one
  segment) is now held back from ordinary writes and opened only for a
  collection. `a_brick_that_has_run_out_can_still_be_collected` keeps it
  honest.

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
  root. Space reclaim is mark-and-sweep from the anchored roots, with
  compaction of sparse segments — a recorded deviation from this design's
  earlier refcount sketch. With the index rebuilt by scan, liveness is
  already a pure function of durable state, and a sweep has no persistent
  counter to ever desync; refcount deltas may return alongside the
  persisted dedupe index if scan-marking ever grows too slow, and nothing
  on disk precludes them.
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
└── lumen-fsd     the daemon binary: sockets, exports (NBD now, ublk next),
                  threads and files today, io_uring when measurements ask
```

The split is the testing strategy. Everything that can corrupt data lives in
`lumen-fs` as a deterministic state machine over trait-abstracted storage,
network, and time — so `cargo test` on a laptop runs thousands of simulated
histories: crashes at every fsync boundary, torn writes, reordered peers,
partitions mid-resync, each with a seed that replays the failure exactly.
This is how FoundationDB earned trust and it is the only way a from-scratch
engine gets to hold customer data. `lumen-fsd` is a thin binding of that
core to sockets and files (io_uring when measurements ask for it), and its
integration suite runs two whole daemons in-process over loopback;
`lumen-pool` tests against `MockPeers` and a mock daemon exactly as
`lumen-drbd` does today.

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
