//! Synchronous replication among a pool's members — phase 2's two-node
//! core, restructured around per-peer sessions for phase 5, as a sans-IO
//! state machine the simulation can torture.
//!
//! One [`ReplNode`] wraps one node's [`Pool`]. It never touches a socket or
//! a clock: the daemon (later) and the test harness (now) deliver peer
//! messages to [`ReplNode::handle`], tell it about links and verdicts
//! ([`ReplNode::peer_lost`], [`ReplNode::set_member_fenced`]), and drain
//! its [`Effect`]s — messages to send (each named with its addressee),
//! guest flushes to acknowledge or fail.
//!
//! ## One node, many sessions
//!
//! Every peer relationship is its own [`PeerSession`]: its own state, its
//! own dense op stream (`Apply` numbering is per ordered pair of nodes),
//! its own resync. The op stream fans out — **every op goes to every
//! streaming peer** (metadata is replicated to every member; the recorded
//! phase-5 decision) — while each session's numbering stays contiguous, so
//! the "skipped or repeated" check needs no change. What differs per peer
//! is only *which durability gates a guest flush*, below.
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
//! A guest flush completes only when the node is serving and every *need*
//! it parked with is settled. A need names a peer, that peer's session
//! generation, and the last op sequence whose durability that peer owes.
//! At two members every op needs the one peer, which is DRBD protocol C
//! restated; at three, a data write will need only its block's homes
//! (phase-5 slice 24) while every other op needs every live member — the
//! rule that keeps "metadata everywhere" a durability property. A need is
//! settled by that peer's `Durable` (same session generation only — a
//! sequence number from a dead session must never be satisfied by a new
//! session's counting), or excused wholesale by the peer's `SyncDone`
//! (adoption covers everything) or its fence verdict (the cluster vouched
//! there is no one left to ask). Nothing else settles anything: no blanket
//! drains.
//!
//! ## States, and who decides them
//!
//! Each session runs the phase-2 diagram; the node serves guests only when
//! every known peer is accounted for:
//!
//! ```text
//!   Synced ──peer_lost──▶ Suspended ──set_member_fenced──▶ (fenced)
//!     ▲                       │  ▲                             │
//!     │                    (that peer unaccounted:   (era bumped to the
//!     │                     writes refuse,            verdict's number;
//!     │                     flushes park)             writes go on without
//!     │                                               that member)
//!     └── Resyncing ◀──Hello exchange on reconnect────────────┘
//!          (the target refuses writes; a source with era superiority
//!           keeps serving guests and streams its ops as it goes)
//! ```
//!
//! The engine never decides death. `set_member_fenced` is the cluster's
//! verdict (Pacemaker's fence confirmation, docs/cluster.md) arriving from
//! above; without it a partitioned node suspends forever — integrity over
//! availability, always. The verdict **carries the era**: two survivors
//! bumping independently could mint different numbers, and the next hello
//! would read the difference as a fence verdict and discard acknowledged
//! writes — so the layer that holds quorum state computes one number from
//! every survivor's reported floor ([`ReplNode::era_target`]) and every
//! survivor adopts it. At two members the sole survivor's own floor *is*
//! that computation.
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
//! target's fetched fragment is referenced by nothing yet) — **per
//! session**, because a node can source one peer's pull while pulling from
//! another — and a fence verdict's era clears every era this node has
//! *witnessed*, not just its own, so a survivor fenced mid-pull cannot
//! mint a second copy of the era it was adopting.
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
//!
//! ## Two windows this restructure closed
//!
//! Both were reachable at two nodes and neither simulation had looked:
//! a payload delivered ahead of its op was invisible to garbage
//! collection until the op mapped it — a collection in that gap swept it
//! and the op then died at the "store must hold it" check — so arrived
//! payloads are now pinned per session until their op lands. And the
//! resync pins, pull, and backlog were single global slots that any hello
//! clobbered; they live in the session now, where a second peer's arrival
//! cannot reach them.

/// Placement arithmetic: hash → slice → an ordered pair of members.
pub mod slice;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::disk::Disk;
use crate::error::{FsError, Result};
use crate::hash::BlockHash;
use crate::pool::map;
use crate::pool::{AdoptScope, Lease, Pool};

pub type NodeId = u8;

/// One vdisk in a sync offer: id, size, tier, checkpointed root.
pub type VdiskOffer = (u64, u64, u8, Option<BlockHash>);
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
fn offer_pins(offer: &SyncOffer, block_size: u32) -> Vec<(BlockHash, u32, u8)> {
    let entries = map::entries_per_node(block_size);
    let block = block_size as u64;
    let mut pins = Vec::new();
    for (_, size_bytes, tier, root) in &offer.vdisks {
        if let Some(root) = root {
            pins.push((
                *root,
                map::depth_for(size_bytes.div_ceil(block), entries),
                *tier,
            ));
        }
    }
    for (vdisk, _, size_bytes, root) in &offer.snapshots {
        if let Some(root) = root {
            pins.push((
                *root,
                map::depth_for(size_bytes.div_ceil(block), entries),
                offer_vdisk_tier(offer, *vdisk),
            ));
        }
    }
    pins
}

/// The tier of a vdisk named in an offer. A snapshot's vdisk is always in
/// the same offer — deleting a vdisk with snapshots is refused — so an
/// absent one is a malformed offer, mapped to tier 0 here and caught by
/// the adoption's own checks rather than a panic in pin arithmetic.
fn offer_vdisk_tier(offer: &SyncOffer, vdisk: u64) -> u8 {
    offer
        .vdisks
        .iter()
        .find(|(id, _, _, _)| *id == vdisk)
        .map(|(_, _, tier, _)| *tier)
        .unwrap_or(0)
}

/// One replicated operation — the wire form of the pool's mutating API.
/// Payloads travel separately ([`PeerMessage::Payloads`]); an op references
/// data only by content address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplOp {
    CreateVdisk {
        id: u64,
        size_bytes: u64,
        tier: u8,
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
    /// Who I am, which era my state belongs to, and which tiers my brick
    /// set carries. Opens every connection; both sides derive the same
    /// source/target roles from the pair, and the tier inventory is what
    /// lets a vdisk create refuse a tier a peer cannot hold instead of
    /// failing that peer's replay.
    Hello {
        era: u64,
        node: NodeId,
        tiers: Vec<u8>,
        /// The committed slice map's version, 0 when unplaced. Versions
        /// must agree before roles are even elected: a member that slept
        /// through reassignments cannot compute the current map (the
        /// arithmetic chains), so the higher side ships it whole.
        map_version: u64,
    },
    /// Data blocks ahead of the ops that reference them, each on the tier
    /// its op will reference it at.
    Payloads(Vec<(u8, Vec<u8>)>),
    /// Ops in stream order. `first_rseq` numbers the first op; the rest
    /// follow sequentially — a gap means the transport lied. Numbering is
    /// per ordered pair of nodes: each session counts its own stream.
    Apply { first_rseq: u64, ops: Vec<ReplOp> },
    /// Make everything up to `up_to` durable and say so.
    Flush { up_to: u64 },
    /// Everything up to `up_to` is durable here.
    Durable { up_to: u64 },
    /// Target asks the source to checkpoint and offer its state.
    SyncStart,
    /// The source's settled state, roots and all.
    SyncManifest(SyncOffer),
    /// Blocks the target lacks, each named with the tier it lives on —
    /// the source's route is per tier, so a bare hash is half an address.
    SyncNeed(Vec<(u8, BlockHash)>),
    /// The blocks, tier and payload — each self-identifies by its hash.
    SyncData(Vec<(u8, Vec<u8>)>),
    /// The committed map, whole — version and every pair — for a member
    /// whose hello showed it behind. Persisted before streaming resumes;
    /// the receiver re-hellos under the new version.
    MapIs {
        version: u64,
        pairs: Vec<slice::Homes>,
    },
    /// Blocks wanted right now, outside any resync: a non-home guest read,
    /// a home repairing, or a reassignment move landing. Served from the
    /// store exactly like `SyncNeed`; answered with `ReadData`.
    Read(Vec<(u8, BlockHash)>),
    /// The fetched blocks, tier and payload — each self-identifies by its
    /// hash. A home stores what arrives (that is the repair); a non-home
    /// serves the waiting read and keeps nothing.
    ReadData(Vec<(u8, Vec<u8>)>),
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

/// Merge what one engine call queued into as few wire messages as the
/// stream's meaning allows. A large guest write emits `Payloads`, `Apply`,
/// `Payloads`, `Apply`, … per block — hundreds of messages, each one a
/// syscall on this side and a lock acquisition on the peer's. Merged, the
/// same call is one `Payloads` and one `Apply` per peer.
///
/// Two rules keep it exactly as meaningful as the original stream:
///
/// - **A payload may move earlier; an op may not.** The peer must hold a
///   block before the op referencing it applies. Folding a payload into
///   an earlier `Payloads` keeps every payload ahead of its op; folding
///   an op into an apply that sits BEFORE a payload emitted in between
///   would put the op ahead of its own block — the three-node sim caught
///   exactly that — so a freshly opened `Payloads` window closes the
///   peer's apply window, while merging into an existing earlier one
///   leaves it open.
/// - **Applies merge only while dense.** `first_rseq` plus the op count
///   must meet the next message's `first_rseq` — the stream's numbering
///   is the replication contract, and a gap means something else
///   happened in between.
///
/// Any other message to the same peer is a barrier: nothing merges across
/// a `Hello`, a `Flush`, a `Durable` — those carry ordering meaning this
/// function refuses to reason about. Effects that are not sends pass
/// through untouched; they answer local waiters, not the wire.
fn coalesce(effects: Vec<Effect>) -> Vec<Effect> {
    /// Where a peer's open merge window sits in `out`.
    #[derive(Default, Clone, Copy)]
    struct Window {
        payloads: Option<usize>,
        apply: Option<(usize, u64)>,
    }

    let mut out: Vec<Effect> = Vec::with_capacity(effects.len());
    let mut windows: std::collections::HashMap<NodeId, Window> = std::collections::HashMap::new();

    for effect in effects {
        match effect {
            Effect::Send(peer, PeerMessage::Payloads(mut blocks)) => {
                let window = windows.entry(peer).or_default();
                match window.payloads {
                    Some(at) => {
                        let Effect::Send(_, PeerMessage::Payloads(open)) = &mut out[at] else {
                            unreachable!("the window indexes what this fn put there");
                        };
                        open.append(&mut blocks);
                    }
                    None => {
                        window.payloads = Some(out.len());
                        // Ops emitted before this point must not merge
                        // backward past it: an op after this payload may
                        // reference it.
                        window.apply = None;
                        out.push(Effect::Send(peer, PeerMessage::Payloads(blocks)));
                    }
                }
            }
            Effect::Send(
                peer,
                PeerMessage::Apply {
                    first_rseq,
                    mut ops,
                },
            ) => {
                let open = windows.entry(peer).or_default().apply;
                match open {
                    Some((at, next)) if next == first_rseq => {
                        let Effect::Send(_, PeerMessage::Apply { ops: window, .. }) = &mut out[at]
                        else {
                            unreachable!("the window indexes what this fn put there");
                        };
                        let merged = next + ops.len() as u64;
                        window.append(&mut ops);
                        windows.entry(peer).or_default().apply = Some((at, merged));
                    }
                    _ => {
                        // A gap (or nothing open): this message starts its
                        // own window, and the old one is closed by being
                        // left behind.
                        let next = first_rseq + ops.len() as u64;
                        windows.entry(peer).or_default().apply = Some((out.len(), next));
                        out.push(Effect::Send(peer, PeerMessage::Apply { first_rseq, ops }));
                    }
                }
            }
            Effect::Send(peer, other) => {
                // A barrier for this peer: whatever meaning it carries,
                // nothing merges across it.
                windows.remove(&peer);
                out.push(Effect::Send(peer, other));
            }
            other => out.push(other),
        }
    }
    out
}

/// What the node wants the outside world to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Put this message on the wire to this peer — and only this peer;
    /// the addressee is part of the effect because the node speaks to
    /// each member down that member's own session.
    Send(NodeId, PeerMessage),
    /// The guest flush with this ticket is fully acknowledged.
    FlushDone(u64),
    /// The guest flush with this ticket can never complete honestly — its
    /// writes were discarded by an adoption. The guest sees an error, not
    /// a lie.
    FlushFailed(u64),
    /// The fetch with this ticket landed: the block is readable now — from
    /// the store if this member homes it (the repair path), or from the
    /// serve-once buffer if it does not.
    ReadDone(u64),
    /// The fetch with this ticket cannot complete: the session it rode
    /// died. The caller retries, and the retry picks whichever home is
    /// reachable then — an error the guest can survive, never a hang.
    ReadFailed(u64),
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
    /// Hashes to resolve locally as `(hash, kind, data_tier)`: present
    /// ones get walked, absent ones get requested. The data tier rides
    /// along because a kind-0 entry is a data block on its vdisk's tier,
    /// while every node above it lives on tier 0.
    pending: Vec<(BlockHash, u32, u8)>,
    /// Entries already resolved — shared subtrees are common, and a diff
    /// must not walk one twice.
    seen: HashSet<(BlockHash, u32, u8)>,
    /// Absent here: `(hash, store_tier)` → the `(kind, data_tier)`
    /// continuations waiting on it. One payload can be wanted by several
    /// walks — and, across tiers, the same hash is different blocks.
    wanted: HashMap<(BlockHash, u8), Vec<(u32, u8)>>,
    /// Wanted but not yet requested, as `(store_tier, hash)`.
    unrequested: Vec<(u8, BlockHash)>,
    /// Requested and not yet received.
    outstanding: usize,
    manifest: Option<SyncOffer>,
}

