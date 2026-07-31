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
source keeps writing**, the destination may not — and `relinquish` moves
it in one durable step. Every path out closes the window,
`abort_handover` included, because a window left open is how two writers
eventually happen.

**The pen is always handed over, never taken**, and that asymmetry is the
whole of why there is no interval with two writers. `relinquish` runs on
the source; the destination's `accept_handover` merely *asks* whether the
pen has arrived, and refuses until it has — which is what an orchestrator
waits on. Were it the other way round, the destination could take the
lease while the source went on believing it may write until the change
replicated to it. Because only the holder ever changes the record, that
window does not exist. (It briefly did, and driving a migration through
the control protocol is what showed it.)

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
  export dies at setup with EPERM until a sysctl opens it. The storage
  package now ships the `sysctl.d` drop-in that raises it to 1 —
  privileged callers only, never 0 — and the two-host run is what proved
  the drop-in has to be *shipped* rather than set: the value had been
  raised by hand on lumen1 and lumen2 was still at EL10's 2.
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

**Both prerequisites are in.** The daemon now has the vdisk lifecycle and
the export registry on its control surface — `vdisk-create`,
`vdisk-delete`, `vdisks`, `export <vdisk> <dev>`, `unexport`, `exports`,
`lease`, and the three migration acts (`handover <vdisk> <to>` on the
source, `accept` on the destination, `abort` on the source) — so an
orchestrator drives storage without restarting anything. Exports are
owned by the daemon rather than by whoever started them, which is what
makes stopping possible at all: a block device that outlives the process
serving it is a device whose next reader hangs, so shutdown tears every
export down *before* the engine stops answering.

And the attach no longer grabs the pen unconditionally. `GuestHandle::attach`
returns `Writer` or `Penless`: ordinarily it claims the lease — that is
the single-writer door — but an attach that finds a window aimed at this
node opens **penless**, and an attach where the peer holds the lease with
no window naming us is somebody else's disk and is refused outright. A
window from a retired era binds nothing, so a survivor that moved on is
not offered a door it should not walk through. This is what lets a
migration destination hold the disk open while the source is still
writing it, which is the whole shape of the thing.

The suite now covers that shape end to end on real sockets: a vdisk's
creation and deletion replicating from either end; an ordinary attach
taking the pen and a pre-window attach being refused; a penless attach
inside the window where the source keeps writing and the destination is
refused; the accept as a single instant, after which the *source* is the
one refused; and an aborted window leaving the disk exactly where it
started, with the destination unable to accept or even reopen. Each of
those fails if the penless branch is removed — checked by removing it.

**And it holds on real block devices.** On lumen1: a vdisk created
through the control surface and replicated; exported from A, given an
ext4 and 64 MiB of payload with flushes engaging; a window opened, and
the *same vdisk* opened on B as its own `/dev/ublkb` — both devices
reading identically, a `dd` to the destination refused by the kernel with
an I/O error, and the source still writing through its own window; the
accept, after which the destination writes and the **source** is the one
refused; the filesystem mounted from the destination with the payload
byte-identical; and the export torn down leaving no device behind. Two
writers never existed at any point a shell could observe.

Getting there cost two teardown deadlocks, both of the kind only a real
kernel tells you about, and together they fix the order of an export's
last four steps — each one unblocking the next:

1. **Release parked I/O.** STOP_DEV freezes the block queue and waits for
   in-flight requests to complete. A request parked *inside the engine* on
   a suspension no verdict will ever resolve never completes — so a peer
   dying unfenced left STOP_DEV waiting in `blk_mq_freeze_queue_wait`
   while the guest's `dd` waited in `submit_bio_wait`, forever, with the
   control surface stuck behind it (a teardown runs on the thread
   answering the operator). A guest handle can now be **cancelled**:
   parked operations give up and become errors, which is the honest answer
   for a device that is going away — nothing was acknowledged, so nothing
   is being taken back. Suspended I/O still waits for a verdict in every
   case except the one where its device is being removed.
2. **STOP_DEV**, which aborts the queue's parked fetches — the only thing
   that ends the servicing loop, so it must precede the join.
3. **Join**, which is what drops the descriptor mapping.
4. **DEL_DEV**, last. The first deadlock was here: `unexport` never
   returned, with a kernel thread in `ublk_ctrl_del_dev` and the queue
   thread long since exited, because **an mmap outlives the descriptor it
   came from**. The request-descriptor region was mapped and never
   unmapped, so it kept its own reference to the char device and the
   kernel's delete waited forever for a device nobody was using. The
   mapping is an RAII guard now, and the delete waits only on what the
   join has already released.

