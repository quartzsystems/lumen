# LumenFS engine concurrency — sharding the one lock

**Status: design, not yet code — revised once, after an adversarial
review (2026-08-02) found nine holes in the first draft.** The review's
findings are folded in below and marked ⚠ where they changed the
design; the biggest ones made the original "step 1 is small" claim
false, and the revised step 1 is honest about what it needs.

This document is the architecture conversation the write-path work of
2026-08-01/02 ended at. Every incremental fix landed (SIMD hashing,
prehash hoists, batched puts, pipelined peer ingest, two-phase
checkpoint, coalesced sends, bounded send queues, implicit durability,
scattered/zero-copy wire) and the node's ceiling is now the engine
mutex itself. Nothing here changes the wire format, the on-disk format,
the durability contract, or the fencing/resync semantics — it changes
who may hold which part of the engine at the same time.

## The problem, measured

One node, one `Mutex<ReplNode<FileDisk>>` (lumen-fsd daemon.rs). Every
guest write, every guest read, every peer apply, every maintenance fold,
every status probe takes the same lock. After the 2026-08-02 round:

- Sequential write, one writer: **524 MB/s** (from 313 at the round's
  start). Sequential read: **1.17 GB/s**. The perf rig (same code, no
  replication, faster CPU) does 731–781 MB/s — the remaining gap is
  lock-budget arithmetic, not hardware.
- **Simultaneous writes from both nodes: 147 + 57 = ~204 MB/s total** —
  *less than half of what one node does alone.* Each node is then both a
  source (its guest's writes) and an applier (the peer's stream), and
  both roles queue on one mutex. This is the shape a real VM fleet has
  all day: the bidirectional number is the honest capacity number, and
  it is the worst number we have.
- Profile of a saturated source: ~30% of lock-held time is `pwrite`
  into page cache (kernel memcpy + folio accounting), ~13% userspace
  memmove and allocator churn (now reduced by the zero-copy wire work),
  actual engine logic **under 5%**. The mutex serializes a workload that
  is overwhelmingly memcpy against independent cache pages.

The engine was built single-threaded on purpose — a pure state machine
under a deterministic simulation, no threads, no clocks — and that
purity is why the crash suites exist and why the protocol is
trustworthy. The design constraint for sharding is therefore double:
concurrency for the daemon, **and** the sans-IO single-threaded engine
must survive for the simulation. Anything that makes `cargo test`'s
seeded power-loss histories nondeterministic is wrong regardless of its
throughput.

## What the one lock actually protects

Enumerated from the code, because a sharding design that misses one of
these is a corruption generator:

| State | Written by | Coupling |
|---|---|---|
| Brick write heads (open segment cursor, per brick) | guest puts, peer puts, GC compaction | per brick; ⚠ GC seals, releases, and **reuses** segments |
| Brick index (hash → location) + `payload_bytes` counters | puts, GC | global dedupe: same hash must land once; ⚠ GC *recomputes* `payload_bytes` by summing the index |
| WAL ring (one holder brick) | every mutating op | **order = recovery replay order**; `WalFull` is answered by a checkpoint |
| Vdisk map trees: **dirty maps** + roots + manifest | writes/trims per vdisk, checkpoint folds | ⚠ reads consult `state.dirty` *before* the root; roots advance only at checkpoint |
| Repl sessions (rseq numbering, parked flushes, per-session pins, backlog) | ops, flush, peer messages | **rseq density per session = the replication contract** |
| Effects queue → per-peer outbound queues | everything | ⚠ per-peer FIFO is made in the daemon's `drain`, which today piggybacks on the engine mutex |
| Era, placement map, leases, serving state | verdicts, resync/adoption, reassign | validity of a write is judged against these **at entry** today |
| GC / scrub / checkpoint walkers | maintenance | need a consistent view of all of the above; scrub iterates the index under `&self` |
| `BrickSet.dirty` flags, `PayloadCache` | puts, flush, reads | dirty flags gate which bricks a flush syncs |

And the invariants that couple them, stated once so every step below can
be audited against them:

1. **Durability**: a block is guaranteed only after flush; flush spans
   every dirty brick. Payload is written before the WAL entry that
   references it, and recovery treats an op whose block is missing as
   the corruption it is.
2. **Stream density**: each session's `Apply` numbering is contiguous;
   a gap kills the session.
3. **Payload before op**: a peer must hold a block before the op
   referencing it applies; per-peer FIFO delivery carries this — and
   the FIFO is assembled in the daemon's drain, not only in the engine.
4. **Acknowledgement**: a guest flush completes only when every need is
   settled; a `Durable { up_to }` may only name ops **whose durability
   the just-finished barrier actually covered** — under concurrency
   that means `applied_rseq` snapshotted *before* the barrier starts,
   never read after it finishes. ⚠ (Today's single mutex makes
   read-after equivalent to read-before; plane B breaks that
   equivalence, and both `handle(Flush)` and `announce_durable` must
   switch to the snapshot discipline the two-phase checkpoint's
   `ckpt_applied` already uses.)
5. **Checkpoint consistency**: the anchor names a fold of a consistent
   snapshot — roots, WAL cursor, era, manifest from one instant.
6. **Single writer per vdisk**: the lease system — and the lease must
   be true **when the write publishes**, not merely when it entered.
7. **Determinism under simulation**: the crash suites replay seeded
   histories exactly.

## Why not just N mutexes

A naive split (one mutex per brick, one per vdisk, one for the WAL)
deadlocks or diverges immediately: a guest write touches a brick, the
index, the WAL, a vdisk map, and up to two peer sessions *in one
semantic step*, and the acknowledgement rule needs those to agree. The
split has to follow the semantics, not the struct fields. The semantics
divide into three planes with very different concurrency needs.

## The design: three planes

### Plane A — the block store: write-then-publish, no lock held over I/O

The store is content-addressed and blocks are immutable once written.
That makes the classic log-structured trick safe — with three guards
the first draft missed:

1. **Reserve**: bump the brick's write head by the record span, under a
   per-brick micro-lock; opening a fresh segment incarnation takes the
   brick's lock properly. The reservation **pins its segment**: a
   per-segment outstanding-reservation count that seal, GC's
   `release_segment`, and compaction refuse to cross. ⚠ Without the
   pin, the shipping maintenance loop (GC fires at <25% free, checked
   every second) can seal the open segment, find the unpublished
   reservation invisible to the index, release the segment, and reuse
   it for compaction rewrites — concurrent pwrites into the same byte
   range, i.e. acknowledged-data loss with no crash involved.
2. **Write**: `pwrite` header + payload into the reserved extent with
   no lock held. This is the 30% — fully parallel across writers and
   bricks.
3. **Publish**: insert hash → location into the index shard (sharded by
   BLAKE3 bits, so shard selection is free), release the reservation
   pin, **and revalidate the world**. ⚠ Validity was judged at entry
   (`writable()`: lease, era, serving state) and the world may have
   moved during the unlocked write — a relinquished pen (live
   migration!), a fence verdict, an adoption replacing the vdisk table.
   Publish therefore re-checks lease/era/serving plus an
   adoption/suspension generation counter, and a write whose world
   moved is refused: the extent is abandoned as an orphan and the
   caller sees the same refusal it would have seen at entry. Without
   this, a migration's relinquish races a straggler write into a
   two-writer interleaving on the wire — divergence, not a crash state.