/// Everything this node knows about one peer: the relationship's state,
/// both directions of its op stream, and whatever resync is in flight on
/// it. A session outlives its connections — `era_seen` and the fenced
/// flag persist across links — but its stream numbering does not: every
/// hello opens a new generation.
#[derive(Debug)]
struct PeerSession {
    state: ReplState,
    /// Which incarnation of this session's stream the counters below
    /// belong to. Bumped by every hello and every lost link, and recorded
    /// into parked flush needs so a sequence number from a dead stream can
    /// never be satisfied by a new stream's counting.
    generation: u64,
    /// Numbering for ops sent to this peer (writer side).
    next_rseq: u64,
    /// This peer's confirmed durability (writer side).
    durable_rseq: u64,
    /// The highest rseq sent to this peer whose op requires this peer's
    /// durability before a guest flush covering it may complete. Every op
    /// requires every recipient today; slice 24 narrows data writes to
    /// their homes.
    required_rseq: u64,
    /// Last op applied from this peer (applier side).
    applied_rseq: u64,
    /// The tiers this peer's brick set carried at its last hello — what a
    /// vdisk create checks before promising a tier the peer cannot hold.
    tiers: Vec<u8>,
    /// The highest era this peer has ever claimed to this node — one
    /// component of the floor a fence-verdict era must clear. Volatile:
    /// the narrow hole (hear an era, crash before adopting it, then
    /// survive a verdict) is recorded in docs/lumenfs.md rather than
    /// closed.
    era_seen: u64,
    /// The cluster vouched this member dead. Cleared by its next hello —
    /// a verdict is an event about a past, not a permanent sentence.
    fenced: bool,
    /// The target's buffer of this source's live stream, replayed in
    /// arrival order after adoption. Bounding it is flow control, which
    /// belongs to the daemon that owns the socket.
    backlog: Vec<PeerMessage>,
    /// This session's pull, when this node is its resync target.
    pull: SyncPull,
    /// Sourcing this session's resync with era superiority: guests keep
    /// writing here, acknowledged single-copy under the verdict that made
    /// the era.
    serving_source: bool,
    /// The offer has been cut; every op from here on streams to this
    /// peer. Distinct from `serving_source` because writes accepted
    /// *before* the offer's checkpoint are inside the offer — streaming
    /// them too would deliver them twice, and half the ops are not
    /// idempotent.
    stream_live: bool,
}

impl PeerSession {
    fn new() -> PeerSession {
        PeerSession {
            state: ReplState::Suspended,
            generation: 0,
            next_rseq: 1,
            durable_rseq: 0,
            required_rseq: 0,
            applied_rseq: 0,
            tiers: Vec::new(),
            era_seen: 0,
            fenced: false,
            backlog: Vec::new(),
            pull: SyncPull::default(),
            serving_source: false,
            stream_live: false,
        }
    }

    /// Whether ops leave for this peer: in lockstep always, and while
    /// sourcing its resync only once the offer is cut — anything earlier
    /// is inside the offer, and an op delivered both ways applies twice.
    fn stream_active(&self) -> bool {
        self.state == ReplState::Synced || self.stream_live
    }

    /// Whether this peer is accounted for from a guest's point of view:
    /// its durability is either in lockstep, excused by a verdict, or
    /// covered by the single-copy regime a verdict authorized.
    fn accounted_for(&self) -> bool {
        self.fenced
            || self.state == ReplState::Synced
            || (self.state == (ReplState::Resyncing { source: true }) && self.serving_source)
    }
}

/// One guest flush waiting on its peers: settled when every need is gone
/// and the node is serving.
#[derive(Debug)]
struct ParkedFlush {
    ticket: u64,
    /// `(peer, session generation, rseq)` — the last op each peer owes
    /// durability for, under the stream that sent it.
    needs: Vec<(NodeId, u64, u64)>,
}

pub struct ReplNode<D: Disk> {
    pool: Pool<D>,
    node: NodeId,
    effects: VecDeque<Effect>,
    peers: BTreeMap<NodeId, PeerSession>,
    parked: Vec<ParkedFlush>,
    next_ticket: u64,
    /// Fetched payloads for blocks this member does not store, held for
    /// exactly one read each — taken on consumption, because a kept copy
    /// would be a third replica that collection and scrub would have to
    /// account for. "A non-home fetches on demand and keeps nothing."
    fetched: HashMap<(u8, BlockHash), Vec<u8>>,
    /// Outstanding fetches: ticket → what was asked of whom.
    pending_reads: HashMap<u64, (u8, BlockHash, NodeId)>,
    /// Every vdisk named by any offer this multi-source rejoin has seen —
    /// what survives the final cleanup. A vdisk no live member offers was
    /// deleted while this node was away.
    offered_vdisks: HashSet<u64>,
    /// Reassignment moves requested and not yet landed: `(tier, hash)` →
    /// the source asked. Cleared per entry when the block arrives, per
    /// peer when a source's session dies (the next step re-requests from
    /// whoever holds it then).
    reassign_inflight: HashMap<(u8, BlockHash), NodeId>,
}

impl<D: Disk> ReplNode<D> {
    pub fn new(pool: Pool<D>, node: NodeId) -> ReplNode<D> {
        ReplNode {
            pool,
            node,
            effects: VecDeque::new(),
            peers: BTreeMap::new(),
            parked: Vec::new(),
            next_ticket: 1,
            fetched: HashMap::new(),
            pending_reads: HashMap::new(),
            offered_vdisks: HashSet::new(),
            reassign_inflight: HashMap::new(),
        }
    }

    /// Hand this node its seat: which member it is was known at birth;
    /// which slices that member homes arrives here. Everything before this
    /// call treats the node as a home for every block — the two-member
    /// degenerate map made implicit.
    pub fn set_placement(&mut self, members: &[NodeId]) -> Result<()> {
        let map = crate::repl::slice::SliceMap::for_members(1, members)?;
        self.pool.set_placement(self.node, map);
        Ok(())
    }