Both were diagnosed the same way — thread wait-channels on the live box,
which named the exact kernel function each side was parked in — and
neither was reachable from the simulation or from a test that never tore
an export down while its node was suspended. Both are confirmed on
hardware: with the peer killed unfenced and a guest write parked,
`unexport` now returns in **under a second** where it previously never
returned, the parked write gets an honest `EIO`, no device is left
behind, and the control surface keeps answering — three rounds, with the
migration story re-run alongside to show the reordering cost it nothing.

One thing that recovery taught, and it is the daemon's own contract
paying off: the wedge was cleared **without a reboot** by restarting the
dead peer. Suspended I/O waits for a verdict *or a peer*, and the peer
link is independent of the control surface — so bringing the peer back
ended the suspension, completed the parked write two-node, released the
frozen queue, and let the stuck teardown finish on its own. A node whose
control surface is blocked is not necessarily a node that needs the power
cycled.

**An operational rule the second one taught, worth stating for anyone who
ever debugs this daemon: never `kill -9` a ublk server while a guest
request is in flight.** Nothing is left to complete that request, so the
device's queue can never freeze, the server's threads can never leave the
kernel, and SIGKILL is ignored because they are in uninterruptible sleep
— the minor is then unrecoverable until the machine reboots. A recoverable
hang becomes a permanent one. Ask the daemon to `unexport` (bounded), and
escalate no further than SIGTERM while a device still exists.

**The control surface is now the library's, with a typed client**, because
it has two callers of equal standing: an operator typing verbs at a
socket, and the orchestration layer issuing the same ones from Rust. A
protocol with two consumers should have one definition, and the tests
should drive the real dispatcher rather than a stand-in. Replies a program
reads are `key=value` — `lease 2` answers `ok: holder=0 era=1 handing=1`,
as readable at a shell as it is from `Client::lease`. Two things the tests
found immediately, both fixed: an empty listing answers a bare `ok` and a
client that treated that as a parse failure would have reported a healthy
pool as broken; and `lease` on a vdisk that does not exist used to answer
`unheld`, which conflates "nobody holds it" with "there is no such
vdisk" — a distinction the seam's `disk_of` predicate depends on.

**One honesty note about the handover, surfaced by driving it through the
protocol.** `accept` moves the pen on the destination in one durable step,
and the source learns by replication — so for the width of that
propagation there is an interval in which the source still believes it may
write. In a real migration nothing writes there, because the guest is
paused on the source before it is resumed on the destination: the lease is
a durable record of who *should* be writing, and the ordering is supplied
by the migration protocol above it. But that means the lease is a backstop
against mistakes rather than a mutual-exclusion primitive across the
propagation delay.

**That gap is closed: the source relinquishes.** It sets
`holder = destination` in one durable step, replicated like any other
lease change, and the destination waits to see the lease name it. Because
only one node ever changes the record, there is no interval in which two
nodes both believe they may write, and the backstop is now exactly as
strong as the record. `accept` survives as a *verification* rather than a
seizure — it succeeds only when the lease names this node **and no window
is still open**, and refuses otherwise, so no path remains by which a
destination takes a pen the source has not handed over. That second
condition is not pedantry: a node holding the pen mid-window is
mid-migration, and answering yes there would let a caller treat an
unfinished handover as a finished one. The unplanned case needs nothing
new — a source that is simply gone is a failover, which the era bump and a
fresh claim already handle.

Both halves are pinned by tests that fail when either is removed, and the
whole sequence is validated on lumen1 against real ublk devices: the
destination refused before the handover, the source's `relinquish`
answered, the destination's `accept` then confirming, the source refused
afterward, and the filesystem mounted from the destination byte-identical.

**And then across two machines, which is the run that matters.** The same
sequence between lumen1 and lumen2, peered on TCP 7800 over the Core bond
rather than loopback: a vdisk created on one member and arriving on the
other by replication alone, exported as `/dev/ublkb6` there and
`/dev/ublkb7` here, both reading identically mid-window while the
destination's writes were refused and the source's were not, the pen moving
once, and the payload byte-identical mounted from the far machine — six
rounds, every scrub clean. Two details only a real pair could have taught.
The port is the proof the firewall service is load-bearing: 7800 falls
outside `lumen-replication`'s 7788–7799, so nothing but `lumen-pool` bound
to the Core interface's zone opens it. And `kernel.io_uring_disabled` had
been raised by hand on lumen1 and never persisted, so lumen2 still sat at
EL10's 2 and could not have exported at all — the sysctl drop-in is not
packaging tidiness, it is the difference between one node working and a
pool working.

