//! Two-node synchronous replication: the phase-2 core of docs/lumenfs.md,
//! as a sans-IO state machine the simulation can torture.
//!
//! One [`ReplNode`] wraps one node's [`Pool`]. It never touches a socket or
//! a clock: the daemon (later) and the test harness (now) deliver peer
//! messages to [`ReplNode::handle`], tell it about links and verdicts
//! ([`ReplNode::peer_lost`], [`ReplNode::set_peer_fenced`]), and drain its
//! [`Effect`]s — messages to send, guest flushes to acknowledge or fail.
//!
//! ## What replicates, and why so little
//!
//! Only the operation stream and its payloads cross the wire. Each node
//! owns its WAL, its checkpoints, its anchor, its garbage collection —
//! none of it coordinated — because map trees are canonical: the same
//! mappings fold to the same roots on any brick. Ship the ops, and the
//! nodes converge by construction.
//!
//! ## The acknowledgement rule
//!
//! A guest flush completes only when everything before it is durable on
//! **both** nodes (the peer's `Durable` answer), or when a fence verdict
//! says there is only one node left to ask. That is DRBD protocol C's
//! guarantee, restated: writes are not promises until both replicas hold
//! them or the cluster has vouched the peer is dead.
//!
//! ## States, and who decides them
//!
//! ```text
//!   Synced ──peer_lost──▶ Suspended ──set_peer_fenced──▶ Degraded
//!     ▲                       │  ▲                           │
//!     │                    (writes refuse,        (era bumped past every
//!     │                     flushes park)          witnessed era; writes
//!     │                                            go on alone)
//!     └── Resyncing ◀──Hello exchange on reconnect──────────┘
//!          (the target refuses writes; a source with era superiority
//!           keeps serving guests and streams its ops as it goes)
//! ```
//!
//! The engine never decides death. `set_peer_fenced` is the cluster's
//! verdict (Pacemaker's fence confirmation, docs/cluster.md) arriving from
//! above; without it a partitioned node suspends forever — integrity over
//! availability, always. The era (anchored, see format.rs) is what makes
//! the verdict durable: the survivor bumps it before writing anything new,
//! so whichever node returns with the lower era knows to adopt.
//!
//! ## Resync is a Merkle diff, and the guests never notice
//!
//! The returning node pulls: the source checkpoints and offers its roots;
//! the target walks down from each root it lacks, fetching only subtrees
//! whose hashes it does not hold, then adopts the source's manifest whole.
//! Content addressing makes this correct with no dirty bitmap and no
//! bookkeeping — identical subtrees are skipped because they are
//! *identical*, not because someone remembered they were. The target's own
//! divergent unacknowledged history is discarded by the adoption; that is
//! the point.
//!
//! A source whose era is strictly higher — the fence-verdict survivor —
//! **keeps serving guests while the target pulls**. Its writes stay in the
//! single-copy acknowledgement regime the verdict already authorized, and
//! everything from the offer's checkpoint onward streams to the target as
//! ordinary ops. The target buffers the stream and replays it after
//! adopting the offer, which lands it exactly on the source's live state:
//! offer + post-offer ops, no gap and no overlap, because streaming starts
//! at the same checkpoint the offer was cut from. Two consequences carry
//! the correctness: the offered roots are pinned against garbage
//! collection on both ends (the source's own checkpoints orphan them; the
//! target's fetched fragment is referenced by nothing yet), and a fence
//! verdict bumps the era past every era this node has *witnessed*, not
//! just its own — a survivor fenced mid-pull would otherwise mint a second
//! copy of the era it was adopting, and two divergent lineages would tie.
//!
//! The resync ends with a handshake, and the handshake is load-bearing.
//! Adoption hands the target the source's era, and from that instant the
//! eras can no longer order the two nodes — so a single-copy
//! acknowledgement issued after it would be a write that a later
//! equal-era tie-break could discard. Therefore the target does not adopt
//! when its pull completes; it sends `SyncReady`, the source stops
//! serving *first* and answers `SyncAdopt`, and only then does the target
//! adopt, replay the buffered stream, flush, and confirm with `SyncDone`.
//! The source refuses writes for exactly that closing exchange — a round
//! trip plus whatever stream is still in flight, the daemon's
//! queue-and-retry window — where the pull itself may take as long as it
//! likes.
//!
//! When the eras are equal — a clean partition healing, no verdict, both
//! nodes holding every acknowledged write — the source refuses writes for
//! the duration, as before. The diff is cheap by construction there, and
//! without a verdict there is no honest way to acknowledge single-copy.
//! The tie runs the same closing handshake; it simply has nothing to
//! stop doing.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::disk::Disk;
use crate::error::{FsError, Result};
use crate::hash::BlockHash;
use crate::map;
use crate::pool::{Lease, Pool};

pub type NodeId = u8;

/// One vdisk in a sync offer: id, size, checkpointed root.
pub type VdiskOffer = (u64, u64, Option<BlockHash>);
/// One snapshot in a sync offer: vdisk, snapshot, size, pinned root.
pub type SnapshotOffer = (u64, u64, u64, Option<BlockHash>);

/// Everything a resync source offers: the settled state a target walks
/// toward and then adopts whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOffer {
    pub era: u64,
    pub vdisks: Vec<VdiskOffer>,
    pub snapshots: Vec<SnapshotOffer>,
    pub leases: Vec<(u64, Lease)>,
}

/// How many blocks a resync target asks for per round trip.
const SYNC_BATCH: usize = 64;