    /// The node's relationship to its pool, derived from the sessions: a
    /// resync in flight names itself, a fleet of verdicts is Degraded, a
    /// live lockstep is Synced, and anything less is Suspended. With one
    /// peer this is exactly the phase-2 state; with more it is the row a
    /// status line summarizes, while each session keeps its own truth.
    pub fn state(&self) -> ReplState {
        if let Some(session) = self
            .peers
            .values()
            .find(|s| matches!(s.state, ReplState::Resyncing { .. }))
        {
            return session.state;
        }
        if !self.peers.is_empty() && self.peers.values().all(|s| s.fenced) {
            return ReplState::Degraded;
        }
        if !self.peers.is_empty()
            && self
                .peers
                .values()
                .all(|s| s.fenced || s.state == ReplState::Synced)
        {
            return ReplState::Synced;
        }
        ReplState::Suspended
    }

    /// The stream position, for observability: ops sent, ops confirmed
    /// durable, ops applied. A survivor's claim of honesty is checkable
    /// from outside only if these are visible. With one peer these are
    /// that session's counters; with more they summarize the busiest
    /// stream (per-peer counters are the daemon's status surface).
    pub fn stream_counters(&self) -> (u64, u64, u64) {
        self.peers
            .values()
            .map(|s| (s.next_rseq - 1, s.durable_rseq, s.applied_rseq))
            .max()
            .unwrap_or((0, 0, 0))
    }

    /// Every session's stream position, per peer: `(peer, sent, durable,
    /// applied)` — the mesh's status surface.
    pub fn peer_counters(&self) -> Vec<(NodeId, u64, u64, u64)> {
        self.peers
            .iter()
            .map(|(id, s)| (*id, s.next_rseq - 1, s.durable_rseq, s.applied_rseq))
            .collect()
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
        coalesce(self.effects.drain(..).collect())
    }

    fn emit(&mut self, effect: Effect) {
        self.effects.push_back(effect);
    }

    fn session(&mut self, peer: NodeId) -> &mut PeerSession {
        self.peers.entry(peer).or_insert_with(PeerSession::new)
    }

    // -----------------------------------------------------------------
    // Links and verdicts — the outside world's facts.

    /// The link to this peer is up; say hello. Role assignment happens
    /// when the peer's own hello arrives.
    pub fn connect(&mut self, peer: NodeId) {
        self.session(peer);
        self.emit(Effect::Send(
            peer,
            PeerMessage::Hello {
                era: self.pool.era(),
                node: self.node,
                tiers: self.pool.tiers(),
                map_version: self.map_version(),
            },
        ));
    }

    /// The committed map's version; 0 for an unplaced pool, whose peers
    /// are equally unplaced by construction.
    fn map_version(&self) -> u64 {
        self.pool
            .placement()
            .map(|(_, map)| map.version())
            .unwrap_or(0)
    }

    /// The link to this peer is gone. Whatever was in flight is gone with
    /// it; flushes stay parked until a verdict or a reconciliation decides
    /// their fate.
    ///
    /// Queued messages to this peer are dropped here, and that is
    /// load-bearing rather than tidy. A message written for a connection
    /// that has died must not be delivered over the next one: a reply to a
    /// request from a session that no longer exists can arrive inside a
    /// fresh session and be mistaken for an answer to it. This is the
    /// engine's half of the contract; the daemon's half is to never carry
    /// bytes across connection incarnations. Guest-facing effects are not
    /// messages and survive — a parked flush is still owed an answer.
    pub fn peer_lost(&mut self, peer: NodeId) {
        self.effects
            .retain(|effect| !matches!(effect, Effect::Send(to, _) if *to == peer));
        let session = self.session(peer);
        session.state = ReplState::Suspended;
        session.generation += 1;
        session.pull = SyncPull::default();
        // A buffered stream from a dead session must never replay into the
        // next one: the next offer already contains those writes, and the
        // next stream will reuse their sequence numbers.
        session.backlog.clear();
        session.serving_source = false;
        session.stream_live = false;
        // The offer pins and any payloads delivered ahead of ops that will
        // now never arrive: garbage, correctly, once unpinned.
        self.pool.drop_session_pins(peer);
        // Fetches riding the dead session fail rather than hang; the
        // retry asks whichever home is reachable then.
        let failed: Vec<u64> = self
            .pending_reads
            .iter()
            .filter(|(_, (_, _, target))| *target == peer)
            .map(|(ticket, _)| *ticket)
            .collect();
        for ticket in failed {
            self.pending_reads.remove(&ticket);
            self.emit(Effect::ReadFailed(ticket));
        }
        // Reassignment requests to the dead source re-issue at the next
        // step, from whichever kept home is reachable then.
        self.reassign_inflight.retain(|_, source| *source != peer);
    }

    /// The floor a fence-verdict era must clear from this node's vantage:
    /// past its own era and past every era any peer has ever claimed to
    /// it. The layer that issues verdicts takes the max of every
    /// survivor's floor, so all of them adopt one number — two survivors
    /// computing independently could mint different eras, and the next
    /// hello would read the difference as a verdict that never happened.
    pub fn era_target(&self) -> u64 {
        self.peers
            .values()
            .map(|s| s.era_seen)
            .fold(self.pool.era(), u64::max)
            + 1
    }

    /// The cluster's verdict: this member has been fenced, and the pool
    /// continues at `era`. Not this crate's decision — it arrives from the
    /// machinery that owns quorum and fencing (docs/cluster.md), and it is
    /// the only thing that turns suspension into progress without the
    /// member. The era bump is anchored before any new state exists under
    /// it, and the fenced member's needs come off every parked flush:
    /// its durability is no longer anyone's to owe.
    pub fn set_member_fenced(&mut self, peer: NodeId, era: u64) -> Result<()> {
        let floor = self.era_target();
        let session = self.session(peer);
        if session.state != ReplState::Suspended {
            return Err(FsError::Corrupt(
                "a fence verdict arrived for a peer that is not lost",
            ));
        }
        if session.fenced {
            return Err(FsError::Corrupt(
                "a fence verdict arrived for a member already fenced",
            ));
        }
        if era < floor {
            return Err(FsError::Corrupt(
                "a fence verdict's era is below what this node has witnessed",
            ));
        }
        session.fenced = true;
        self.pool.bump_era_to(era)?;
        // The bump retired every lease of the era it closed — including
        // this node's own. The dead member's leases must stay retired, so
        // a failover can claim them; but a guest already running *here*
        // must not lose its pen to another member's death, so leases this
        // node held are re-issued under the new era — on the stream, so
        // any peer still in lockstep applies the same conclusion. A
        // handover window open toward the dead node closes in the same
        // stroke, which is the abort that migration was owed.
        let mine: Vec<u64> = self
            .pool
            .leases()
            .into_iter()
            .filter(|(_, lease)| lease.holder == self.node)
            .map(|(vdisk, _)| vdisk)
            .collect();
        for vdisk in mine {
            let node = self.node;
            self.pool.claim_lease(vdisk, node)?;
            if let Some(lease) = self.pool.lease(vdisk) {
                self.send_ops(vec![ReplOp::SetLease { vdisk, lease }]);
            }
        }
        self.excuse_needs(peer);
        Ok(())
    }

    /// Whether this member currently stands fenced from this node's view.
    pub fn is_fenced(&self, peer: NodeId) -> bool {
        self.peers.get(&peer).is_some_and(|s| s.fenced)
    }

    // -----------------------------------------------------------------
    // Leases.

    /// Take the writer role for a vdisk. Legitimate while degraded — the
    /// HA restart after a fence — or while synced, where it replicates as
    /// an explicit claim.
    ///
    /// An era-stale lease is claimable only when its holder is fenced, or
    /// is this node. A stale record naming a *live* member is a member
    /// that may still be writing — the era retired the lease's authority,
    /// not the machine — and claiming out from under it would be the
    /// second writer this design exists to prevent.
    pub fn claim_writer(&mut self, vdisk: u64) -> Result<()> {
        if let Some(lease) = self.pool.lease(vdisk) {
            let stale = lease.era < self.pool.era();
            let holder_live = lease.holder != self.node && !self.is_fenced(lease.holder);
            if stale && holder_live {
                return Err(FsError::LeaseHeld {
                    vdisk,
                    holder: lease.holder,
                });
            }
        }
        self.lease_change(vdisk, |pool, node| pool.claim_lease(vdisk, node))
    }

    /// Open the migration window. The guest keeps running — and keeps
    /// writing — here; the destination may open the disk alongside.
    pub fn begin_handover(&mut self, vdisk: u64, to: NodeId) -> Result<()> {
        self.lease_change(vdisk, |pool, node| pool.begin_handover(vdisk, node, to))
    }

    /// Hand the pen to the destination: the instant this node stops being
    /// the writer and the other starts. Runs on the **source**, because
    /// only the holder changing the record leaves no interval in which two
    /// nodes both believe they hold it.
    pub fn relinquish(&mut self, vdisk: u64, to: NodeId) -> Result<()> {
        self.lease_change(vdisk, |pool, node| pool.relinquish_lease(vdisk, node, to))
    }