**And the seam's window verb has changed shape to match.**
`set_two_primaries(device, allow)` was symmetric because DRBD's window is;
a lease handover names its destination and distinguishes success from
failure. It is now one method over an enum:

```rust
async fn migration_window(&self, device: &str, window: MigrationWindow) -> Result<()>;

enum MigrationWindow { Open { destination: String }, Accepted, Aborted }
```

DRBD's implementation collapses `Accepted` and `Aborted` into `allow=no`
and loses nothing, because which ending it was does not change what DRBD
must do; the LumenFS implementation will use all three. And
`VirtService::migrate` now says out loud whether its close is a success or
a failure — something it always knew and used to discard at the door. A
test asserts each ending reaches the storage layer, and fails if the
distinction is dropped.

This was the first change to reach the shipping crates, and it reached
only the seam: `lumen-drbd` (trait, `DrbdService`, `MockVmVolumes`) and the
two call sites in `lumen-virt::migrate`. Nothing about DRBD's behavior
changed — 38 storage tests, 96 compute tests, and the controlplane's suites
all pass untouched.

**The seam is implemented: `lumen-pool` exists.** The same `VmVolumes` the
DRBD path implements, over vdisks and writer leases — and a test asserts it
coerces to `Arc<dyn VmVolumes>` beside the DRBD one, because if that stops
compiling the integration is broken however well the logic passes.

Two facts about LumenFS shape the whole service, and neither was true of
DRBD. **Creating and deleting a vdisk replicates by itself**, so one member
is told and both have it — nothing to fan out, nothing to unwind halfway.
**Exports do not**: `/dev/ublkb<id>` is the same string on every member,
which is what keeps one domain document valid everywhere, but the device
exists only where the daemon exported it. So the device is materialized
where the machine is — here at create, and on the destination when a
migration window opens, where the export is penless by construction.

**Recorded deviation: there is no vdisk record.** The survey expected a
sibling record type on `ClusterRecord` mapping vdisks to names, the shape
`VolumeRecord` has. DRBD needs that because `/dev/drbd<minor>` says nothing
about which machine owns it — minors come from a pool, so the association
has to be written down. Here identity is *derived* and the mapping is
invertible:

```text
  vm-7-disk-3  →  vdisk 1795  →  /dev/ublkb1795  →  vm-7-disk-3
```

The vdisk id is the machine id shifted up a byte with the disk index in the
low byte, and the ublk device id is the same number. That removes a second
source of truth that could disagree with the engine about what exists: the
engine already knows its vdisks, and a derived name cannot go stale. The
cost is a stated ceiling — 256 disks per machine, machine ids below 2^24 so
the device id fits the `u32` the driver takes — and a name outside the
convention is refused rather than reshaped. The pool, brick, and slice-map
records phases 4 and 5 need are unaffected; those describe things no id can
encode.

A pleasant consequence: a freshly formatted brick's own vdisk 1 decodes to
machine 0, which is no machine, so `disk_of` correctly answers "not a
pooled machine disk" for it with no special case.

`common_members` returns the pool's members, and that is the whole of
pooled HA eligibility — placement is by content hash, so no member holds a
better share of any one disk than another, and `ha.rs` needs no change at
all.

**The fleet is real now too.** `SocketFleet` is a control connection per
member per call, each behind `spawn_blocking` — a connection per call
because these are administrative verbs rather than a data path, and holding
connections that can go stale while nobody is looking buys nothing but
liveness checks and half-open sockets discovered mid-verb. The node ids the
leases speak are *asked for* rather than configured: a daemon reports its
own id in `status`, and duplicating that in configuration is how the two
come to disagree. Four tests drive it against two real daemons over real
sockets — ids, replicated creation and deletion, and a lease actually
moving between two engines with the destination refused until the source
hands over — leaving only the ublk export untested here, since that needs a
kernel and lives in the appliance smoke scripts.

**And the failover gap is closed.** The sweep starts a machine on a
survivor without passing through a migration window, so nothing had
exported the device there. Investigating it corrected a wrong assumption
worth recording: the *lease* was never the obstacle — after a fence verdict
the era bumps, the dead holder's lease stops binding, and the attach claims
successfully. The only missing step was that nobody called `export`.

So the seam gained a verb, `ensure_local_device(device)`, and the compute
domain calls it in `start_domain` — the one chokepoint every start passes
through, `adopt` and ordinary starts alike. DRBD's implementation is a
lookup and nothing else, because its device exists wherever the resource
does; LumenFS exports if it is not already serving, leaving an existing
export alone rather than cycling it out from under whatever is using it.
Putting it on every start rather than only the HA path also fixes a case
nobody had hit yet: a daemon restart loses its exports, so even a normal
start of a stopped machine needs the device made again. A test asserts both
starts ready their devices, and fails if the loop is removed.