/// An offer's roots as GC pins, `(root, depth)` — computed identically on
/// both ends, because both ends pin them: the source so its own progress
/// cannot collect the offer, the target so its own collection cannot eat
/// the top-connected fragment it has fetched so far.
fn offer_pins(offer: &SyncOffer, block_size: u32) -> Vec<(BlockHash, u32)> {
    let entries = map::entries_per_node(block_size);
    let block = block_size as u64;
    let mut pins = Vec::new();
    for (_, size_bytes, root) in &offer.vdisks {
        if let Some(root) = root {
            pins.push((*root, map::depth_for(size_bytes.div_ceil(block), entries)));
        }
    }
    for (_, _, size_bytes, root) in &offer.snapshots {
        if let Some(root) = root {
            pins.push((*root, map::depth_for(size_bytes.div_ceil(block), entries)));
        }
    }
    pins
}

/// One replicated operation — the wire form of the pool's mutating API.
/// Payloads travel separately ([`PeerMessage::Payloads`]); an op references
/// data only by content address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplOp {
    CreateVdisk {
        id: u64,
        size_bytes: u64,
    },
    Write {
        vdisk: u64,
        index: u64,
        hash: BlockHash,
    },
    Trim {
        vdisk: u64,
        index: u64,
    },
    DeleteVdisk {
        id: u64,
    },
    Snapshot {
        vdisk: u64,
        snapshot: u64,
    },
    DeleteSnapshot {
        vdisk: u64,
        snapshot: u64,
    },
    Rollback {
        vdisk: u64,
        snapshot: u64,
    },
    Clone {
        new_id: u64,
        vdisk: u64,
        snapshot: u64,
    },
    /// The whole lease, not a request for one: the peer applies what the
    /// holder decided rather than deciding again. Every lease change —
    /// claim, window open, handover, abort — travels as one of these.
    SetLease {
        vdisk: u64,
        lease: Lease,
    },
}

/// The protocol. In-memory shapes for now — the daemon owns wire encoding,
/// exactly as it owns sockets; the simulation passes these by value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerMessage {
    /// Who I am and which era my state belongs to. Opens every connection;
    /// both sides derive the same source/target roles from the pair.
    Hello { era: u64, node: NodeId },
    /// Data blocks ahead of the ops that reference them.
    Payloads(Vec<Vec<u8>>),
    /// Ops in stream order. `first_rseq` numbers the first op; the rest
    /// follow sequentially — a gap means the transport lied.
    Apply { first_rseq: u64, ops: Vec<ReplOp> },
    /// Make everything up to `up_to` durable and say so.
    Flush { up_to: u64 },
    /// Everything up to `up_to` is durable here.
    Durable { up_to: u64 },
    /// Target asks the source to checkpoint and offer its state.
    SyncStart,
    /// The source's settled state, roots and all.
    SyncManifest(SyncOffer),
    /// Blocks the target lacks.
    SyncNeed(Vec<BlockHash>),
    /// The blocks, payloads only — each self-identifies by its hash.
    SyncData(Vec<Vec<u8>>),
    /// The pull is complete; the target asks leave to adopt. This is the
    /// source's cue to stop acknowledging alone: adoption hands the
    /// target the source's era, and a single-copy acknowledgement issued
    /// after that would be a write the eras can no longer order — a tie
    /// the tie-break would resolve by discarding it.
    SyncReady,
    /// The source has stopped serving; every op it will ever send under
    /// this session's stream precedes this message, and there are exactly
    /// `final_rseq` of them. The target may now adopt and replay.
    SyncAdopt { final_rseq: u64 },
    /// The target has adopted; both sides return to Synced.
    SyncDone { era: u64 },
}