    /// Confirm the pen is here and settled — the destination's half of a
    /// migration, and deliberately a *question* rather than an act.
    ///
    /// Nothing here can take a lease. A destination that could seize one
    /// would be the second opinion this whole design exists to prevent: the
    /// source would go on believing it may write until the change reached
    /// it. So this succeeds only once the source has handed over, and until
    /// then it refuses — which is what an orchestrator waits on.
    ///
    /// "Settled" is the second half and it is not pedantry: a node holding
    /// the pen with a window still open is mid-migration, and answering yes
    /// there would let a caller treat an unfinished handover as a finished
    /// one.
    pub fn accept_handover(&mut self, vdisk: u64) -> Result<()> {
        match self.state() {
            ReplState::Synced | ReplState::Degraded | ReplState::Resyncing { .. } => {}
            _ => return Err(FsError::Suspended),
        }
        let settled = self
            .pool
            .lease(vdisk)
            .is_some_and(|lease| lease.handing_to.is_none());
        if settled && self.pool.may_write(vdisk, self.node) {
            Ok(())
        } else {
            Err(FsError::NoSuchHandover(vdisk))
        }
    }

    /// The migration failed. The window closes and the disk stays where it
    /// was — every path out of a migration closes it.
    pub fn abort_handover(&mut self, vdisk: u64) -> Result<()> {
        self.lease_change(vdisk, |pool, node| pool.abort_handover(vdisk, node))
    }

    /// Apply a lease change locally and replicate whatever it settled on.
    /// The peers are sent the resulting lease rather than the request, so
    /// no two nodes can reach different conclusions from the same words.
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
    // Reassignment — growing, shrinking, and re-protecting are one call
    // for the arithmetic and one machine for the moves.

    /// Open a reassignment toward `members` at `version`. From here until
    /// the commit, writes land on the union of old and new homes, and the
    /// moves this member owes itself are fetched by [`step_reassign`].
    /// Idempotent at the same version — the layer above re-delivers after
    /// a crash, and the arithmetic recomputes the identical map.
    pub fn prepare_reassign(&mut self, version: u64, members: &[NodeId]) -> Result<()> {
        self.pool.prepare_reassign(version, members)?;
        Ok(())
    }

    /// The version a pending reassignment is moving to, if one is open.
    pub fn reassign_pending(&self) -> Option<u64> {
        self.pool.pending_map().map(|map| map.version())
    }

    /// Drive this member's incoming moves: walk the checkpointed trees,
    /// find every data block the pending map homes here that the bricks
    /// do not hold, and ask a kept home for a batch of them. Returns how
    /// many blocks are still owed (absent and not yet requested count
    /// alike) — zero means this member's moves are complete. Idempotent
    /// and crash-safe: every call recomputes from durable state, and a
    /// re-request of a block that arrived meanwhile is a dedupe no-op.
    pub fn step_reassign(&mut self) -> Result<usize> {
        let wants = self.reassign_wants()?;
        let owed = wants.len();
        let mut batches: BTreeMap<NodeId, Vec<(u8, BlockHash)>> = BTreeMap::new();
        for (tier, hash, source) in wants {
            if self.reassign_inflight.contains_key(&(tier, hash)) {
                continue;
            }
            let streaming = self
                .peers
                .get(&source)
                .is_some_and(|session| session.stream_active());
            if !streaming {
                continue;
            }
            let batch = batches.entry(source).or_default();
            if batch.len() < SYNC_BATCH {
                batch.push((tier, hash));
                self.reassign_inflight.insert((tier, hash), source);
            }
        }
        for (source, batch) in batches {
            self.emit(Effect::Send(source, PeerMessage::Read(batch)));
        }
        Ok(owed)
    }

    /// Commit the reassignment: refused while this member still owes
    /// itself blocks — the caller confirms *every* member reports zero
    /// before committing any, because the commit is what licenses each
    /// member's collector to drop the displaced copies the others may
    /// still be pulling from.
    pub fn commit_reassign(&mut self) -> Result<()> {
        if !self.reassign_wants()?.is_empty() {
            return Err(FsError::Placement(
                "moves are still owed; a commit now would license dropping their sources",
            ));
        }
        self.pool.commit_reassign()
    }