**The observed view exists, and typing it fixed the one reply that was
still prose.** A console page needs to know what the pool is doing right
now, so `lumen-pool` gained `state.rs` and `PoolService::state()`: per-member
replication state, era, brick space, and stream counters, joined with every
vdisk, the machine disk its id decodes to, who holds its pen, and which
members are actually serving it as a device. Read live and never stored — a
pool's health is a fact about this instant, and the same rule `lumen-drbd`'s
state module follows.

Two rules carried over from the DRBD side shaped these types more than
anything else. **A member that does not answer is presented, not dropped**:
a pool of two with one member unreachable is not a healthy pool of one, so
silence is a variant that carries its reason rather than a row that
disappears. And **a verdict is never better than its evidence**: with a
member silent this cannot tell whether the pool is fine or halved, so
`Unknown` exists and is deliberately *not* folded into `Degraded`. Both are
sabotage-tested — reversing the verdict's precedence, or filtering silent
members out of the view, each fails a named test.

Typing the status forced a wire change worth recording. Every other
machine-read reply became `key=value` in the control-surface slice, but
`status` was left as prose because nothing typed consumed it yet. It could
not have survived being parsed: `ReplState` derives `Debug`, and
`Resyncing { source: true }` renders **with spaces in it**, so a
space-separated line could never be read past `state`. The direction now
rides its own key — `state=resyncing sync=source` — absent for the other
three states rather than filled in with a lie, and a resync with no
direction is refused, because a target that refuses writes must never read
as a source that serves them. Nothing is defaulted on a parse failure
either: a status that reads "era 0, nothing exported" when it could not be
understood is how an orchestrator concludes a healthy pool is an empty one.
The formatter and the parser live in different crates, so the round trip is
pinned against a real daemon rather than a canned string, and the
appliance's smoke scripts were updated with it — the two-host migration was
re-run end to end on the new format.

One consequence went the other way and is worth the sentence: because the
daemon already puts the vdisk and lease listings in the status reply,
`MemberStatus` carries them. Asking separately would have cost a round trip
per vdisk to learn who holds each pen — and since the fleet opens a fresh
control connection per call, a fifty-disk pool would have cost fifty-one
connections to draw one page.

**And then the view was the thing that exposed a hole under the whole
seam.** Wiring a console route meant asking how a control plane reaches
each member's daemon, and the answer was that it cannot. The daemon's
control surface binds to loopback and stays there — the shipped unit passes
`--control 127.0.0.1:7799`, and `lumen-pool.xml` opens the peer link's
ports while pointedly not opening that one, because the control surface is
the local orchestration seam and `fence-peer` is on it. `SocketFleet`
addressed every member by socket, which is honest only where every daemon
happens to be loopback-reachable: two daemons in one test process. Two
machines was never tested, because the hardware runs drove each box's
control surface over its own ssh session rather than through the fleet.
Confirmed on the pair rather than reasoned about — lumen2's daemon bound to
`127.0.0.1:7799`, and lumen1 refused on both the management and the Core
address.

That is not a cosmetic gap. `migration_window(Open)` exports the disk on
the **destination**, `destroy_disk` unexports on **every** member, and the
observed view asks **each** member how it is doing — so live migration
between two machines could not have worked through the seam, and every
remote member would have read as permanently silent.

`peers.rs` is the fix, and its shape is the one the rest of the appliance
already uses: this node's daemon over loopback, every other member through
its own control plane, which then talks to *its* daemon over *its*
loopback. Each daemon is still only ever spoken to by the machine it runs
on, which is the property that let the control surface be loopback-bound in
the first place. `PoolVerb` is a **closed enum**, not a command string — a
member being able to ask a peer to run one of twelve named verbs is a
reviewable thing and "run whatever you like" is not — and `execute` is the
single definition of what each verb does, called by whichever machine owns
the daemon, so a local verb and a remote one cannot drift apart. Both
routing directions are sabotage-tested: sending the local member through
the peer channel fails a named test, and so does dialling every member
directly, which is the original bug.

**The snapshot vertical, which the engine had and nothing above it did.**
`lumen-fs` has had `snapshot_vdisk`, `delete_snapshot`, `rollback_vdisk`
and `snapshots()` since phase 1, but `GuestHandle` exposed none of them, so
neither did the control surface, the typed client, the peer verbs, the
fleet, or the service. All four now run the whole way up, and they are
replicated operations like vdisk creation — one member is told and both
have it, which is the only reason a snapshot is worth offering: one that
lived on the member that took it would be gone exactly when it was wanted,
after that member died.