/// What the node wants the outside world to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Send(PeerMessage),
    /// The guest flush with this ticket is fully acknowledged.
    FlushDone(u64),
    /// The guest flush with this ticket can never complete honestly — its
    /// writes were discarded by an adoption. The guest sees an error, not
    /// a lie.
    FlushFailed(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplState {
    /// No peer link and no verdict: reads only, writes refuse, flushes park.
    Suspended,
    /// Peer in lockstep: every acknowledgement is two-node durable.
    Synced,
    /// Alone under a fence verdict, at a bumped era.
    Degraded,
    /// Reconciling with a returning peer. The target refuses writes — its
    /// state is about to be replaced. A source with era superiority keeps
    /// serving guests, streaming as it goes; a source at an equal era
    /// refuses too, because no verdict backs a single-copy promise.
    Resyncing { source: bool },
}

/// The resync target's walk state.
///
/// Hashes carry a **kind**: `0` is a data block (nothing below it), and
/// `k > 0` is a map node whose children are of kind `k - 1`. A root of a
/// depth-`d` tree is kind `d`, so a depth-1 tree's root is kind 1 and its
/// children are data. Getting this shifted by one is how a resync silently
/// stops descending — hence the explicit name.
#[derive(Debug, Default)]
struct SyncPull {
    /// Hashes to resolve locally: present ones get walked, absent ones get
    /// requested.
    pending: Vec<(BlockHash, u32)>,
    /// `(hash, kind)` pairs already resolved — shared subtrees are common,
    /// and a diff must not walk one twice.
    seen: HashSet<(BlockHash, u32)>,
    /// Absent here: hash → the kinds it is wanted as.
    wanted: HashMap<BlockHash, Vec<u32>>,
    /// Wanted but not yet requested.
    unrequested: Vec<BlockHash>,
    /// Requested and not yet received.
    outstanding: usize,
    manifest: Option<SyncOffer>,
}

pub struct ReplNode<D: Disk> {
    pool: Pool<D>,
    node: NodeId,
    state: ReplState,
    effects: VecDeque<Effect>,
    /// Numbering for ops sent (writer side).
    next_rseq: u64,
    /// Peer-confirmed durability (writer side).
    durable_rseq: u64,
    /// Last op applied (applier side).
    applied_rseq: u64,
    /// Guest flushes waiting on the peer: `(ticket, needs_rseq)`.
    parked: Vec<(u64, u64)>,
    next_ticket: u64,
    pull: SyncPull,
    /// Sourcing this resync with era superiority: guests keep writing here,
    /// acknowledged single-copy under the verdict that made the era.
    serving_source: bool,
    /// The offer has been cut; every op from here on streams to the target.
    /// Distinct from `serving_source` because writes accepted *before* the
    /// offer's checkpoint are inside the offer — streaming them too would
    /// deliver them twice, and half the ops are not idempotent.
    stream_live: bool,
    /// The highest era any peer has ever claimed to this node — the floor
    /// a fence-verdict bump must clear. Volatile: the narrow hole (hear an
    /// era, crash before adopting it, then survive a verdict) is recorded
    /// in docs/lumenfs.md rather than closed.
    peer_era_seen: u64,
    /// The target's buffer of the source's live stream, replayed in
    /// arrival order after adoption. Bounding it is flow control, which
    /// belongs to the daemon that owns the socket.
    backlog: Vec<PeerMessage>,
}

impl<D: Disk> ReplNode<D> {
    pub fn new(pool: Pool<D>, node: NodeId) -> ReplNode<D> {
        ReplNode {
            pool,
            node,
            state: ReplState::Suspended,
            effects: VecDeque::new(),
            next_rseq: 1,
            durable_rseq: 0,
            applied_rseq: 0,
            parked: Vec::new(),
            next_ticket: 1,
            pull: SyncPull::default(),
            serving_source: false,
            stream_live: false,
            peer_era_seen: 0,
            backlog: Vec::new(),
        }
    }

    pub fn state(&self) -> ReplState {
        self.state
    }

    pub fn node(&self) -> NodeId {
        self.node
    }

    /// The pool, for reads and verification. Mutation goes through the
    /// node, which is what keeps the peers honest.
    pub fn pool(&self) -> &Pool<D> {
        &self.pool
    }

    pub fn into_pool(self) -> Pool<D> {
        self.pool
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        self.effects.drain(..).collect()
    }

    fn emit(&mut self, effect: Effect) {
        self.effects.push_back(effect);
    }

    // -----------------------------------------------------------------
    // Links and verdicts — the outside world's facts.

    /// The link is up; say hello. Role assignment happens when the peer's
    /// own hello arrives.
    pub fn connect(&mut self) {
        self.emit(Effect::Send(PeerMessage::Hello {
            era: self.pool.era(),
            node: self.node,
        }));
    }

    /// The link is gone. Whatever was in flight is gone with it; flushes
    /// stay parked until a verdict or a reconciliation decides their fate.
    ///
    /// Queued peer messages are dropped here, and that is load-bearing
    /// rather than tidy. A message written for a connection that has died
    /// must not be delivered over the next one: a reply to a request from
    /// a session that no longer exists can arrive inside a fresh session
    /// and be mistaken for an answer to it. This is the engine's half of
    /// the contract; the daemon's half is to never carry bytes across
    /// connection incarnations. Guest-facing effects are not messages and
    /// survive — a parked flush is still owed an answer.
    pub fn peer_lost(&mut self) {
        self.effects
            .retain(|effect| !matches!(effect, Effect::Send(_)));
        self.state = ReplState::Suspended;
        self.pull = SyncPull::default();
        // A buffered stream from a dead session must never replay into the
        // next one: the next offer already contains those writes, and the
        // next stream will reuse their sequence numbers.
        self.backlog.clear();
        self.serving_source = false;
        self.stream_live = false;
        self.pool.unpin_sync();
    }

    /// The cluster's verdict: the peer has been fenced. Not this crate's
    /// decision — it arrives from the machinery that owns quorum and
    /// fencing (docs/cluster.md), and it is the only thing that turns
    /// suspension into solitary progress. The era bump is anchored before
    /// any new state exists under it, and every parked flush completes:
    /// single-copy durable is what acknowledged means now.
    pub fn set_peer_fenced(&mut self) -> Result<()> {
        if self.state != ReplState::Suspended {
            return Err(FsError::Corrupt(
                "a fence verdict arrived for a peer that is not lost",
            ));
        }
        self.pool.bump_era(self.peer_era_seen)?;
        // The bump retired every lease of the era it closed — including
        // this node's own. The dead peer's leases must stay retired, so a
        // failover can claim them; but a guest already running *here* must
        // not lose its pen to its peer's death, so leases this node held
        // are re-issued under the new era. A handover window open toward
        // the dead node closes in the same stroke, which is the abort that
        // migration was owed.
        let mine: Vec<u64> = self
            .pool
            .leases()
            .into_iter()
            .filter(|(_, lease)| lease.holder == self.node)
            .map(|(vdisk, _)| vdisk)
            .collect();
        for vdisk in mine {
            self.pool.claim_lease(vdisk, self.node)?;
        }
        let parked = std::mem::take(&mut self.parked);
        for (ticket, _) in parked {
            self.emit(Effect::FlushDone(ticket));
        }
        self.state = ReplState::Degraded;
        Ok(())
    }

    /// Take the writer role for a vdisk. Legitimate while degraded — the
    /// HA restart after a fence — or while synced, where it replicates as
    /// an explicit claim. (The migration-window handover under a live
    /// guest is phase 3.)
    pub fn claim_writer(&mut self, vdisk: u64) -> Result<()> {
        self.lease_change(vdisk, |pool, node| pool.claim_lease(vdisk, node))
    }

    /// Open the migration window. The guest keeps running — and keeps
    /// writing — here; the destination may open the disk alongside.
    pub fn begin_handover(&mut self, vdisk: u64, to: NodeId) -> Result<()> {
        self.lease_change(vdisk, |pool, node| pool.begin_handover(vdisk, node, to))
    }

    /// Take a lease that was handed to this node: the instant the guest
    /// becomes ours. One durable step, so there is no moment in which both
    /// nodes could write.
    pub fn accept_handover(&mut self, vdisk: u64) -> Result<()> {
        self.lease_change(vdisk, |pool, node| pool.accept_handover(vdisk, node))
    }

    /// The migration failed. The window closes and the disk stays where it
    /// was — every path out of a migration closes it.
    pub fn abort_handover(&mut self, vdisk: u64) -> Result<()> {
        self.lease_change(vdisk, |pool, node| pool.abort_handover(vdisk, node))
    }

    /// Apply a lease change locally and replicate whatever it settled on.
    /// The peer is sent the resulting lease rather than the request, so the
    /// two sides cannot reach different conclusions from the same words.
    fn lease_change(
        &mut self,
        vdisk: u64,
        change: impl FnOnce(&mut Pool<D>, NodeId) -> Result<()>,
    ) -> Result<()> {
        if !self.serving() {
            return Err(FsError::Suspended);
        }
        let node = self.node;
        change(&mut self.pool, node)?;
        if let Some(lease) = self.pool.lease(vdisk) {
            self.send_ops(vec![ReplOp::SetLease { vdisk, lease }]);
        }
        Ok(())
    }

    /// What the durable lease says about this vdisk.
    pub fn lease(&self, vdisk: u64) -> Option<Lease> {
        self.pool.lease(vdisk)
    }

    // -----------------------------------------------------------------
    // The guest-facing write path.

    /// Whether a guest write would be accepted right now — what a daemon
    /// consults to queue-and-retry rather than surface an error, most
    /// usefully in the one-round-trip window at the end of a streamed
    /// resync when the source has stopped serving but is not yet Synced.
    pub fn accepts_writes(&self) -> bool {
        self.serving()
    }

    /// Whether guests may be served mutations here at all: in lockstep,
    /// alone under a verdict, or sourcing a resync under the era that
    /// verdict minted — the one resync role that owes its guests
    /// continuity.
    fn serving(&self) -> bool {
        match self.state {
            ReplState::Synced | ReplState::Degraded => true,
            ReplState::Resyncing { source: true } => self.serving_source,
            _ => false,
        }
    }

    fn writable(&self, vdisk: u64) -> Result<()> {
        if !self.serving() {
            return Err(FsError::Suspended);
        }
        if self.pool.may_write(vdisk, self.node) {
            Ok(())
        } else {
            Err(FsError::NotWriter(vdisk))
        }
    }

    /// Whether ops leave the building: in lockstep always, and while
    /// sourcing a resync only once the offer is cut — anything earlier is
    /// inside the offer, and an op delivered both ways applies twice.
    fn stream_active(&self) -> bool {
        self.state == ReplState::Synced || self.stream_live
    }

    fn send_ops(&mut self, ops: Vec<ReplOp>) {
        if self.stream_active() {
            let first_rseq = self.next_rseq;
            self.next_rseq += ops.len() as u64;
            self.emit(Effect::Send(PeerMessage::Apply { first_rseq, ops }));
        }
    }

    /// Local WAL pressure is local business: checkpoint and go again,
    /// exactly as the NBD tool does. The peer's ring is its own.
    fn with_wal_room(&mut self, mut op: impl FnMut(&mut Pool<D>) -> Result<()>) -> Result<()> {
        match op(&mut self.pool) {
            Err(FsError::WalFull) => {
                self.pool.checkpoint()?;
                op(&mut self.pool)
            }
            other => other,
        }
    }

    pub fn create_vdisk(&mut self, id: u64, size_bytes: u64) -> Result<()> {
        if !self.serving() {
            return Err(FsError::Suspended);
        }
        self.with_wal_room(|pool| pool.create_vdisk(id, size_bytes))?;
        // Whoever made it holds it, and that has to be durable before
        // anything is written to it.
        let node = self.node;
        self.pool.claim_lease(id, node)?;
        let lease = self.pool.lease(id).expect("just claimed");
        self.send_ops(vec![
            ReplOp::CreateVdisk { id, size_bytes },
            ReplOp::SetLease { vdisk: id, lease },
        ]);
        Ok(())
    }

    pub fn write_block(&mut self, vdisk: u64, index: u64, payload: &[u8]) -> Result<()> {
        self.writable(vdisk)?;
        let hash = self.pool.put_block(payload)?;
        self.with_wal_room(|pool| pool.write_block_prehashed(vdisk, index, hash))?;
        if self.stream_active() {
            self.emit(Effect::Send(PeerMessage::Payloads(vec![payload.to_vec()])));
            self.send_ops(vec![ReplOp::Write { vdisk, index, hash }]);
        }
        Ok(())
    }

    pub fn trim_block(&mut self, vdisk: u64, index: u64) -> Result<()> {
        self.writable(vdisk)?;
        self.with_wal_room(|pool| pool.trim_block(vdisk, index))?;
        self.send_ops(vec![ReplOp::Trim { vdisk, index }]);
        Ok(())
    }

    pub fn delete_vdisk(&mut self, id: u64) -> Result<()> {
        self.writable(id)?;
        self.with_wal_room(|pool| pool.delete_vdisk(id))?;
        self.send_ops(vec![ReplOp::DeleteVdisk { id }]);
        Ok(())
    }

    pub fn snapshot_vdisk(&mut self, vdisk: u64, snapshot: u64) -> Result<()> {
        self.writable(vdisk)?;
        self.pool.snapshot_vdisk(vdisk, snapshot)?;
        self.send_ops(vec![ReplOp::Snapshot { vdisk, snapshot }]);
        Ok(())
    }

    pub fn delete_snapshot(&mut self, vdisk: u64, snapshot: u64) -> Result<()> {
        self.writable(vdisk)?;
        self.pool.delete_snapshot(vdisk, snapshot)?;
        self.send_ops(vec![ReplOp::DeleteSnapshot { vdisk, snapshot }]);
        Ok(())
    }

    pub fn rollback_vdisk(&mut self, vdisk: u64, snapshot: u64) -> Result<()> {
        self.writable(vdisk)?;
        self.pool.rollback_vdisk(vdisk, snapshot)?;
        self.send_ops(vec![ReplOp::Rollback { vdisk, snapshot }]);
        Ok(())
    }

    pub fn clone_vdisk(&mut self, new_id: u64, vdisk: u64, snapshot: u64) -> Result<()> {
        self.writable(vdisk)?;
        self.pool.clone_vdisk(new_id, vdisk, snapshot)?;
        let node = self.node;
        self.pool.claim_lease(new_id, node)?;
        let lease = self.pool.lease(new_id).expect("just claimed");
        self.send_ops(vec![
            ReplOp::Clone {
                new_id,
                vdisk,
                snapshot,
            },
            ReplOp::SetLease {
                vdisk: new_id,
                lease,
            },
        ]);
        Ok(())
    }

    /// The guest's durability barrier. Local durability happens now; the
    /// returned ticket completes (an [`Effect::FlushDone`]) when the peer
    /// is durable too, immediately when this node is alone under a verdict
    /// — Degraded, or sourcing a resync in the era the verdict minted —
    /// or parks when suspended, which is DRBD's suspended I/O wearing
    /// sans-IO clothes.
    pub fn flush(&mut self) -> Result<u64> {
        self.pool.flush()?;
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let needs = self.next_rseq - 1;
        match self.state {
            ReplState::Degraded => self.emit(Effect::FlushDone(ticket)),
            ReplState::Resyncing { source: true } if self.serving_source => {
                self.emit(Effect::FlushDone(ticket))
            }
            ReplState::Synced => {
                if self.durable_rseq >= needs {
                    self.emit(Effect::FlushDone(ticket));
                } else {
                    self.parked.push((ticket, needs));
                    self.emit(Effect::Send(PeerMessage::Flush { up_to: needs }));
                }
            }
            _ => self.parked.push((ticket, needs)),
        }
        Ok(ticket)
    }

    // -----------------------------------------------------------------
    // Reads — always local, always allowed. What is readable here is
    // exactly what this node would serve after a failover.

    pub fn read_block(&self, vdisk: u64, index: u64) -> Result<Option<Vec<u8>>> {
        self.pool.read_block(vdisk, index)
    }

    pub fn read_snapshot_block(
        &self,
        vdisk: u64,
        snapshot: u64,
        index: u64,
    ) -> Result<Option<Vec<u8>>> {
        self.pool.read_snapshot_block(vdisk, snapshot, index)
    }

    /// Local maintenance; each node runs its own on its own schedule.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.pool.checkpoint()
    }

    /// Local collection, legal at any point in a resync on either end:
    /// the sync pins are part of the mark, so neither the offer nor a
    /// half-fetched fragment can be swept out from under the pull.
    pub fn collect_garbage(&mut self) -> Result<crate::brick::GcStats> {
        self.pool.collect_garbage()
    }

    // -----------------------------------------------------------------
    // The peer's messages.

    pub fn handle(&mut self, message: PeerMessage) -> Result<()> {
        match message {
            PeerMessage::Hello { era, node } => self.on_hello(era, node),
            // A pulling target buffers the source's live stream rather than
            // applying it: its own state is about to be replaced by the
            // adoption, and anything applied before that would be applied
            // to a history that is going away. The replay after adoption is
            // what lands the target on the source's *live* state.
            PeerMessage::Payloads(_) | PeerMessage::Apply { .. }
                if self.state == (ReplState::Resyncing { source: false }) =>
            {
                self.backlog.push(message);
                Ok(())
            }
            PeerMessage::Payloads(payloads) => {
                for payload in payloads {
                    self.pool.put_block(&payload)?;
                }
                Ok(())
            }
            PeerMessage::Apply { first_rseq, ops } => self.on_apply(first_rseq, ops),
            PeerMessage::Flush { up_to } => {
                if self.applied_rseq < up_to {
                    return Err(FsError::Corrupt(
                        "a flush asks for ops the stream never delivered",
                    ));
                }
                self.pool.flush()?;
                self.emit(Effect::Send(PeerMessage::Durable { up_to }));
                Ok(())
            }
            PeerMessage::Durable { up_to } => {
                self.durable_rseq = self.durable_rseq.max(up_to);
                let durable = self.durable_rseq;
                let (done, parked): (Vec<_>, Vec<_>) = std::mem::take(&mut self.parked)
                    .into_iter()
                    .partition(|(_, needs)| *needs <= durable);
                self.parked = parked;
                for (ticket, _) in done {
                    self.emit(Effect::FlushDone(ticket));
                }
                Ok(())
            }
            PeerMessage::SyncStart => self.on_sync_start(),
            PeerMessage::SyncManifest(offer) => self.on_sync_manifest(offer),
            PeerMessage::SyncNeed(hashes) => {
                let mut payloads = Vec::with_capacity(hashes.len());
                for hash in hashes {
                    match self.pool.block_payload(&hash)? {
                        Some(payload) => payloads.push(payload),
                        None => {
                            return Err(FsError::Corrupt(
                                "the peer asked for a block this store does not hold",
                            ))
                        }
                    }
                }
                self.emit(Effect::Send(PeerMessage::SyncData(payloads)));
                Ok(())
            }
            PeerMessage::SyncData(payloads) => self.on_sync_data(payloads),
            PeerMessage::SyncReady => self.on_sync_ready(),
            PeerMessage::SyncAdopt { final_rseq } => self.on_sync_adopt(final_rseq),
            PeerMessage::SyncDone { era: _ } => {
                // The target adopted this node's state and replayed the
                // stream; lockstep continues on the numbering the stream
                // already established — resetting it here would orphan the
                // ops sent while sourcing. Every flush that was waiting out
                // the divergence is now two-node durable by adoption.
                self.state = ReplState::Synced;
                self.serving_source = false;
                self.stream_live = false;
                self.pool.unpin_sync();
                let parked = std::mem::take(&mut self.parked);
                for (ticket, _) in parked {
                    self.emit(Effect::FlushDone(ticket));
                }
                Ok(())
            }
        }
    }

    fn reset_stream(&mut self) {
        self.next_rseq = 1;
        self.durable_rseq = 0;
        self.applied_rseq = 0;
        self.pull = SyncPull::default();
    }

    fn on_hello(&mut self, peer_era: u64, peer_node: NodeId) -> Result<()> {
        // Both sides compute the same answer from the same pair: higher
        // era is the source; equal eras fall to the lower node id — a tie
        // means both hold every acknowledged write, and the diff is cheap.
        self.peer_era_seen = self.peer_era_seen.max(peer_era);
        // A hello opens a session; nothing numbered under an earlier one
        // may be mistaken for part of this one.
        self.reset_stream();
        self.pool.unpin_sync();
        let my_era = self.pool.era();
        let source = my_era > peer_era || (my_era == peer_era && self.node < peer_node);
        if source {
            self.state = ReplState::Resyncing { source: true };
            // Era superiority is a fence verdict made durable, and the
            // verdict is what lets this node keep acknowledging alone
            // while the stale peer catches up. A tie has no verdict, so
            // the tie's source suspends its guests as it always has.
            self.serving_source = my_era > peer_era;
        } else {
            self.state = ReplState::Resyncing { source: false };
            self.serving_source = false;
            self.emit(Effect::Send(PeerMessage::SyncStart));
        }
        Ok(())
    }

    fn on_sync_start(&mut self) -> Result<()> {
        if self.state != (ReplState::Resyncing { source: true }) {
            return Err(FsError::Corrupt(
                "asked to source a sync while not sourcing",
            ));
        }
        // Settle everything, then offer it.
        self.pool.checkpoint()?;
        let (era, vdisks, snapshots) = self.pool.sync_manifest();
        let leases = self.pool.leases();
        let offer = SyncOffer {
            era,
            vdisks,
            snapshots,
            leases,
        };
        // The offer's trees must outlive this node's own progress: a
        // serving source keeps checkpointing and collecting, and without
        // the pins its next collection could free blocks the target has
        // yet to ask for.
        self.pool
            .pin_sync(offer_pins(&offer, self.pool.block_size()));
        // From exactly this checkpoint onward, ops stream. Earlier writes
        // are inside the offer; later ones are on the wire; nothing is in
        // both places.
        self.stream_live = self.serving_source;
        self.emit(Effect::Send(PeerMessage::SyncManifest(offer)));
        Ok(())
    }

    /// Resolve everything queued: walk down through what this node already
    /// holds, and list what it does not.
    ///
    /// The walk descends into subtrees it already has rather than assuming
    /// them complete. That assumption looks safe — content addressing does
    /// mean an identical hash is an identical subtree — but it is false on
    /// a node whose previous pull was cut short: it can hold an interior
    /// node whose children never arrived, and skipping it would adopt a
    /// root over a hole. Descending costs local reads of blocks already
    /// here, and buys back the guarantee. What is still skipped, which is
    /// the point of a Merkle diff, is *transferring* any subtree already
    /// held.
    fn resolve_pending(&mut self) -> Result<()> {
        while let Some((hash, kind)) = self.pull.pending.pop() {
            if !self.pull.seen.insert((hash, kind)) {
                continue;
            }
            if self.pool.has_block(&hash) {
                if kind > 0 {
                    let payload = self
                        .pool
                        .block_payload(&hash)?
                        .ok_or(FsError::Corrupt("a held block vanished mid-resync"))?;
                    for child in map::children(&payload) {
                        self.pull.pending.push((child, kind - 1));
                    }
                }
                continue;
            }
            let entry = self.pull.wanted.entry(hash).or_default();
            if entry.is_empty() {
                self.pull.unrequested.push(hash);
            }
            if !entry.contains(&kind) {
                entry.push(kind);
            }
        }
        Ok(())
    }

    fn pull_complete(&self) -> bool {
        self.pull.pending.is_empty()
            && self.pull.wanted.is_empty()
            && self.pull.unrequested.is_empty()
    }

    fn request_more(&mut self) {
        if self.pull.outstanding > 0 || self.pull.unrequested.is_empty() {
            return;
        }
        let take = self.pull.unrequested.len().min(SYNC_BATCH);
        let batch: Vec<BlockHash> = self.pull.unrequested.drain(..take).collect();
        self.pull.outstanding = batch.len();
        self.emit(Effect::Send(PeerMessage::SyncNeed(batch)));
    }

    fn on_sync_manifest(&mut self, offer: SyncOffer) -> Result<()> {
        if self.state != (ReplState::Resyncing { source: false }) {
            return Err(FsError::Corrupt(
                "offered a sync manifest while not pulling",
            ));
        }
        // Everything this pull fetches hangs off these roots, referenced
        // by nothing in this node's own manifest until adoption. The pull
        // arrives top-down, so the pins reach every fetched block through
        // blocks the store holds.
        self.pool
            .pin_sync(offer_pins(&offer, self.pool.block_size()));
        let entries = map::entries_per_node(self.pool.block_size());
        let block_size = self.pool.block_size() as u64;
        // A root of a depth-d tree is kind d: its children are kind d-1,
        // and kind 0 is a data block.
        for (_, size_bytes, root) in &offer.vdisks {
            if let Some(root) = root {
                let depth = map::depth_for(size_bytes.div_ceil(block_size), entries);
                self.pull.pending.push((*root, depth));
            }
        }
        for (_, _, size_bytes, root) in &offer.snapshots {
            if let Some(root) = root {
                let depth = map::depth_for(size_bytes.div_ceil(block_size), entries);
                self.pull.pending.push((*root, depth));
            }
        }
        self.pull.manifest = Some(offer);
        self.resolve_pending()?;
        if self.pull_complete() {
            self.emit(Effect::Send(PeerMessage::SyncReady));
        } else {
            self.request_more();
        }
        Ok(())
    }

    fn on_sync_data(&mut self, payloads: Vec<Vec<u8>>) -> Result<()> {
        if self.state != (ReplState::Resyncing { source: false }) {
            return Err(FsError::Corrupt("sync data arrived while not pulling"));
        }
        for payload in payloads {
            let hash = self.pool.put_block(&payload)?;
            self.pull.outstanding = self.pull.outstanding.saturating_sub(1);
            let kinds = self.pull.wanted.remove(&hash).unwrap_or_default();
            for kind in kinds {
                // The block is here now; walking it from `pending` keeps
                // one descent path for arrived and already-held blocks
                // alike.
                self.pull.seen.remove(&(hash, kind));
                self.pull.pending.push((hash, kind));
            }
        }
        self.resolve_pending()?;
        if self.pull_complete() {
            self.emit(Effect::Send(PeerMessage::SyncReady));
        } else {
            self.request_more();
        }
        Ok(())
    }

    /// The target asked leave to adopt. Granting it means stopping the
    /// solo-acknowledgement regime *first*: from here until SyncDone, this
    /// node refuses writes — a round trip plus the stream still in flight,
    /// the daemon's queue-and-retry window — so that no acknowledged write
    /// can exist that the adopting peer lacks. The fence in the reply is
    /// its position: FIFO delivery puts every streamed op before it.
    fn on_sync_ready(&mut self) -> Result<()> {
        if self.state != (ReplState::Resyncing { source: true }) {
            return Err(FsError::Corrupt(
                "a sync-ready arrived at a node that is not sourcing",
            ));
        }
        self.serving_source = false;
        self.emit(Effect::Send(PeerMessage::SyncAdopt {
            final_rseq: self.next_rseq - 1,
        }));
        Ok(())
    }

    fn on_sync_adopt(&mut self, final_rseq: u64) -> Result<()> {
        if self.state != (ReplState::Resyncing { source: false }) || !self.pull_complete() {
            return Err(FsError::Corrupt(
                "told to adopt without a completed pull to adopt",
            ));
        }
        let offer = self.pull.manifest.take().ok_or(FsError::Corrupt(
            "finishing a pull that never had a manifest",
        ))?;
        let era = offer.era;
        self.pool
            .adopt_sync(era, &offer.vdisks, &offer.snapshots, &offer.leases)?;
        // The leases came over inside the adoption: the source's view of
        // who may write is part of the state being adopted, not a separate
        // negotiation.
        self.pull = SyncPull::default();
        self.pool.unpin_sync();
        self.state = ReplState::Synced;
        // The offer was the source at its checkpoint; the backlog is
        // everything its guests did since. Replayed through the ordinary
        // handlers — sequence checks and all — it lands this node on the
        // source's live state, and lockstep continues on that numbering.
        for message in std::mem::take(&mut self.backlog) {
            self.handle(message)?;
        }
        if self.applied_rseq != final_rseq {
            return Err(FsError::Corrupt(
                "the stream this node replayed is not the stream the source sent",
            ));
        }
        // Durable before SyncDone: the source will complete parked flushes
        // on that message, and a promise kept in an unflushed WAL is not
        // yet a promise.
        self.pool.flush()?;
        // Anything this node's guests had in flight before it went stale
        // was discarded by the adoption; saying so beats pretending.
        let parked = std::mem::take(&mut self.parked);
        for (ticket, _) in parked {
            self.emit(Effect::FlushFailed(ticket));
        }
        self.emit(Effect::Send(PeerMessage::SyncDone { era }));
        Ok(())
    }

    fn on_apply(&mut self, first_rseq: u64, ops: Vec<ReplOp>) -> Result<()> {
        if first_rseq != self.applied_rseq + 1 {
            return Err(FsError::Corrupt("the op stream skipped or repeated"));
        }
        let count = ops.len() as u64;
        for op in ops {
            self.apply_op(op)?;
        }
        self.applied_rseq += count;
        Ok(())
    }

    fn apply_op(&mut self, op: ReplOp) -> Result<()> {
        match op {
            ReplOp::CreateVdisk { id, size_bytes } => {
                self.with_wal_room(|pool| pool.create_vdisk(id, size_bytes))
            }
            ReplOp::Write { vdisk, index, hash } => {
                self.with_wal_room(|pool| pool.write_block_prehashed(vdisk, index, hash))
            }
            ReplOp::Trim { vdisk, index } => {
                self.with_wal_room(|pool| pool.trim_block(vdisk, index))
            }
            ReplOp::DeleteVdisk { id } => self.with_wal_room(|pool| pool.delete_vdisk(id)),
            ReplOp::Snapshot { vdisk, snapshot } => self.pool.snapshot_vdisk(vdisk, snapshot),
            ReplOp::DeleteSnapshot { vdisk, snapshot } => {
                self.pool.delete_snapshot(vdisk, snapshot)
            }
            ReplOp::Rollback { vdisk, snapshot } => self.pool.rollback_vdisk(vdisk, snapshot),
            ReplOp::Clone {
                new_id,
                vdisk,
                snapshot,
            } => self.pool.clone_vdisk(new_id, vdisk, snapshot),
            // Applied as decided, not re-decided: the holder already
            // resolved who may write, and a peer that argued would be the
            // second opinion this design exists to prevent.
            ReplOp::SetLease { vdisk, lease } => self.pool.set_lease(vdisk, lease),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brick::{Brick, BrickParams};
    use crate::sim::SimDisk;

    const KIB: u64 = 1024;

    fn node(seed: u64, id: NodeId) -> ReplNode<SimDisk> {
        let brick = Brick::format(
            SimDisk::new(4 * KIB * KIB, seed),
            BrickParams {
                pool_uuid: [0xAA; 16],
                brick_uuid: [id; 16],
                block_size: 4 * KIB as u32,
                segment_size: 128 * KIB,
                wal_size: 32 * KIB,
            },
        )
        .unwrap();
        ReplNode::new(Pool::create(brick).unwrap(), id)
    }

    #[test]
    fn a_suspended_node_refuses_writes_and_parks_flushes() {
        let mut a = node(1, 0);
        assert_eq!(a.state(), ReplState::Suspended);
        assert_eq!(
            a.create_vdisk(1, 100 * 4096).unwrap_err(),
            FsError::Suspended
        );
        let ticket = a.flush().unwrap();
        // Parked: no FlushDone among the effects.
        assert!(a
            .take_effects()
            .iter()
            .all(|e| !matches!(e, Effect::FlushDone(_))));
        // The verdict releases it.
        a.set_peer_fenced().unwrap();
        assert_eq!(a.state(), ReplState::Degraded);
        assert!(a.take_effects().contains(&Effect::FlushDone(ticket)));
    }

    #[test]
    fn a_degraded_node_acknowledges_alone_and_bumped_its_era() {
        let mut a = node(2, 0);
        assert_eq!(a.pool().era(), 1);
        a.set_peer_fenced().unwrap();
        assert_eq!(a.pool().era(), 2);
        a.create_vdisk(1, 100 * 4096).unwrap();
        a.write_block(1, 0, b"alone but honest").unwrap();
        let ticket = a.flush().unwrap();
        assert!(a.take_effects().contains(&Effect::FlushDone(ticket)));
    }

    #[test]
    fn a_verdict_for_a_present_peer_is_refused() {
        let mut a = node(3, 0);
        a.set_peer_fenced().unwrap();
        // Already degraded: a second verdict makes no sense.
        assert!(a.set_peer_fenced().is_err());
    }

    #[test]
    fn only_the_writer_writes() {
        let mut a = node(4, 0);
        a.set_peer_fenced().unwrap();
        a.create_vdisk(1, 100 * 4096).unwrap();
        a.take_effects();
        // Another node's view: same vdisk, no writer role.
        let mut b = node(5, 1);
        b.set_peer_fenced().unwrap();
        b.pool_create_for_test(1);
        assert_eq!(
            b.write_block(1, 0, b"not mine").unwrap_err(),
            FsError::NotWriter(1)
        );
    }

    impl ReplNode<SimDisk> {
        fn pool_create_for_test(&mut self, id: u64) {
            // Create the vdisk without claiming the writer role, as an
            // applier would.
            self.pool.create_vdisk(id, 100 * 4096).unwrap();
        }
    }
}