Dedupe: publish is compare-and-insert; the losing writer of a same-hash
race abandons its extent (orphan, GC's bread). ⚠ A dedupe **hit** at
reserve time is a fourth guard: the found block may be an unreferenced
orphan, and a GC running during the unlocked window would sweep it
before the WAL entry that references it lands — the guest then gets a
hard `Corrupt` refusal (an EIO under space pressure, exactly when GC is
active). A dedupe hit must therefore take a pin on its target that
holds until the referencing WAL entry lands — the same shape as
`pin_inflight`, extended to local writes. On the applier path,
publish + `pin_inflight` must be one atomic act under the shard lock,
or a barrier'd GC between them re-opens the payload-ahead-of-op sweep
this codebase already found and fixed once.

Crash story: a crash between reserve and publish leaves a torn or
orphaned record in the segment — exactly what the salvage scan already
tolerates (recovery resynchronizes at sector granularity and record
spans are sector-padded, so an acknowledged record after a torn
reservation hole still recovers). A crash after publish but before the
WAL entry is an unreferenced block — the existing orphan class. **No
new crash states** — the new states are *concurrency* states, and they
are exactly the four guards above.

### Plane B — the op stream: one short sequencer lock

The WAL append, the rseq assignment, and the effect emission are the
places where *order is the product*. They stay serialized — but the
critical section shrinks to: append a ~50-byte WAL record, insert the
dirty-map entry, bump per-session rseq, push effects **into the
per-peer outbound queues**. Microseconds, no I/O, no hashing, no
payload bytes. Call it the **stream lock**.

⚠ Three ordering rules the first draft hand-waved:

- **The drain is part of the stream.** Today `Shared::drain` moves
  engine effects into `PeerLink` queues while holding the engine mutex
  — that piggyback is what makes per-peer FIFO real. Under plane B,
  two threads' stream sections racing their separate drains would
  enqueue Apply(6) before Apply(5) (density violation — session death)
  or an op before its payload (the peer's store-must-hold-it refusal —
  session death). So the stream lock must cover the enqueue into the
  per-peer queues, not just the effect emission; equivalently, the
  effects buffer dissolves and the stream section writes the peer
  queues directly. This constrains the daemon and `coalesce`, not just
  the engine.
- **Payload sends enqueue before their thread enters the stream
  section** (payload-before-op per invariant 3); interleaving by other
  threads between them is harmless — different blocks.
- **`WalFull` aborts the stream section having emitted nothing.** The
  append is the first act; on `WalFull` the section unwinds without
  consuming an rseq or queueing an effect, releases, runs the existing
  checkpoint-and-retry policy (`with_room`) under the barrier, and
  re-enters. An aborted section that had emitted anything would
  double-number the stream on retry.

A guest write becomes: hash on the worker (already the case) → plane A
put (parallel) → stream lock { WAL append, dirty-map insert, rseq,
enqueue } → done.

`Durable` replies and `announce_durable` follow invariant 4's snapshot
discipline: `applied_rseq` is read under the stream lock *before* the
flush barrier starts, and the answer names that snapshot — the
two-phase checkpoint's `ckpt_applied` already does exactly this, and it
generalizes.

### The read path — honest version

⚠ The first draft claimed lock-free reads from "root snapshots +
immutable blocks". That engine does not exist: `Pool::read_block`
consults the vdisk's **dirty map first** (including trim tombstones),
and roots advance only at checkpoint — up to seconds of writes live
only in `state.dirty`. A root-walk read would resurrect overwritten and
trimmed data.

Revised: a read takes the **stream lock for the dirty-map consult and
root load only** (a `BTreeMap` lookup — nanoseconds), then walks
immutable published blocks and does its `pread` with no lock. The
block-store `get` goes through the index shard. Contention on the
stream lock is bounded by the lookup cost, not the I/O. Two further
notes the table now carries: `ReplNode::fetched` (the serve-once
buffer) takes `&mut` on the read path and moves under the stream lock
too; and `BrickSet`'s `PayloadCache` mutex — uncontended today by
construction — becomes contended and must shard with the index or be
retired.

### Plane C — control: the old lock, now rarely taken

Sessions' state machines, hello/resync/adoption, fence verdicts, lease
changes, placement, GC, scrub, and checkpoint folds keep a single
control lock — the current mutex, demoted from "every 16 KiB block" to
"cluster events and maintenance". Operations that must stop the world
(adoption, era bumps, GC's mark, suspension flips) acquire control →
stream → per-brick locks in that fixed order — a **barrier
acquisition** — plus a wait on outstanding plane-A reservations (the
reservation pins double as the quiesce count). Rare, so cheap.

The two-phase checkpoint already has the correct shape: *begin* takes
the barrier just long enough to fold and snapshot (WAL cursor, and
since 2026-08-02 the per-session applied positions), the drain runs
with everything released, *commit* takes it again for the anchor. That
decision was made for latency; it turns out to be the concurrency
design too.

### Bookkeeping the first draft missed ⚠

- `Brick.payload_bytes` is maintained incrementally at `index_put` and
  **recomputed by summing the index** in GC's retain — under a sharded
  index that becomes per-shard sums, and a publish racing the recompute
  must not drift the counter (it feeds capacity reporting).
- `BrickSet.dirty` flags are set at **publish**, not reserve — a flush
  between reserve and publish must not clear a flag for data that is
  not yet flushed; publish-after-flush leaves the brick dirty for the
  next barrier, which is the conservative direction.
- `scrub_chunk` iterates the index and reads locations under `&self`;
  with concurrent publishes it needs a shard-snapshot iteration, and a
  compaction-moved record mid-read must re-resolve rather than report
  a false `Corrupt`.
- `Daemon::status` currently reads one consistent instant; under three
  planes its fields are mutually torn. That is acceptable for a status
  line and **must not** be "fixed" with a barrier acquisition.

## The simulation stays single-threaded — with a stronger claim

The crash suites and the repl sims must not become concurrent tests.
⚠ The first draft's seam claim was too glib: there is no existing
store trait to slot a concurrent implementation behind — `Pool` owns
`BrickSet` concretely and every put path is `&mut self`. The honest
precedent is the **`FlushHandle` pattern** the two-phase checkpoint
already established: a detached capability handed out under the lock,
exercised off it, committed back under it. Reserve/publish is exactly
that shape — *two engine calls with unlocked work between them* — and
it is new API on `Brick`, `BrickSet`, `Pool`, and `ReplNode`, not a
hidden internal change.

That shape buys back determinism: **because the phases are engine
calls, the sim pump can order them adversarially.** The crash suites
gain seeded histories that interleave GC, lease changes, fence
verdicts, adoptions, and crashes *between* a reserve and its publish —
single-threaded, deterministic, replayable — covering findings 1, 2,
and 5 above without a thread in sight. A threaded stress suite over
`FileDisk` on tmpfs (scrub + acknowledged-write audit per round) covers
what only real parallelism can: the dedupe CAS race, torn-counter
drift, barrier livelock.

## Migration steps, each shippable and measurable

1. **Puts off the lock — the `FlushHandle` pattern.** Keep the engine
   mutex; split the put into reserve (in-lock: head bump, segment pin,
   dedupe check-and-pin) → pwrite (out-of-lock) → publish (in-lock:
   index insert, pin release, **revalidation** of lease/era/serving/
   adoption generation, refusal-and-orphan when the world moved). GC
   learns to respect reservation pins. The sim keeps the single-call
   form *and* gains the interleaved three-phase histories. *Expected:
   bidirectional total roughly doubles; single-writer seqwrite gains
   the pwrite overlap.* Not the "evening patch" the first draft
   implied — the pins and revalidation are real engine surface — but
   still the smallest correct step.
2. **Stream lock**: extract WAL-append + dirty-map insert + rseq +
   per-peer enqueue into the short sequencer (drain moves inside it);
   `WalFull` becomes abort-and-retry; `Durable`/`announce_durable`
   switch to before-barrier snapshots; reads take the stream lock for
   the dirty consult only. The big mutex becomes plane C.
3. **Sharded index + per-brick heads**: full plane A — dedupe CAS,
   orphan-on-loss, per-shard `payload_bytes`, scrub shard-snapshots,
   atomic publish+pin on the applier path.
4. **Follow-ons that ride along**: `pwritev` scatter into the segment
   (kills the last staging memcpy — `libc` at the `FileDisk` seam),
   `read_into` for the ublk read path, per-brick flush fan-out at
   guest-flush time.

## What this does not fix

- The wire is one TCP stream per peer pair; at some throughput the
  single connection becomes the cap. Multiple streams per peer (striped
  by slice) are a separate, later conversation — stream density
  (invariant 2) makes this a protocol change.
- Cross-node scaling: sharding fixes *one node's* ceiling; placement
  already spreads the mesh.
- fsync latency floors: bounded since 2026-08-02 by the send-queue cap
  + implicit durability; sharding shortens the applier's queue time
  further but the floor is the platter.