**Rollback needed a guard built rather than inherited.** The engine swaps
the map root whenever it is asked; it has no idea a guest is mounted on the
other side of a ublk device, so a rollback under a running machine would
replace every block beneath a live filesystem without telling it. The
contract — refused while the disk is open anywhere — is therefore enforced
twice on purpose. The daemon refuses to roll back a disk *it* is serving,
because it must not depend on being called politely; and the service asks
**every** member what it is serving first, so a disk open only on the far
member still refuses. A member that cannot be reached refuses it too: it
might be the one serving the disk, and "I could not check" must never read
as "nobody has it". Both halves are sabotage-tested, and the daemon's own —
which no off-appliance suite can reach, since WSL has no `ublk_drv` — was
verified on lumen1 against a real device: refused while served, filesystem
untouched by the refusal, and after an unexport the rollback restored the
snapshot's bytes exactly.

**And a pool says whether it exists in one place.** `config.rs` reads the
daemon's own drop-in, `/etc/lumen/fsd.conf` — the `EnvironmentFile` the
shipped unit already reads. Its presence is the answer to "is there a pool
here"; membership is not in it and does not belong in it, because a pool
spans its cluster and the control plane already knows who those members
are. A second file or a replicated record would only be a second thing to
disagree. A file that exists but does not say what it must is an error
rather than an absent pool, so a half-written drop-in cannot hide behind a
console page that cheerfully reports nothing to show.

**Phase 3 closes with the control plane and the console.** The peer
channel's pool half is `impl PoolPeers for HttpPeerChannel` plus one route,
`/api/peer/pool/verb`: a member's control plane asks another to run one of
the closed enum's verbs against its own daemon over its own loopback, under
the same peer tickets and environment CA as every other member-to-member
call — and there is deliberately no local short-circuit, because
`PeeredFleet` already routes the local member straight to loopback, so a
request for this node arriving over HTTPS is a routing bug and is refused
by name rather than buried. Writing that route's test found a bug shipped
with the enum itself: `PoolAnswer` was internally tagged, and internal
tagging cannot serialize a newtype variant holding a sequence or a string —
`Vdisks` and `Device` would have failed at runtime on the first real call.
The round-trip test had covered `Status`, the one variant whose content is
a map, which is the one shape internal tagging happens to allow. The enum
is adjacently tagged now and the test sweeps **every** variant of both
enums.