    /// Every data block the pending map homes here that the store lacks,
    /// with the kept home to ask: `(tier, hash, source)`. Settled state
    /// only — the walk runs after a checkpoint, and the live stream keeps
    /// new writes union-homed on its own.
    fn reassign_wants(&mut self) -> Result<Vec<(u8, BlockHash, NodeId)>> {
        if self.pool.pending_map().is_none() {
            return Err(FsError::Placement("no reassignment is pending"));
        }
        self.pool.checkpoint()?;
        let (_, vdisks, snapshots) = self.pool.sync_manifest();
        let entries = map::entries_per_node(self.pool.block_size());
        let block_size = self.pool.block_size() as u64;
        let mut roots: Vec<(BlockHash, u32, u8)> = Vec::new();
        for (_, size_bytes, tier, root) in &vdisks {
            if let Some(root) = root {
                roots.push((
                    *root,
                    map::depth_for(size_bytes.div_ceil(block_size), entries),
                    *tier,
                ));
            }
        }
        for (vdisk, _, size_bytes, root) in &snapshots {
            if let Some(root) = root {
                let tier = vdisks
                    .iter()
                    .find(|(id, _, _, _)| id == vdisk)
                    .map(|(_, _, tier, _)| *tier)
                    .unwrap_or(0);
                roots.push((
                    *root,
                    map::depth_for(size_bytes.div_ceil(block_size), entries),
                    tier,
                ));
            }
        }
        let mut wants = Vec::new();
        let mut seen: HashSet<(BlockHash, u32)> = HashSet::new();
        let mut pending: Vec<(BlockHash, u32, u8)> = roots;
        while let Some((hash, kind, data_tier)) = pending.pop() {
            if !seen.insert((hash, kind)) {
                continue;
            }
            if kind > 0 {
                let payload = self
                    .pool
                    .block_payload(0, &hash)?
                    .ok_or(FsError::Corrupt("a map node vanished mid-walk"))?;
                for child in map::children(&payload) {
                    pending.push((child, kind - 1, data_tier));
                }
                continue;
            }
            let (node, map) = self.pool.placement().expect("pending implies placed");
            let committed_homes = map.homes_of(&hash);
            let pending_homes = self
                .pool
                .pending_map()
                .expect("checked above")
                .homes_of(&hash);
            // A move incoming to this member: homed under the pending map,
            // not under the committed one. The source is a kept home —
            // reassigned() never moves both homes of a slice, so one
            // committed home always survives into the pending map.
            if !pending_homes.contains(&node) || committed_homes.contains(&node) {
                continue;
            }
            if self.pool.has_block(data_tier, &hash) {
                continue;
            }
            let source = committed_homes
                .iter()
                .copied()
                .find(|home| pending_homes.contains(home))
                .or_else(|| committed_homes.first().copied())
                .ok_or(FsError::Placement("a slice with no source to pull from"))?;
            wants.push((data_tier, hash, source));
        }
        Ok(wants)
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

    /// Whether guests may be served mutations here at all: every known
    /// peer accounted for — in lockstep, fenced, or being sourced under a
    /// verdict's era — and at least one fact on the table. A node that has
    /// never met a peer and never heard a verdict does not know its
    /// standing, and a node that does not know its standing promises
    /// nothing.
    fn serving(&self) -> bool {
        !self.peers.is_empty() && self.peers.values().all(|s| s.accounted_for())
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

    /// Fan an op batch out to every streaming session, each under its own
    /// numbering. Every op here is metadata, and metadata durability is
    /// owed by every live member — the requirement that keeps "replicated
    /// everywhere" a promise rather than a distribution detail.
    fn send_ops(&mut self, ops: Vec<ReplOp>) {
        self.send_ops_requiring(ops, None);
    }

    /// The general fan-out: ops go to every streaming session (numbering
    /// must stay dense on every stream), while `required` — when given —
    /// names the only peers whose durability a flush must wait on for
    /// these ops. A data write passes its block's homes; everything else
    /// requires everyone.
    fn send_ops_requiring(&mut self, ops: Vec<ReplOp>, required: Option<&[NodeId]>) {
        if ops.is_empty() {
            return;
        }
        let recipients: Vec<NodeId> = self
            .peers
            .iter()
            .filter(|(_, s)| s.stream_active())
            .map(|(id, _)| *id)
            .collect();
        for peer in recipients {
            let session = self.peers.get_mut(&peer).expect("collected above");
            let first_rseq = session.next_rseq;
            session.next_rseq += ops.len() as u64;
            if required.is_none_or(|homes| homes.contains(&peer)) {
                session.required_rseq = session.next_rseq - 1;
            }
            self.emit(Effect::Send(
                peer,
                PeerMessage::Apply {
                    first_rseq,
                    ops: ops.clone(),
                },
            ));
        }
    }

    /// Local WAL pressure is local business: checkpoint and go again,
    /// exactly as the NBD tool does. The peers' rings are their own.
    fn with_wal_room(&mut self, mut op: impl FnMut(&mut Pool<D>) -> Result<()>) -> Result<()> {
        match op(&mut self.pool) {
            Err(FsError::WalFull) => {
                self.pool.checkpoint()?;
                op(&mut self.pool)
            }
            other => other,
        }
    }

    pub fn create_vdisk(&mut self, id: u64, size_bytes: u64, tier: u8) -> Result<()> {
        if !self.serving() {
            return Err(FsError::Suspended);
        }
        // The op is about to stream to peers that must apply it; a tier
        // some brick set cannot hold would fail that peer's replay into
        // divergence, so the promise is refused here, where the caller
        // can hear it — checked against every streaming session, because
        // any one of them is a replay that must succeed.
        for session in self.peers.values() {
            if session.stream_active() && !session.tiers.contains(&tier) {
                return Err(FsError::TierNotOnPeer(tier));
            }
        }
        self.with_wal_room(|pool| pool.create_vdisk(id, size_bytes, tier))?;
        // Whoever made it holds it, and that has to be durable before
        // anything is written to it.
        let node = self.node;
        self.pool.claim_lease(id, node)?;
        let lease = self.pool.lease(id).expect("just claimed");
        self.send_ops(vec![
            ReplOp::CreateVdisk {
                id,
                size_bytes,
                tier,
            },
            ReplOp::SetLease { vdisk: id, lease },
        ]);
        Ok(())
    }

    pub fn write_block(&mut self, vdisk: u64, index: u64, payload: &[u8]) -> Result<()> {
        self.writable(vdisk)?;
        let tier = self.pool.vdisk_tier(vdisk)?;
        let hash = crate::hash::hash_block(payload);
        // Where the block lives is the hash's decision, not the writer's:
        // its slice's homes store it — the committed ones plus, during a
        // reassignment, the pending ones, so nothing written mid-move can
        // land only on homes the commit is about to retire. Without a
        // placement every member is a home — the two-member degenerate.
        let homes: Vec<NodeId> = match self.pool.write_homes_of(&hash) {
            Some(homes) => homes,
            None => {
                let mut everyone: Vec<NodeId> = self.peers.keys().copied().collect();
                everyone.push(self.node);
                everyone
            }
        };
        // At least one home must be able to take the payload *now*: this
        // node itself, or a home whose session streams. Zero reachable
        // homes would be a write acknowledged with zero durable copies.
        let local_home = homes.contains(&self.node);
        let remote_homes: Vec<NodeId> = homes
            .iter()
            .copied()
            .filter(|home| {
                *home != self.node
                    && self
                        .peers
                        .get(home)
                        .is_some_and(|session| session.stream_active())
            })
            .collect();
        if !local_home && remote_homes.is_empty() {
            return Err(FsError::SliceUnreachable(slice::slice_of(&hash) as u8));
        }
        if local_home {
            // Hashed once, above, to place it — the store takes the
            // address instead of computing it again over 16 KiB.
            self.pool.put_block_prehashed(tier, hash, payload)?;
        }
        self.with_wal_room(|pool| pool.write_block_prehashed(vdisk, index, hash))?;
        for peer in &remote_homes {
            self.emit(Effect::Send(
                *peer,
                PeerMessage::Payloads(vec![(tier, payload.to_vec())]),
            ));
        }
        self.send_ops_requiring(
            vec![ReplOp::Write { vdisk, index, hash }],
            Some(&remote_homes),
        );
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
    /// returned ticket completes (an [`Effect::FlushDone`]) when every
    /// need is settled and the node is serving — immediately when nothing
    /// is owed, or parked until the peers' answers, a verdict, or an
    /// adoption decides its fate.
    pub fn flush(&mut self) -> Result<u64> {
        self.pool.flush()?;
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let mut needs = Vec::new();
        for (peer, session) in &self.peers {
            if session.fenced {
                continue;
            }
            // A source serving under a verdict's era owes its guests
            // continuity, not this target's durability — the verdict
            // already authorized single-copy acknowledgement.
            if session.state == (ReplState::Resyncing { source: true }) && session.serving_source {
                continue;
            }
            if session.required_rseq > session.durable_rseq {
                needs.push((*peer, session.generation, session.required_rseq));
            }
        }
        // Ask the peers that can be asked now; the rest are excused or
        // settled by resync and verdict machinery.
        for (peer, _, rseq) in &needs {
            if self.peers[peer].state == ReplState::Synced {
                self.emit(Effect::Send(*peer, PeerMessage::Flush { up_to: *rseq }));
            }
        }
        self.parked.push(ParkedFlush { ticket, needs });
        self.settle_parked();
        Ok(ticket)
    }

    /// Complete every parked flush whose needs are gone, in ticket order —
    /// but only while serving: a node that does not know its standing
    /// completes nothing, which is how a fresh flush on a suspended node
    /// parks even when nothing is numbered against it.
    fn settle_parked(&mut self) {
        if !self.serving() {
            return;
        }
        let mut still_parked = Vec::new();
        for flush in std::mem::take(&mut self.parked) {
            if flush.needs.is_empty() {
                self.effects.push_back(Effect::FlushDone(flush.ticket));
            } else {
                still_parked.push(flush);
            }
        }
        self.parked = still_parked;
    }

    /// This peer's durability is no longer owed — a verdict said there is
    /// no one to ask, or an adoption covered everything it ever missed.
    fn excuse_needs(&mut self, peer: NodeId) {
        for flush in &mut self.parked {
            flush.needs.retain(|(p, _, _)| *p != peer);
        }
        self.settle_parked();
    }

    // -----------------------------------------------------------------
    // Reads — local when this member homes the block, a fetch when it
    // does not. What is readable here without a peer is exactly what this
    // node would serve after a failover.

    /// Read one block. A block this member does not store surfaces as
    /// [`FsError::BlockElsewhere`] unless a fetch already brought it —
    /// the serve-once buffer is consumed here, which is what "keeps
    /// nothing" means in code. The caller's loop is: read, fetch on
    /// `BlockElsewhere`, read again.
    pub fn read_block(&mut self, vdisk: u64, index: u64) -> Result<Option<Vec<u8>>> {
        match self.pool.read_block(vdisk, index) {
            Err(FsError::BlockElsewhere { tier, hash }) => {
                match self.fetched.remove(&(tier, hash)) {
                    Some(payload) => Ok(Some(payload)),
                    None => Err(FsError::BlockElsewhere { tier, hash }),
                }
            }
            other => other,
        }
    }

    pub fn read_snapshot_block(
        &mut self,
        vdisk: u64,
        snapshot: u64,
        index: u64,
    ) -> Result<Option<Vec<u8>>> {
        match self.pool.read_snapshot_block(vdisk, snapshot, index) {
            Err(FsError::BlockElsewhere { tier, hash }) => {
                match self.fetched.remove(&(tier, hash)) {
                    Some(payload) => Ok(Some(payload)),
                    None => Err(FsError::BlockElsewhere { tier, hash }),
                }
            }
            other => other,
        }
    }

    /// Ask a home for a block this member does not store. The returned
    /// ticket completes as [`Effect::ReadDone`] when the payload arrives
    /// — the next [`ReplNode::read_block`] will serve it — or
    /// [`Effect::ReadFailed`] if the session dies first, in which case
    /// the caller retries and the retry asks whichever home is reachable
    /// then. Preference order is the map's: `homes[0]` first, its
    /// co-home when the first is not streaming.
    pub fn fetch_block(&mut self, tier: u8, hash: BlockHash) -> Result<u64> {
        let homes = match self.pool.placement() {
            Some((_, map)) => map.homes_of(&hash),
            None => {
                return Err(FsError::Corrupt(
                    "a fetch on a member that holds everything",
                ))
            }
        };
        let target = homes.iter().copied().find(|home| {
            *home != self.node
                && self.peers.get(home).is_some_and(|session| {
                    session.state == ReplState::Synced || session.stream_active()
                })
        });
        let Some(target) = target else {
            return Err(FsError::SliceUnreachable(slice::slice_of(&hash) as u8));
        };
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.pending_reads.insert(ticket, (tier, hash, target));
        self.emit(Effect::Send(target, PeerMessage::Read(vec![(tier, hash)])));
        Ok(ticket)
    }

    /// Local maintenance; each node runs its own on its own schedule.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.pool.checkpoint()
    }

    /// Local collection, legal at any point in a resync on either end:
    /// the per-session pins are part of the mark, so neither an offer nor
    /// a half-fetched fragment nor a payload waiting on its op can be
    /// swept out from under any session.
    pub fn collect_garbage(&mut self) -> Result<crate::store::brick::GcStats> {
        self.pool.collect_garbage()
    }

    // -----------------------------------------------------------------
    // The peers' messages.

    /// `Payloads`, with the addresses already computed from the bytes.
    ///
    /// The address must come from hashing the payload here — never from
    /// the wire, or a peer could file bytes under a lie — but *here*
    /// means this node, not this engine: the daemon hashes the batch on
    /// its reader thread, outside the engine lock, and the apply loop
    /// stops spending its lock time on arithmetic the reader could have
    /// done. The stream stays honest; only the core it burns moves.
    pub fn payloads_prehashed(
        &mut self,
        from: NodeId,
        blocks: Vec<(u8, BlockHash, Vec<u8>)>,
    ) -> Result<()> {
        if !self.peers.contains_key(&from) {
            return Err(FsError::Corrupt(
                "a message arrived from a peer that never said hello",
            ));
        }
        // A pulling target buffers rather than applies — same rule as
        // `handle`'s guard. The backlog keeps the wire shape; a replay
        // after adoption is rare enough to pay the re-hash.
        if self.peers[&from].state == (ReplState::Resyncing { source: false }) {
            let wire = blocks
                .into_iter()
                .map(|(tier, _, payload)| (tier, payload))
                .collect();
            self.session(from)
                .backlog
                .push(PeerMessage::Payloads(wire));
            return Ok(());
        }
        for (tier, hash, payload) in blocks {
            debug_assert_eq!(
                crate::hash::hash_block(&payload),
                hash,
                "a prehashed payload's address must be its content's"
            );
            // A payload lands only on a home — committed or, mid-
            // reassignment, pending; the sender routes by the same
            // union. Anything else is a stray, and a stray stored
            // here would be the third copy the accounting never
            // expects.
            let stores = self
                .pool
                .write_homes_of(&hash)
                .is_none_or(|homes| homes.contains(&self.node));
            if !stores {
                continue;
            }
            self.pool.put_block_prehashed(tier, hash, &payload)?;
            // Pinned until the op that references it lands: a
            // collection between the payload and its op would
            // otherwise sweep the block and kill the op's replay.
            self.pool.pin_inflight(from, tier, hash);
        }
        Ok(())
    }

    pub fn handle(&mut self, from: NodeId, message: PeerMessage) -> Result<()> {
        if !matches!(message, PeerMessage::Hello { .. }) && !self.peers.contains_key(&from) {
            return Err(FsError::Corrupt(
                "a message arrived from a peer that never said hello",
            ));
        }
        match message {
            PeerMessage::Hello {
                era,
                node,
                tiers,
                map_version,
            } => self.on_hello(era, node, tiers, map_version),
            PeerMessage::MapIs { version, pairs } => {
                // Bytes from a peer are claims until the shape checks.
                // Every current member ships the same map, so the second
                // arrival finds it already adopted — that is agreement,
                // not an error — and one from behind our own version is a
                // lagging peer whose next hello will get *our* MapIs.
                let mine = self.map_version();
                if version > mine {
                    let map = slice::SliceMap::from_pairs(version, pairs)?;
                    self.pool.adopt_map(map)?;
                }
                if version >= mine {
                    // Re-hello under the agreed version: the election that
                    // was deferred can run now, on agreeing maps.
                    self.connect(from);
                }
                Ok(())
            }
            // A pulling target buffers the source's live stream rather than
            // applying it: its own state is about to be replaced by the
            // adoption, and anything applied before that would be applied
            // to a history that is going away. The replay after adoption is
            // what lands the target on the source's *live* state.
            PeerMessage::Payloads(_) | PeerMessage::Apply { .. }
                if self.peers[&from].state == (ReplState::Resyncing { source: false }) =>
            {
                self.session(from).backlog.push(message);
                Ok(())
            }
            PeerMessage::Payloads(payloads) => {
                let hashed = payloads
                    .into_iter()
                    .map(|(tier, payload)| {
                        let hash = crate::hash::hash_block(&payload);
                        (tier, hash, payload)
                    })
                    .collect();
                self.payloads_prehashed(from, hashed)
            }
            PeerMessage::Apply { first_rseq, ops } => self.on_apply(from, first_rseq, ops),
            PeerMessage::Flush { up_to } => {
                if self.peers[&from].applied_rseq < up_to {
                    return Err(FsError::Corrupt(
                        "a flush asks for ops the stream never delivered",
                    ));
                }
                self.pool.flush()?;
                self.emit(Effect::Send(from, PeerMessage::Durable { up_to }));
                Ok(())
            }
            PeerMessage::Durable { up_to } => {
                let session = self.session(from);
                session.durable_rseq = session.durable_rseq.max(up_to);
                let durable = session.durable_rseq;
                let generation = session.generation;
                for flush in &mut self.parked {
                    flush.needs.retain(|(p, generation_then, rseq)| {
                        !(*p == from && *generation_then == generation && *rseq <= durable)
                    });
                }
                self.settle_parked();
                Ok(())
            }
            PeerMessage::SyncStart => self.on_sync_start(from),
            PeerMessage::SyncManifest(offer) => self.on_sync_manifest(from, offer),
            PeerMessage::SyncNeed(hashes) => {
                let mut payloads = Vec::with_capacity(hashes.len());
                for (tier, hash) in hashes {
                    match self.pool.block_payload(tier, &hash)? {
                        Some(payload) => payloads.push((tier, payload)),
                        None => {
                            return Err(FsError::Corrupt(
                                "the peer asked for a block this store does not hold",
                            ))
                        }
                    }
                }
                self.emit(Effect::Send(from, PeerMessage::SyncData(payloads)));
                Ok(())
            }
            PeerMessage::Read(hashes) => {
                let mut payloads = Vec::with_capacity(hashes.len());
                for (tier, hash) in hashes {
                    match self.pool.block_payload(tier, &hash)? {
                        Some(payload) => payloads.push((tier, payload)),
                        None => {
                            return Err(FsError::Corrupt(
                                "the peer asked for a block this store does not hold",
                            ))
                        }
                    }
                }
                self.emit(Effect::Send(from, PeerMessage::ReadData(payloads)));
                Ok(())
            }
            PeerMessage::ReadData(payloads) => {
                for (tier, payload) in payloads {
                    let hash = crate::hash::hash_block(&payload);
                    // A home stores what arrives — that is the repair, the
                    // refill, and a reassignment move landing (homes are
                    // the union of committed and pending); a non-home
                    // serves the read and keeps nothing beyond the one
                    // consumption.
                    let stores = self
                        .pool
                        .write_homes_of(&hash)
                        .is_some_and(|homes| homes.contains(&self.node));
                    if stores {
                        self.pool.put_block(tier, &payload)?;
                        self.reassign_inflight.remove(&(tier, hash));
                    } else {
                        self.fetched.insert((tier, hash), payload);
                    }
                    let done: Vec<u64> = self
                        .pending_reads
                        .iter()
                        .filter(|(_, (t, h, _))| *t == tier && *h == hash)
                        .map(|(ticket, _)| *ticket)
                        .collect();
                    for ticket in done {
                        self.pending_reads.remove(&ticket);
                        self.emit(Effect::ReadDone(ticket));
                    }
                }
                Ok(())
            }
            PeerMessage::SyncData(payloads) => self.on_sync_data(from, payloads),
            PeerMessage::SyncReady => self.on_sync_ready(from),
            PeerMessage::SyncAdopt { final_rseq } => self.on_sync_adopt(from, final_rseq),
            PeerMessage::SyncDone { era: _ } => {
                // The target adopted this node's state and replayed the
                // stream; lockstep continues on the numbering the stream
                // already established — resetting it here would orphan the
                // ops sent while sourcing. Every need this peer was owed
                // is covered by the adoption; anything still parked waits
                // only on the *other* peers.
                let session = self.session(from);
                session.state = ReplState::Synced;
                session.serving_source = false;
                session.stream_live = false;
                self.pool.unpin_sync(from);
                self.excuse_needs(from);
                Ok(())
            }
        }
    }

    fn on_hello(
        &mut self,
        peer_era: u64,
        peer_node: NodeId,
        peer_tiers: Vec<u8>,
        peer_map_version: u64,
    ) -> Result<()> {
        // The maps must agree before roles are elected: a member behind on
        // the map cannot compute the current one (the arithmetic chains
        // from its predecessor), so the higher side ships it whole and the
        // lower side re-hellos once it has persisted the adoption.
        let my_map_version = self.map_version();
        if peer_map_version < my_map_version {
            let (_, map) = self.pool.placement().expect("a version implies a map");
            let pairs = map.to_pairs();
            let version = map.version();
            self.session(peer_node);
            self.emit(Effect::Send(
                peer_node,
                PeerMessage::MapIs { version, pairs },
            ));
            // And a fresh hello behind it: the peer elects roles only on
            // *receiving* a hello, and the one that provoked this reply
            // was spent learning it was behind.
            self.connect(peer_node);
            return Ok(());
        }
        if peer_map_version > my_map_version {
            // The peer saw our hello lower and is sending the map; the
            // session waits for it.
            self.session(peer_node);
            return Ok(());
        }
        // Both sides compute the same answer from the same pair: higher
        // era is the source; equal eras fall to the lower node id — a tie
        // means both hold every acknowledged write, and the diff is cheap.
        let my_era = self.pool.era();
        let my_node = self.node;
        self.pool.drop_session_pins(peer_node);
        let session = self.session(peer_node);
        session.era_seen = session.era_seen.max(peer_era);
        session.tiers = peer_tiers;
        // A member that says hello is not fenced; a verdict is an event,
        // not a sentence, and the returning node is about to be reconciled
        // by the resync this hello opens.
        session.fenced = false;
        // A hello opens a session; nothing numbered under an earlier one
        // may be mistaken for part of this one.
        session.generation += 1;
        session.next_rseq = 1;
        session.durable_rseq = 0;
        session.required_rseq = 0;
        session.applied_rseq = 0;
        session.pull = SyncPull::default();
        session.backlog.clear();
        let source = my_era > peer_era || (my_era == peer_era && my_node < peer_node);
        if source {
            session.state = ReplState::Resyncing { source: true };
            // Era superiority is a fence verdict made durable, and the
            // verdict is what lets this node keep acknowledging alone
            // while the stale peer catches up. A tie has no verdict, so
            // the tie's source suspends its guests as it always has.
            session.serving_source = my_era > peer_era;
            session.stream_live = false;
        } else {
            session.state = ReplState::Resyncing { source: false };
            session.serving_source = false;
            session.stream_live = false;
            // A fresh rejoin wave starts a fresh record of what the live
            // members still offer; a wave already running keeps its own.
            if !self.peers.iter().any(|(id, s)| {
                *id != peer_node && s.state == (ReplState::Resyncing { source: false })
            }) {
                self.offered_vdisks.clear();
            }
            self.emit(Effect::Send(peer_node, PeerMessage::SyncStart));
        }
        Ok(())
    }

    fn on_sync_start(&mut self, from: NodeId) -> Result<()> {
        if self.peers[&from].state != (ReplState::Resyncing { source: true }) {
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
        let pins = offer_pins(&offer, self.pool.block_size());
        self.pool.pin_sync(from, pins);
        // From exactly this checkpoint onward, ops stream. Earlier writes
        // are inside the offer; later ones are on the wire; nothing is in
        // both places.
        let serving = self.peers[&from].serving_source;
        self.session(from).stream_live = serving;
        self.emit(Effect::Send(from, PeerMessage::SyncManifest(offer)));
        Ok(())
    }

    /// Resolve everything queued in this session's pull: walk down through
    /// what this node already holds, and list what it does not.
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
    fn resolve_pending(&mut self, from: NodeId) -> Result<()> {
        loop {
            let entry = {
                let session = self.session(from);
                session.pull.pending.pop()
            };
            let Some((hash, kind, data_tier)) = entry else {
                return Ok(());
            };
            if !self.session(from).pull.seen.insert((hash, kind, data_tier)) {
                continue;
            }
            // Data blocks slice; map nodes do not. A data block this
            // member does not home is not fetched at all, and one whose
            // co-home is a different member is that member's session's
            // job — every pull walks every tree (metadata is everywhere),
            // and each fetches exactly the subset its own source homes,
            // so between the pulls a rejoining home refills completely.
            // Homes are the union of committed and pending maps: a pull
            // overlapping a reassignment fetches for both.
            if kind == 0 {
                if let Some(homes) = self.pool.write_homes_of(&hash) {
                    if !homes.contains(&self.node) || !homes.contains(&from) {
                        continue;
                    }
                }
            }
            // A node lives on tier 0; a kind-0 entry is data on its
            // vdisk's tier. That pair is the whole address.
            let store_tier = if kind > 0 { 0 } else { data_tier };
            if self.pool.has_block(store_tier, &hash) {
                if kind > 0 {
                    let payload = self
                        .pool
                        .block_payload(0, &hash)?
                        .ok_or(FsError::Corrupt("a held block vanished mid-resync"))?;
                    let session = self.session(from);
                    for child in map::children(&payload) {
                        session.pull.pending.push((child, kind - 1, data_tier));
                    }
                }
                continue;
            }
            let session = self.session(from);
            let entry = session.pull.wanted.entry((hash, store_tier)).or_default();
            if entry.is_empty() {
                session.pull.unrequested.push((store_tier, hash));
            }
            if !entry.contains(&(kind, data_tier)) {
                entry.push((kind, data_tier));
            }
        }
    }

    fn request_more(&mut self, from: NodeId) {
        let batch = {
            let session = self.session(from);
            if session.pull.outstanding > 0 || session.pull.unrequested.is_empty() {
                return;
            }
            let take = session.pull.unrequested.len().min(SYNC_BATCH);
            let batch: Vec<(u8, BlockHash)> = session.pull.unrequested.drain(..take).collect();
            session.pull.outstanding = batch.len();
            batch
        };
        self.emit(Effect::Send(from, PeerMessage::SyncNeed(batch)));
    }

    fn after_pull_progress(&mut self, from: NodeId) {
        let complete = {
            let session = self.session(from);
            session.pull.pending.is_empty()
                && session.pull.wanted.is_empty()
                && session.pull.unrequested.is_empty()
        };
        if complete {
            self.emit(Effect::Send(from, PeerMessage::SyncReady));
        } else {
            self.request_more(from);
        }
    }

    fn on_sync_manifest(&mut self, from: NodeId, offer: SyncOffer) -> Result<()> {
        if self.peers[&from].state != (ReplState::Resyncing { source: false }) {
            return Err(FsError::Corrupt(
                "offered a sync manifest while not pulling",
            ));
        }
        // A vdisk on a tier this set lacks has nowhere for its data. The
        // deployment error — asymmetric brick sets — is refused by name
        // before a single block moves, not discovered at adoption.
        for (_, _, tier, _) in &offer.vdisks {
            if !self.pool.tiers().contains(tier) {
                return Err(FsError::NoSuchTier(*tier));
            }
        }
        // Everything this pull fetches hangs off these roots, referenced
        // by nothing in this node's own manifest until adoption. The pull
        // arrives top-down, so the pins reach every fetched block through
        // blocks the store holds.
        let pins = offer_pins(&offer, self.pool.block_size());
        self.pool.pin_sync(from, pins);
        // What this wave's live members still carry — the survivors of
        // the post-adoption cleanup. Accumulated across every offer,
        // because no single member speaks for the set.
        for (id, _, _, _) in &offer.vdisks {
            self.offered_vdisks.insert(*id);
        }
        let entries = map::entries_per_node(self.pool.block_size());
        let block_size = self.pool.block_size() as u64;
        // A root of a depth-d tree is kind d: its children are kind d-1,
        // and kind 0 is a data block on the vdisk's tier.
        let session = self.session(from);
        for (_, size_bytes, tier, root) in &offer.vdisks {
            if let Some(root) = root {
                let depth = map::depth_for(size_bytes.div_ceil(block_size), entries);
                session.pull.pending.push((*root, depth, *tier));
            }
        }
        for (vdisk, _, size_bytes, root) in &offer.snapshots {
            if let Some(root) = root {
                let depth = map::depth_for(size_bytes.div_ceil(block_size), entries);
                session
                    .pull
                    .pending
                    .push((*root, depth, offer_vdisk_tier(&offer, *vdisk)));
            }
        }
        session.pull.manifest = Some(offer);
        self.resolve_pending(from)?;
        self.after_pull_progress(from);
        Ok(())
    }

    fn on_sync_data(&mut self, from: NodeId, payloads: Vec<(u8, Vec<u8>)>) -> Result<()> {
        if self.peers[&from].state != (ReplState::Resyncing { source: false }) {
            return Err(FsError::Corrupt("sync data arrived while not pulling"));
        }
        for (store_tier, payload) in payloads {
            let hash = self.pool.put_block(store_tier, &payload)?;
            let session = self.session(from);
            session.pull.outstanding = session.pull.outstanding.saturating_sub(1);
            let continuations = session
                .pull
                .wanted
                .remove(&(hash, store_tier))
                .unwrap_or_default();
            for (kind, data_tier) in continuations {
                // The block is here now; walking it from `pending` keeps
                // one descent path for arrived and already-held blocks
                // alike.
                session.pull.seen.remove(&(hash, kind, data_tier));
                session.pull.pending.push((hash, kind, data_tier));
            }
        }
        self.resolve_pending(from)?;
        self.after_pull_progress(from);
        Ok(())
    }

    /// The target asked leave to adopt. Granting it means stopping the
    /// solo-acknowledgement regime *first*: from here until SyncDone, this
    /// node refuses writes — a round trip plus the stream still in flight,
    /// the daemon's queue-and-retry window — so that no acknowledged write
    /// can exist that the adopting peer lacks. The fence in the reply is
    /// its position: FIFO delivery puts every streamed op before it.
    fn on_sync_ready(&mut self, from: NodeId) -> Result<()> {
        if self.peers[&from].state != (ReplState::Resyncing { source: true }) {
            return Err(FsError::Corrupt(
                "a sync-ready arrived at a node that is not sourcing",
            ));
        }
        let final_rseq = {
            let session = self.session(from);
            session.serving_source = false;
            session.next_rseq - 1
        };
        self.emit(Effect::Send(from, PeerMessage::SyncAdopt { final_rseq }));
        Ok(())
    }

    fn on_sync_adopt(&mut self, from: NodeId, final_rseq: u64) -> Result<()> {
        let complete = {
            let session = &self.peers[&from];
            session.state == (ReplState::Resyncing { source: false })
                && session.pull.pending.is_empty()
                && session.pull.wanted.is_empty()
                && session.pull.unrequested.is_empty()
        };
        if !complete {
            return Err(FsError::Corrupt(
                "told to adopt without a completed pull to adopt",
            ));
        }
        let offer = self
            .session(from)
            .pull
            .manifest
            .take()
            .ok_or(FsError::Corrupt(
                "finishing a pull that never had a manifest",
            ))?;
        let era = offer.era;
        if self.pool.placement().is_some() {
            // Per-vdisk adoption: every op for a vdisk originates at its
            // lease holder, so only the holder's offer speaks for it —
            // another source's copy is missing whatever the holder wrote
            // since that source's checkpoint. Vdisks held by no live
            // pulling source (this node's own from before it died, a
            // fenced member's, none at all) are identical on every synced
            // member and adopted from whichever offer arrives.
            let other_pulling: Vec<NodeId> = self
                .peers
                .iter()
                .filter(|(id, s)| {
                    **id != from && s.state == (ReplState::Resyncing { source: false })
                })
                .map(|(id, _)| *id)
                .collect();
            let holder_of = |vdisk: u64| {
                offer
                    .leases
                    .iter()
                    .find(|(id, _)| *id == vdisk)
                    .map(|(_, lease)| lease.holder)
            };
            let adopt: Vec<u64> = offer
                .vdisks
                .iter()
                .map(|(id, _, _, _)| *id)
                .filter(|id| match holder_of(*id) {
                    Some(holder) => holder == from || !other_pulling.contains(&holder),
                    None => true,
                })
                .collect();
            let vdisks: Vec<VdiskOffer> = offer
                .vdisks
                .iter()
                .filter(|(id, _, _, _)| adopt.contains(id))
                .copied()
                .collect();
            let snapshots: Vec<SnapshotOffer> = offer
                .snapshots
                .iter()
                .filter(|(vdisk, _, _, _)| adopt.contains(vdisk))
                .copied()
                .collect();
            let leases: Vec<(u64, Lease)> = offer
                .leases
                .iter()
                .filter(|(vdisk, _)| adopt.contains(vdisk))
                .copied()
                .collect();
            self.pool
                .adopt_sync(era, &vdisks, &snapshots, &leases, AdoptScope::Named)?;
        } else {
            self.pool.adopt_sync(
                era,
                &offer.vdisks,
                &offer.snapshots,
                &offer.leases,
                AdoptScope::Whole,
            )?;
        }
        // The leases came over inside the adoption: the source's view of
        // who may write is part of the state being adopted, not a separate
        // negotiation.
        let session = self.session(from);
        session.pull = SyncPull::default();
        session.state = ReplState::Synced;
        self.pool.unpin_sync(from);
        // The offer was the source at its checkpoint; the backlog is
        // everything its guests did since. Replayed through the ordinary
        // handlers — sequence checks and all — it lands this node on the
        // source's live state, and lockstep continues on that numbering.
        let backlog = std::mem::take(&mut self.session(from).backlog);
        for message in backlog {
            self.handle(from, message)?;
        }
        if self.peers[&from].applied_rseq != final_rseq {
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
        for flush in parked {
            self.emit(Effect::FlushFailed(flush.ticket));
        }
        // The wave's last adoption reconciles deletions: Named adoption
        // never removes, so a vdisk no live member offered — deleted
        // while this node was away — is dropped by the survivors' union.
        if self.pool.placement().is_some()
            && !self
                .peers
                .values()
                .any(|s| s.state == (ReplState::Resyncing { source: false }))
        {
            let keep = std::mem::take(&mut self.offered_vdisks);
            self.pool.retain_vdisks(&keep)?;
        }
        self.emit(Effect::Send(from, PeerMessage::SyncDone { era }));
        Ok(())
    }

    fn on_apply(&mut self, from: NodeId, first_rseq: u64, ops: Vec<ReplOp>) -> Result<()> {
        if first_rseq != self.peers[&from].applied_rseq + 1 {
            return Err(FsError::Corrupt("the op stream skipped or repeated"));
        }
        let count = ops.len() as u64;
        for op in ops {
            let mapped = match &op {
                ReplOp::Write { vdisk, hash, .. } => Some((*vdisk, *hash)),
                _ => None,
            };
            self.apply_op(op)?;
            // The op landed; the payload it references is reachable from
            // durable state now and no longer needs its arrival pin.
            if let Some((vdisk, hash)) = mapped {
                let tier = self.pool.vdisk_tier(vdisk)?;
                self.pool.clear_inflight(from, tier, &hash);
            }
        }
        self.session(from).applied_rseq += count;
        Ok(())
    }

    fn apply_op(&mut self, op: ReplOp) -> Result<()> {
        match op {
            ReplOp::CreateVdisk {
                id,
                size_bytes,
                tier,
            } => self.with_wal_room(|pool| pool.create_vdisk(id, size_bytes, tier)),
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
    use crate::disk::sim::SimDisk;
    use crate::store::brick::{Brick, BrickParams};

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
                tier: 0,
                wal_holder: true,
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
            a.create_vdisk(1, 100 * 4096, 0).unwrap_err(),
            FsError::Suspended
        );
        let ticket = a.flush().unwrap();
        // Parked: no FlushDone among the effects.
        assert!(a
            .take_effects()
            .iter()
            .all(|e| !matches!(e, Effect::FlushDone(_))));
        // The verdict releases it.
        let era = a.era_target();
        a.set_member_fenced(1, era).unwrap();
        assert_eq!(a.state(), ReplState::Degraded);
        assert!(a.take_effects().contains(&Effect::FlushDone(ticket)));
    }

    #[test]
    fn a_degraded_node_acknowledges_alone_and_bumped_its_era() {
        let mut a = node(2, 0);
        assert_eq!(a.pool().era(), 1);
        let era = a.era_target();
        a.set_member_fenced(1, era).unwrap();
        assert_eq!(a.pool().era(), 2);
        a.create_vdisk(1, 100 * 4096, 0).unwrap();
        a.write_block(1, 0, b"alone but honest").unwrap();
        let ticket = a.flush().unwrap();
        assert!(a.take_effects().contains(&Effect::FlushDone(ticket)));
    }

    #[test]
    fn a_verdict_for_a_present_peer_is_refused() {
        let mut a = node(3, 0);
        let era = a.era_target();
        a.set_member_fenced(1, era).unwrap();
        // Already fenced: a second verdict makes no sense.
        let era = a.era_target();
        assert!(a.set_member_fenced(1, era).is_err());
    }

    #[test]
    fn a_verdict_below_the_witnessed_floor_is_refused() {
        let mut a = node(6, 0);
        // The node's own era is 1, so the target floor is 2; a verdict
        // naming the era already lived in cannot fence anyone honestly.
        assert!(a.set_member_fenced(1, 1).is_err());
        // And the honest number still works.
        a.set_member_fenced(1, a.era_target()).unwrap();
        assert_eq!(a.pool().era(), 2);
    }

    #[test]
    fn only_the_writer_writes() {
        let mut a = node(4, 0);
        let era = a.era_target();
        a.set_member_fenced(1, era).unwrap();
        a.create_vdisk(1, 100 * 4096, 0).unwrap();
        a.take_effects();
        // Another node's view: same vdisk, no writer role.
        let mut b = node(5, 1);
        let era = b.era_target();
        b.set_member_fenced(0, era).unwrap();
        b.pool_create_for_test(1);
        assert_eq!(
            b.write_block(1, 0, b"not mine").unwrap_err(),
            FsError::NotWriter(1)
        );
    }

    #[test]
    fn an_era_stale_lease_of_a_live_member_is_not_claimable() {
        let mut a = node(7, 0);
        a.set_member_fenced(1, a.era_target()).unwrap();
        a.create_vdisk(1, 100 * 4096, 0).unwrap();
        // Hand the record to node 2 (live, never fenced), then age it: a
        // second verdict against a different member bumps the era past
        // the lease.
        let lease = Lease {
            holder: 2,
            handing_to: None,
            era: a.pool().era(),
        };
        a.pool.set_lease(1, lease).unwrap();
        a.set_member_fenced(3, a.era_target()).unwrap();
        // The record is stale but its holder is alive somewhere: claiming
        // would make a second writer.
        assert_eq!(
            a.claim_writer(1).unwrap_err(),
            FsError::LeaseHeld {
                vdisk: 1,
                holder: 2
            }
        );
    }

    impl ReplNode<SimDisk> {
        fn pool_create_for_test(&mut self, id: u64) {
            // Create the vdisk without claiming the writer role, as an
            // applier would.
            self.pool.create_vdisk(id, 100 * 4096, 0).unwrap();
        }
    }
}

#[cfg(test)]
mod coalesce_tests {
    use super::*;

    fn payload(byte: u8) -> (u8, Vec<u8>) {
        (0, vec![byte; 4])
    }

    fn op(index: u64) -> ReplOp {
        ReplOp::Trim { vdisk: 1, index }
    }

    /// The shape a large guest write actually emits — payload, apply,
    /// payload, apply — folds to one of each, order preserved: every
    /// payload still precedes the op that references it.
    #[test]
    fn a_writes_worth_of_chatter_becomes_two_messages() {
        let out = coalesce(vec![
            Effect::Send(1, PeerMessage::Payloads(vec![payload(1)])),
            Effect::Send(
                1,
                PeerMessage::Apply {
                    first_rseq: 10,
                    ops: vec![op(0)],
                },
            ),
            Effect::Send(1, PeerMessage::Payloads(vec![payload(2)])),
            Effect::Send(
                1,
                PeerMessage::Apply {
                    first_rseq: 11,
                    ops: vec![op(1)],
                },
            ),
        ]);
        assert_eq!(
            out,
            vec![
                Effect::Send(1, PeerMessage::Payloads(vec![payload(1), payload(2)])),
                Effect::Send(
                    1,
                    PeerMessage::Apply {
                        first_rseq: 10,
                        ops: vec![op(0), op(1)],
                    },
                ),
            ]
        );
    }

    /// The stream numbering is the contract: a gap means something
    /// happened in between, and the messages stay apart.
    #[test]
    fn a_sequence_gap_refuses_to_merge() {
        let effects = vec![
            Effect::Send(
                1,
                PeerMessage::Apply {
                    first_rseq: 10,
                    ops: vec![op(0)],
                },
            ),
            Effect::Send(
                1,
                PeerMessage::Apply {
                    first_rseq: 12,
                    ops: vec![op(1)],
                },
            ),
        ];
        assert_eq!(coalesce(effects.clone()), effects);
    }

    /// Any other message is a barrier — a flush's position against the
    /// payloads around it survives exactly as emitted.
    #[test]
    fn a_barrier_stops_the_merge_and_peers_merge_independently() {
        let out = coalesce(vec![
            Effect::Send(1, PeerMessage::Payloads(vec![payload(1)])),
            Effect::Send(2, PeerMessage::Payloads(vec![payload(9)])),
            Effect::Send(1, PeerMessage::Flush { up_to: 5 }),
            Effect::Send(1, PeerMessage::Payloads(vec![payload(2)])),
            Effect::Send(2, PeerMessage::Payloads(vec![payload(8)])),
        ]);
        assert_eq!(
            out,
            vec![
                Effect::Send(1, PeerMessage::Payloads(vec![payload(1)])),
                // Peer 2 merges across peer 1's barrier: barriers are
                // per-stream facts.
                Effect::Send(2, PeerMessage::Payloads(vec![payload(9), payload(8)])),
                Effect::Send(1, PeerMessage::Flush { up_to: 5 }),
                Effect::Send(1, PeerMessage::Payloads(vec![payload(2)])),
            ]
        );
    }

    /// The pattern a non-home writer sends a home: an op for one block,
    /// then another block's payload, then its op. The second op must NOT
    /// merge backward past the payload it references — the three-node
    /// sim's refusal ("a replicated write names a block the store does
    /// not hold") is what this pins.
    #[test]
    fn an_op_never_merges_backward_past_its_own_payload() {
        let effects = vec![
            Effect::Send(
                1,
                PeerMessage::Apply {
                    first_rseq: 10,
                    ops: vec![op(0)],
                },
            ),
            Effect::Send(1, PeerMessage::Payloads(vec![payload(2)])),
            Effect::Send(
                1,
                PeerMessage::Apply {
                    first_rseq: 11,
                    ops: vec![op(1)],
                },
            ),
        ];
        assert_eq!(coalesce(effects.clone()), effects);
    }

    /// Non-send effects answer local waiters, not the wire — they pass
    /// through in place and merge nothing.
    #[test]
    fn local_effects_pass_through_untouched() {
        let effects = vec![
            Effect::FlushDone(7),
            Effect::Send(1, PeerMessage::Payloads(vec![payload(1)])),
            Effect::ReadDone(9),
            Effect::Send(1, PeerMessage::Payloads(vec![payload(2)])),
        ];
        let out = coalesce(effects);
        assert_eq!(
            out,
            vec![
                Effect::FlushDone(7),
                Effect::Send(1, PeerMessage::Payloads(vec![payload(1), payload(2)])),
                Effect::ReadDone(9),
            ]
        );
    }
}