The control plane decides whether this node carries a pool the same way
everything else about the pool is decided — by reading, not remembering:
`PoolPresence` is `Absent` (no drop-in: the standalone appliance, or a
DRBD cluster), `Broken` (a drop-in that exists but could not be assembled,
shown as its own sentence because "nothing to show" and "your deployment
is broken" must never look alike), or `Present`. On a pooled node the pool
service **is** the `VmVolumes` the compute domain gets — the seam's
promise, kept in `main.rs` with four lines and no compute change — and the
engine choice is exactly "one engine per cluster ever" made mechanical.

The operator surface is `GET /api/storage/pool` (the observed view, one
read) plus snapshot/delete/rollback routes addressed by the disk's *name* —
`vm-7-disk-3`, the same fact as the device path without the slashes in it —
and the console's Volumes page gains a Pooled Storage section beside the
replicated one: members with replication state, era, and brick space
(a silent member shown unreachable with its reason, never dropped), vdisks
with their writer as a member's name, an open migration window as
`source → destination`, and the snapshot dialog — take, list, delete, and
rollback behind the same acknowledgement the DRBD dialog requires, with
snapshot ids minted as Unix seconds so the list reads as history. At most
one of the two sections renders, because a cluster runs one engine.

The route tests run the real router: the observed view changing as the
verbs run, the rollback guard refusing unacknowledged and refusing while
served, the name-only addressing, and the peer verb executed against a
**real daemon** on loopback — plus the two authentication rules, a browser
cookie never opening the peer surface and a verb outside the closed enum
never running anything.

Pool *creation* stays with phase 4's drive wizard, where choosing which
disks become bricks belongs — until then `/etc/lumen/fsd.conf` is written
by hand, and `PoolPresence` reads whatever it says. Owed to the burn-in
ledger rather than to phase 3: a seam-driven migration between the two
real machines (the fleet and peer route are proven against real daemons
in-process; the two-controlplane run needs a pool wired under the
appliances' own control planes, which is the deployment phase 4 builds).

**Phase 4 built that deployment, and its exit test collected the debt
(complete, 2026-07-30).** Tier is on the platter now: superblock v2
carries the brick's tier and a WAL-holder flag, the anchor carries a
roster of every brick its node serves, and decode checks the version
*before* the checksum so a v1 brick is refused by name rather than
misread as blank. v2 is reformat-only — Cody's call: no decode compat,
no in-place upgrade, because the wizard rebuilds a pool from bare disks
anyway and the only v1 pool in existence was our own test rig. The disk
scanner tries the same decode against sector 0 of every candidate disk,
so the Disks page, the ZFS pickers, and the wizard tell one ownership
story from one definition — a disk is a ZFS pool's or a LumenFS brick's,
never both, and the boot pool is nobody's to take.

A node serves a `BrickSet` rather than a brick: sorted by (tier, uuid),
exactly one WAL holder, dedupe strictly per tier — a block's tier is part
of its home — and allocation to the most-free brick of that tier with
ties broken by lowest uuid, a pure function of durable state that every
replay reaches again. A set that does not match its own roster is refused
by name, as is a set with no tier-0 brick for the WAL to live on.
Capacity is finally said in bytes: per tier, pool-usable is the **min
over members** — never a sum divided by anything — and `None` while any
member is silent, because a guess is not a figure. The label is *usable*,
captioned that dedupe only makes it bigger.

Creation moved from the hand-written conf to the wizard and its
workflow. The coordinator computes the whole plan up front — pool and
brick uuids minted once and passed to every format, node ids from the
sorted member names, listener and dialer assigned — then per-member
prepare runs sequentially: wipe, format each brick, write the conf from
`PoolConfig::render()` (round-trip-tested against its own parser),
enable the daemon, verify it answers on loopback. Any failure unwinds
the prepared members in reverse before a single control plane has
restarted. Adoption is restart choreography: peers first, each polled
back to Present; the coordinator marks the job Complete and restarts
itself **last**, and the console switches from the progress feed to the
observed pool when the feed's own server goes down — truth over feed.
Consent never travels a peer route. Destroy refuses while any vdisk
exists or any member cannot be asked (silence could hide a vdisk), with
one escape: a Broken pool destroys with the acknowledgement, because
that is the repair path out of Broken.

The exit run on lumen1/lumen2 drove destroy and create through the real
API, verified the conf, unit, and status on the hosts against what the
workflow claims to write, exercised the refusals (a v1 brick and a boot
disk both named in a failed create; destroy answering 409 while a vdisk
existed), and paid the owed migration: a machine on a pool vdisk moved
lumen1 → lumen2 → lumen1 through the two appliances' own control planes,
each leg landing in seconds with the writer lease and the export
following the machine and both consoles agreeing. What the run added to
the ledger, every entry a thing no simulation had said:

- **`wipefs` cannot see a LumenFS superblock**, so "wipe then format"
  left the old pool's identity on the platter until the wipe learned to
  zero the first 16 KiB with `dd` before asking `wipefs` for the rest.
- **Two concurrent creates split the pool's identity** — each member
  formatted under a different coordinator's minted uuid, and the daemons
  politely refused each other's handshakes forever. The job slot is now
  claimed atomically (`try_begin`); the second create answers Conflict
  instead of racing.
- **The `Accepted` window arm never took the source's device down.** The
  first migration landed and the return leg was refused with "already
  exported": the handover moved the pen but left the source's penless
  export standing — not a second writer, but exactly the export that
  refuses the machine's way back. `Accepted` now unexports the source
  after the destination confirms the pen, the mirror of what `Aborted`
  does to the destination — and the seam test that had asserted only the
  lease's movement now asserts the device's too.
- **Destroying what is already gone is already done**: a stale DRBD
  record from an earlier hand-torn cluster could never be deleted because
  `drbdadm down` on a resource with no config is an error the CLI now
  recognises as success.
- The daemon's one-at-a-time control socket **wedges on a half-open
  connection** (a killed client mid-exchange). Real, reproduced, and a
  follow-on: the control surface needs read deadlines.
- The migration URI's libvirt listener is the deliberate loose end
  docs/storage.md records: the firewalld service ships but the TCP
  listener is an operator's own security decision. It was enabled by
  hand on both machines (`auth_tcp = "none"`, reachable only on the
  Core-zone interfaces) for the exit test — packaging an opinionated
  default remains open.
- A machine deleted without purging its disks leaves a pooled volume
  **no operator surface can currently reap** — the replicated-volume
  delete route only lists DRBD clusters. Follow-on for the console.

Phase 4's stated non-goals, decisions rather than surprises: no live
brick-add (create and destroy only; the anchor roster is already shaped
so growth can land without a format change), no WAL-brick relocation
(retiring the WAL disk is evacuate-and-reformat via peer resync), no
tier spill (a write to a full tier fails; it never silently lands on
another tier), and no per-tier slice maps yet — that is phase 5's
placement arithmetic, already begun below.

**Phase 5 has begun with placement, which is pure arithmetic and so goes
first.** `slice.rs` is the whole of "which members hold a block": hash →
slice → an ordered pair of members, 256 slices per tier, a slice being one
byte of the digest. Nothing consults a directory and no block's location
is written down anywhere, which is the same sentence that makes dedupe and
placement one mechanism instead of two.

A member set of `n` gets the `n` ring pairs dealt round-robin across
slices, so at three the pattern is `(A,B), (B,C), (C,A)` exactly as this
document already named it, and at two it degenerates to both members
everywhere — every read local, the appliance's center of gravity
preserved by construction rather than by a special case.

One number deserves stating because it surprises people: at three members
with RF=2, **each member holds about two thirds of the unique data**, not
one third. There are `2 × 256` seats however many members exist, so three
split 512 seats about 171 each across 256 slices. That is also the cost of
adding the third member — it must be filled to two thirds before the pool
is balanced — and it is what buys capacity beyond any single node, which
is the only reason a vdisk larger than a node is possible.

A consequence worth noticing early: because placement follows the hash,
every vdisk's blocks spread evenly over every slice, so **every member
holds about two thirds of *every* vdisk**. No member is a better host for
a given machine than any other, which means HA eligibility needs no
locality hint — the seam's answer really can be "any member that can reach
a quorate pool".

Reassignment is one call for what look like three operations. Growing to a
third member, shrinking back to two, and re-protecting after a death are
all `reassigned(version, members)`; there is no separate re-protection
algorithm to keep honest. It keeps a slice's existing homes wherever the
balance allows and **never takes both homes of a slice in one step**, so
every slice always retains a member that already holds its blocks: a
source for the copy, and a reader for anyone asking while it runs. The one
case arithmetic cannot rescue — both homes of a slice gone at once, RF=2
exhausted — is reported by name rather than papered over with a fresh
assignment pointing at members that hold nothing.

Eleven tests pin it, including the invariants that matter for running it
against a live pool: no slice stranded on growth, every move sourced from
a member that really holds the slice, a single death among three never
orphaning anything, a no-op reassignment copying nothing, and the same
inputs always producing the same map on every node regardless of the order
members are listed in.

What remains for phase 5 is the protocol, and its three open questions are
now decided (Cody, 2026-07-29) — recorded here so the code that follows
does not relitigate them:

- **Metadata is replicated to every member; only data blocks slice.** Map
  trees and manifests go everywhere, so garbage collection, scrub, and
  pool-open stay local operations over durable local state — the property
  every invariant this engine has earned depends on. The cost is roughly
  three copies of about 0.2% of stored bytes. The alternative, slicing
  metadata too, would have made collection a distributed protocol to save
  a rounding error of disk.
- **A write acknowledges on its two data homes**, as docs/storage.md
  already specifies. A slow third member adds no latency; it applies the
  op stream behind and catches up by Merkle resync. Its metadata may lag,
  which matters only when it starts a vdisk — and that resyncs first
  anyway.
- **A non-home node fetches on demand and keeps nothing.** About a third
  of reads cross the Core network at three members, and that is the
  honest price. A cached block would be a third copy that collection must
  account for and scrub must verify — a second liveness problem beside
  the one already solved. Measure before adding it; nothing in the format
  precludes it later.

Sequencing is engine-first: the placement and replication core lands in
`lumen-fs` under the simulation, leaving the cluster record, the
workflows, and the console untouched, so nothing destabilizes while DRBD
carries production and each piece stays abandonable.

**Phase 5's protocol is in (complete, 2026-07-31), and it went engine
out.** The two-node core became per-peer sessions — every peer its own
state, its own dense op stream, its own resync, sends carrying their
addressee — with the acknowledgement rule restated as per-peer *needs* no
event may blanket-drain: a data write waits on its block's homes, every
other op waits on every live member, and a need from a dead session
settles only by that peer's adoption or its fence verdict. The verdict
carries its era now, because two survivors computing `max+1` from their
own vantages can mint two different numbers and the next hello would read
the difference as a fence that never happened. The restructure's own
light found two holes the two-node suite had never lit — a payload swept
by a collection in the gap before its op, and one global resync pull any
hello could clobber — both closed and pinned.

Then the map arrived: data placed by hash on exactly its slice's two
homes, metadata everywhere, non-homes fetching on demand and keeping
nothing (the serve-once buffer is consumed on read, and the tests assert
block counts do not move). The rejoin became **concurrent and
per-vdisk** — a returning member pulls from every live peer at once and
adopts each vdisk from *its lease holder's* offer — because the
serialized design, walked honestly, ends with an equal-era tie-break
discarding a live writer's acknowledged history. Membership then learned
to change under a serving pool: the committed map persists in manifest
v4 (v3 stays decodable — it is live on real machines — and an unplaced
pool still writes it), a reassignment opens as a pending map every write
straddles (old ∪ new homes) and every collection respects, the moves are
Merkle-idempotent fetches from homes the arithmetic guarantees survive,
and the commit is what licenses the displacement drop — the new GC rule
without which a reassigned-away member would carry its old slices
forever. A member behind on the map cannot even elect resync roles: the
hello refuses across the gap and the higher side ships the map whole,
with a second hello behind it, because the first was spent teaching.

The daemon meshed — links keyed by handshake, one listener and
lower-id dials, per-member verdicts and the reassignment as three
control verbs — and the mesh test promptly caught a write's
read-modify-write edge needing the same fetch loop a read already had.
The pool layer's capacity figure became seat arithmetic: each member
bounds the pool at `bytes × 256 / seats`, which *is* min-over-members at
two and genuinely exceeds any one node at three. Create accepts two or
three seats (more is refused by name — the map arithmetic is proven to
eight, the protocol to three), new pools are created **placed** so
growth needs no reformat, and growing is its own operator act — `POST
/api/storage/pool/members`, a newcomer prepared, every serving member
taking one new dial, reassign, rebalance, commit, restart — rather than
a rider on the cluster's node-add, because the newcomer's disks are a
choice nothing can infer.

Validated under the simulation and against real daemons in-process
(three on real sockets: placement, fetched reads, a per-member verdict,
a rejoin, and a shrink driven entirely over control verbs), plus the
two-member regression on lumen1/lumen2 — the standing pool destroyed and
recreated through the new path, coming back placed (`map=1 seats=256`)
with the same usable figure. **Owed, recorded, and waiting on hardware:
the three-box burn-in.** There is no third machine yet; the day one
exists, the exit test is the grow workflow against it, then the full
canary suite at three. Also recorded: between a cluster node-add and the
pool grow, the observed view lists the new cluster member as a silent
pool member (identity is derived from the cluster on purpose) — the
grow heals it, and the wart is the price of having no second membership
record to disagree. The console's grow dialog is a follow-on; the API
carries the workflow today.

**The program closed with its debts, not just its features (2026-07-31).**
Phase 6 is descoped by the fact it planned for: there is no DRBD
deployment to migrate (see the phase list). Closing the ledger surfaced
one real defect and paid two recorded follow-ons. The defect: the HA
sweep and the maintenance drain asked **DRBD by name** for placement
eligibility (`state.drbd.common_members`) instead of the engine the node
runs — on a pooled cluster, every HA machine was unrestartable and every
drain refused, because DRBD answers a `/dev/ublkb` device with a
refusal. Both now ask through `VirtService::common_members`, the same
seam the machines run on, and DRBD's replica-currency check constrains
only the devices DRBD can actually see. The follow-ons: the daemon's
one-at-a-time control surface now times out a silent connection (the
half-open wedge the phase-4 exit test hit on real hardware), and an
orphaned pooled volume — kept when its machine was deleted without a
purge — finally has a reap surface: `DELETE
/api/storage/pool/disks/{name}`, refused while any defined machine
still references the device. What remains open is exactly what the
ledger says: the three-box burn-in, the preview label that only
scrub-clean time on real hardware can lift, and the console's grow
dialog. The virtproxyd enablement decision closed the same day it was
named: cluster prepare enables the listener and teardown disables it —
the same resolution the firewall bindings reached, the workflow that
needs it being the one that turns it on (see `docs/storage.md`).

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
firewalld service pattern for its ports — the `lumen-pool` service
definition, bound at prepare on the Core interface's existing zone exactly
as `lumen-replication` is, and never by moving the interface into a zone of
ours. The peer
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
   there are none. *Descoped (Cody, 2026-07-31): there are none.* No
   deployment ever carried DRBD volumes into production — the only
   machines running Lumen are the test pair, reinstalled fresh with a
   pool — so migration tooling would ship with a user population of
   zero. The decision is recorded rather than the phase quietly skipped:
   if a DRBD cluster ever materializes, this phase is where its path
   lives, and DRBD itself remains in the tree as the shipped alternative
   until removed by its own decision.

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
