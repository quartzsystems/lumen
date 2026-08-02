//! The daemon core: one [`ReplNode`] bound to real sockets and threads.
//!
//! Everything that can corrupt data already lives in the engine; this
//! module's whole job is to be a faithful harness for it — the same role
//! the test pump plays under simulation, played against reality:
//!
//! - **Every effect is drained under the engine lock**, in the order the
//!   engine emitted it: sends to the outbound queue, flush fates to the
//!   board guests wait on. Two threads can call into the engine, but the
//!   wire sees one ordered stream.
//! - **Bytes never cross connection incarnations.** That is the daemon's
//!   half of the contract repl.rs states: every session boundary bumps the
//!   incarnation and clears the outbound queue, the writer thread quits
//!   the moment its incarnation is stale, and a session that dies tears
//!   itself down exactly once — the incarnation check is what keeps a late
//!   teardown from knocking a Degraded node back to Suspended after a
//!   fence verdict already landed.
//! - **The engine never learns policy.** When to checkpoint, when to
//!   collect, what to do about a full ring — the maintenance thread and
//!   the room-making retry own that, exactly as the smoke tool does.
//!
//! Suspension is real here: a guest write against a suspended node blocks
//! (with the engine's own `Suspended` as the wakeable condition), and a
//! guest flush blocks until its ticket completes — which may be never,
//! until a verdict or a reconciliation decides. That is DRBD's suspended
//! I/O, honestly reproduced.
//!
//! The peer link is one TCP connection. The configured listener accepts;
//! the configured dialer redials with backoff; whichever side notices
//! death first tears the session down and the engine suspends. A fence
//! verdict arriving while the socket still looks alive (a pulled cable is
//! silent) forces the link down first — the verdict's authority does not
//! queue behind a TCP timeout.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lumen_fs::disk::file::{is_block_device, FileDisk};
use lumen_fs::{
    Brick, BrickParams, BrickSet, BrickStats, ByteView, Disk, Effect, FsError, GcStats, Lease,
    PeerMessage, Pool, ReplNode, ReplState, SpaceReport,
};

use crate::wire::{self, Handshake, HANDSHAKE_LEN, MAX_FRAME};

/// How long the dialer waits between failed connection attempts.
const REDIAL: Duration = Duration::from_millis(250);
/// The wait quantum for every blocking condition — short enough that
/// shutdown is prompt, long enough to cost nothing.
const TICK: Duration = Duration::from_millis(100);
/// Maintenance cadence: the space check runs every tick, the periodic
/// checkpoint every CHECKPOINT_TICKS of them.
const MAINTENANCE_TICK: Duration = Duration::from_millis(1000);
const CHECKPOINT_TICKS: u32 = 5;

pub struct Config {
    pub node: u8,
    /// Every brick of this node's set, one path per disk. The set's own
    /// checks decide whether they belong together; the daemon just opens
    /// what it is given and refuses to guess.
    pub bricks: Vec<PathBuf>,
    /// Accept peers here. The mesh convention: every member listens and
    /// each dials every lower-id member, so a pair has exactly one dialer
    /// — decided by deployment, carried in the conf.
    pub listen: Option<SocketAddr>,
    /// Dial these peers — one address per lower-id member.
    pub dials: Vec<SocketAddr>,
    /// The pool's member ids, for placement. Empty is the two-member
    /// legacy: unplaced, every member holds everything. With members the
    /// pool opens placed — the manifest's committed map when one is
    /// anchored, the derived first map when the brick is virgin.
    pub members: Vec<u8>,
}

/// What the daemon will say about itself, to the console and the tests.
#[derive(Debug, Clone)]
pub struct Status {
    pub node: u8,
    pub state: ReplState,
    pub era: u64,
    pub accepts_writes: bool,
    pub vdisks: Vec<(u64, u64)>,
    pub leases: Vec<(u64, Lease)>,
    pub space: BrickStats,
    /// The same space said in bytes, per tier and per brick.
    pub report: SpaceReport,
    /// `(sent, peer_confirmed_durable, applied_from_peer)` — the busiest
    /// stream's triple, kept for the two-member surface.
    pub stream: (u64, u64, u64),
    /// The same, per peer — the mesh's honest form.
    pub peers: Vec<(u8, u64, u64, u64)>,
    /// The floor a fence-verdict era must clear from this node — what the
    /// verdict layer maxes over survivors to agree one number.
    pub era_target: u64,
    /// The committed slice map's version; `None` when unplaced.
    pub map_version: Option<u64>,
    /// How many of the 256 slices this member homes under the committed
    /// map — what a pool-wide capacity divides each member's bytes by.
    pub seats: Option<u64>,
    /// The pool identity the bricks were formatted into — what a grow
    /// workflow formats a newcomer's bricks with.
    pub pool_uuid: [u8; 16],
    /// The version a reassignment is moving to, while one is open.
    pub reassign_pending: Option<u64>,
    /// A background scrub in flight: `(records verified, records total)`.
    /// Absent when none is running — progress is a fact about a pass, not
    /// about the node.
    pub scrub: Option<(u64, u64)>,
}

/// What an attach got: the pen, or a seat beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// This node holds the writer lease; the guest may write.
    Writer,
    /// Opened inside a migration window aimed here. Reads serve, writes
    /// refuse, and the pen arrives with the handover's accept.
    Penless,
}

/// A live guest export, held by the daemon so it can be taken down —
/// abstract because the only implementation is Linux-only, and the
/// daemon's own bookkeeping should not be.
pub trait ExportControl: Send {
    /// For the status line: what and where.
    fn describe(&self) -> String;
    /// Tear the export down and wait for its servicing to end.
    fn stop(self: Box<Self>) -> Result<(), String>;
}

/// One peer's link: its outbound queue, its session generation, and the
/// live socket if any. The mesh is a map of these, keyed by the peer's
/// node id from the handshake — a new connection replaces only *that*
/// peer's session.
#[derive(Default)]
struct PeerLink {
    queue: VecDeque<PeerMessage>,
    /// What the queue holds, in [`send_cost`] terms. This is the number
    /// the guest-write brake reads: the queue is unbounded by structure,
    /// and an unbounded queue is exactly how a fast local engine built
    /// gigabytes of unflushed debt a first fsync then paid for all at
    /// once — the intermittent 88-second syncwrite collapse, measured.
    queued_bytes: usize,
    /// The session generation. Bumped at every session start, every
    /// session death, and every forced link reset — and checked by the
    /// writer and by teardown, so nothing written for one session can act
    /// on another.
    incarnation: u64,
    socket: Option<TcpStream>,
}

/// What one queued message costs against [`SEND_QUEUE_BYTES`]: its
/// payload bytes plus a small constant for the frame around them. An
/// estimate on purpose — the brake needs a bound, not an audit.
fn send_cost(message: &PeerMessage) -> usize {
    64 + wire::payload_list(message)
        .map_or(0, |payloads| payloads.iter().map(|(_, p)| p.len()).sum())
}

/// How much may sit in one peer's outbound queue before guest writes
/// wait for the writer to catch up. This bounds the debt a guest fsync
/// can inherit: at most this much still queued, plus the TCP buffers and
/// the peer's ingest pipeline — a bounded second or so of apply, where
/// the unbounded queue was a bounded nothing. It also bounds the
/// daemon's memory, which the queue alone never did.
const SEND_QUEUE_BYTES: usize = 128 << 20;

#[derive(Default)]
struct Flushes {
    /// Ticket → completed honestly. Entries are removed by the waiter.
    outcomes: HashMap<u64, bool>,
}

#[derive(Default)]
struct Reads {
    /// Fetch ticket → the payload arrived. Entries are removed by the
    /// waiter; a `false` is a dead session, and the waiter's retry asks
    /// whichever home is reachable then.
    outcomes: HashMap<u64, bool>,
}

struct Shared {
    node_id: u8,
    engine: Mutex<ReplNode<FileDisk>>,
    /// Notified after every engine call — the wakeable "something changed"
    /// every blocked guest operation waits on. Paired with `engine`.
    changed: Condvar,
    /// Every peer's link, keyed by node id — sockets held so a verdict or
    /// a replacement connection can kill exactly one peer's session from
    /// outside its own threads.
    links: Mutex<HashMap<u8, PeerLink>>,
    out_ready: Condvar,
    /// Notified when a peer queue shrinks below the cap (and whenever a
    /// session bounces, which empties one) — what a guest write blocked
    /// on [`SEND_QUEUE_BYTES`] waits for. Paired with `links`.
    send_room: Condvar,
    flushes: Mutex<Flushes>,
    flush_ready: Condvar,
    reads: Mutex<Reads>,
    read_ready: Condvar,
    /// The background scrub, at most one at a time. Its own board rather
    /// than engine state: pacing and progress are the shell's policy,
    /// exactly as checkpoint cadence is.
    scrub: Mutex<ScrubBoard>,
    shutdown: AtomicBool,
}

/// What the scrub thread reports and the status verbs read.
#[derive(Default)]
struct ScrubBoard {
    running: bool,
    verified: u64,
    total: u64,
    /// The last finished pass: (unix seconds, verified, corrupt, missing).
    last: Option<(u64, u64, u64, u64)>,
}

/// How many records one slice of the background scrub verifies before the
/// engine lock is released. Small enough that guest I/O interleaves — a
/// slice is a few dozen milliseconds — large enough that the pass is not
/// all lock churn.
const SCRUB_CHUNK_BLOCKS: usize = 256;
/// The breath between slices: the moment a parked guest operation needs
/// to take the engine first.
const SCRUB_PAUSE: Duration = Duration::from_millis(2);

/// The pass itself, on its own thread from [`Daemon::start_scrub`] to its
/// end: verify in slices, keep the board honest, and finish with the
/// reference walk the one-shot scrub always ran.
fn scrub_loop(shared: &Arc<Shared>) {
    let mut cursor = None;
    let mut verified = 0u64;
    let mut corrupt: Vec<lumen_fs::BlockHash> = Vec::new();
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            shared.scrub.lock().unwrap().running = false;
            return;
        }
        let step =
            shared.with_engine(|engine| engine.pool().scrub_chunk(cursor, SCRUB_CHUNK_BLOCKS));
        match step {
            Ok((count, bad, next)) => {
                verified += count;
                corrupt.extend(bad);
                shared.scrub.lock().unwrap().verified = verified;
                match next {
                    Some(reached) => cursor = Some(reached),
                    None => break,
                }
            }
            Err(err) => {
                eprintln!("scrub abandoned: {err}");
                shared.scrub.lock().unwrap().running = false;
                return;
            }
        }
        std::thread::sleep(SCRUB_PAUSE);
    }
    let report = shared.with_engine(|engine| engine.pool().scrub_references(verified, corrupt));
    let mut board = shared.scrub.lock().unwrap();
    board.running = false;
    match report {
        Ok(report) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            board.last = Some((
                now,
                report.blocks_verified,
                report.corrupt.len() as u64,
                report.missing.len() as u64,
            ));
        }
        Err(err) => eprintln!("scrub reference walk abandoned: {err}"),
    }
}

impl Shared {
    /// Route the engine's pending effects, in the order it emitted them.
    /// Called with the engine lock held — which is what makes the outbound
    /// queue an ordered image of the engine's stream even with several
    /// threads calling in.
    fn drain(&self, engine: &mut ReplNode<FileDisk>) {
        let effects = engine.take_effects();
        if effects.is_empty() {
            return;
        }
        let mut links = self.links.lock().unwrap();
        let mut fl = self.flushes.lock().unwrap();
        let mut rd = self.reads.lock().unwrap();
        for effect in effects {
            match effect {
                // Routed by the addressee the engine named: each peer's
                // writer drains its own queue.
                Effect::Send(to, message) => {
                    let link = links.entry(to).or_default();
                    link.queued_bytes += send_cost(&message);
                    link.queue.push_back(message);
                }
                Effect::FlushDone(ticket) => {
                    fl.outcomes.insert(ticket, true);
                }
                Effect::FlushFailed(ticket) => {
                    fl.outcomes.insert(ticket, false);
                }
                Effect::ReadDone(ticket) => {
                    rd.outcomes.insert(ticket, true);
                }
                Effect::ReadFailed(ticket) => {
                    rd.outcomes.insert(ticket, false);
                }
            }
        }
    }

    fn notify_all(&self) {
        self.out_ready.notify_all();
        self.send_room.notify_all();
        self.flush_ready.notify_all();
        self.read_ready.notify_all();
        self.changed.notify_all();
    }

    /// Hold a guest write until every peer's outbound queue is under the
    /// cap — the flow control the engine's own doc assigns to "the daemon
    /// that owns the socket". Blocking *before* the engine call keeps the
    /// queue an image of what the wire can carry soon, instead of a
    /// mortgage the next fsync forecloses. A session that dies clears its
    /// queue, so a stalled peer converts into suspension (the engine's
    /// business), never a parked write nobody will release.
    fn wait_send_room(&self, cancelled: &AtomicBool) -> Result<(), FsError> {
        let mut links = self.links.lock().unwrap();
        loop {
            if self.shutdown.load(Ordering::SeqCst) || cancelled.load(Ordering::SeqCst) {
                return Err(FsError::Suspended);
            }
            if links
                .values()
                .all(|link| link.queued_bytes <= SEND_QUEUE_BYTES)
            {
                return Ok(());
            }
            links = self.send_room.wait_timeout(links, TICK).unwrap().0;
        }
    }

    /// Run one call against the engine and drain its effects.
    fn with_engine<T>(&self, f: impl FnOnce(&mut ReplNode<FileDisk>) -> T) -> T {
        let mut engine = self.engine.lock().unwrap();
        let out = f(&mut engine);
        self.drain(&mut engine);
        drop(engine);
        self.notify_all();
        out
    }

    /// A session is up: register its socket (replacing only this peer's
    /// old one), open a fresh incarnation (dropping anything queued for a
    /// dead one), and say hello. Returns the incarnation this session
    /// owns — its identity for the writer and for teardown.
    fn session_up(&self, peer: u8, socket: Option<TcpStream>) -> u64 {
        let mut engine = self.engine.lock().unwrap();
        let incarnation = {
            let mut links = self.links.lock().unwrap();
            let link = links.entry(peer).or_default();
            if let Some(old) = link.socket.take() {
                let _ = old.shutdown(Shutdown::Both);
            }
            link.socket = socket;
            link.incarnation += 1;
            link.queue.clear();
            link.queued_bytes = 0;
            link.incarnation
        };
        engine.connect(peer);
        self.drain(&mut engine);
        drop(engine);
        self.notify_all();
        incarnation
    }

    /// The peer a bare (legacy, two-member) fence verdict means: the one
    /// link if exactly one exists, else the two-member convention's other
    /// id.
    fn sole_peer(&self) -> u8 {
        let links = self.links.lock().unwrap();
        let mut keys = links.keys();
        match (keys.next(), keys.next()) {
            (Some(peer), None) => *peer,
            _ => {
                if self.node_id == 0 {
                    1
                } else {
                    0
                }
            }
        }
    }

    /// A session died. Exactly-once semantics ride the incarnation: if it
    /// has already moved on — a replacement session started, or a fence
    /// verdict forced the link down — this teardown is history's and does
    /// nothing, which is what keeps it from knocking a Degraded node back
    /// to Suspended after a verdict already landed.
    fn session_down(&self, peer: u8, my_incarnation: u64) {
        let mut engine = self.engine.lock().unwrap();
        let stale = {
            let mut links = self.links.lock().unwrap();
            let link = links.entry(peer).or_default();
            if link.incarnation != my_incarnation {
                true
            } else {
                link.incarnation += 1;
                link.queue.clear();
                link.queued_bytes = 0;
                link.socket = None;
                false
            }
        };
        if !stale {
            engine.peer_lost(peer);
            self.drain(&mut engine);
        }
        drop(engine);
        self.notify_all();
    }

    /// The cluster's verdict, applied to one member: force its link down
    /// and continue at the agreed era. Fencing a member already fenced is
    /// already done — the verdict layer may retry, and a retry must not
    /// error. `era` is the number the layer above computed from every
    /// survivor's floor; `None` (the two-member legacy verb) uses this
    /// sole survivor's own floor, which at two members is the whole
    /// computation.
    fn fence_member(&self, peer: u8, era: Option<u64>) -> Result<(), FsError> {
        self.kill_link(peer);
        let mut engine = self.engine.lock().unwrap();
        {
            let mut links = self.links.lock().unwrap();
            let link = links.entry(peer).or_default();
            link.incarnation += 1;
            link.queue.clear();
            link.queued_bytes = 0;
        }
        if engine.is_fenced(peer) {
            drop(engine);
            self.notify_all();
            return Ok(());
        }
        engine.peer_lost(peer);
        let era = era.unwrap_or_else(|| engine.era_target());
        let outcome = engine.set_member_fenced(peer, era);
        self.drain(&mut engine);
        drop(engine);
        self.notify_all();
        outcome
    }

    /// Block until the engine changes (or the daemon stops). Returns false
    /// on shutdown.
    fn wait_change(&self) -> bool {
        if self.shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let engine = self.engine.lock().unwrap();
        let _unused = self.changed.wait_timeout(engine, TICK).unwrap();
        !self.shutdown.load(Ordering::SeqCst)
    }

    /// Kill the live peer socket, if any, so its threads notice promptly.
    fn kill_link(&self, peer: u8) {
        let mut links = self.links.lock().unwrap();
        if let Some(link) = links.get_mut(&peer) {
            if let Some(socket) = link.socket.take() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
    }

    fn kill_every_link(&self) {
        let mut links = self.links.lock().unwrap();
        for link in links.values_mut() {
            if let Some(socket) = link.socket.take() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
    }
}

/// The engine reports pressure; the shell makes room and retries, each
/// remedy once — the smoke tool's policy, kept.
fn with_room<T>(
    engine: &mut ReplNode<FileDisk>,
    mut op: impl FnMut(&mut ReplNode<FileDisk>) -> Result<T, FsError>,
) -> Result<T, FsError> {
    let mut checkpointed = false;
    let mut collected = false;
    loop {
        match op(engine) {
            Err(FsError::WalFull) if !checkpointed => {
                checkpointed = true;
                engine.checkpoint()?;
            }
            Err(FsError::Full) if !collected => {
                collected = true;
                checkpointed = true;
                engine.collect_garbage()?;
            }
            other => return other,
        }
    }
}

// ---------------------------------------------------------------------------
// The guest-facing handle: what an export (NBD today, ublk next) calls.

#[derive(Clone)]
pub struct GuestHandle {
    shared: Arc<Shared>,
    /// Set when whatever this handle serves is going away. Clones share
    /// it, so cancelling the handle an export was built from releases the
    /// threads servicing that export — see [`GuestHandle::cancel`].
    cancelled: Arc<AtomicBool>,
}

impl GuestHandle {
    /// Read bytes — local when this member homes the blocks, a fetch and
    /// a retry when it does not. The loop is the engine's contract: read,
    /// hear `BlockElsewhere`, fetch that block, read again. A fetch that
    /// dies with its session retries against whichever home is reachable
    /// then; a fetch with no reachable home surfaces the engine's own
    /// refusal.
    pub fn read(&self, vdisk: u64, offset: u64, len: u64) -> Result<Vec<u8>, FsError> {
        loop {
            let outcome = self
                .shared
                .with_engine(|engine| engine.read_bytes(vdisk, offset, len));
            match outcome {
                Err(FsError::BlockElsewhere { tier, hash }) => self.fetch(tier, hash)?,
                other => return other,
            }
        }
    }

    /// Fetch one block from a home and wait for it to land. Arrived or
    /// failed, the answer is another pass by the caller: a landed payload
    /// serves the retry, a dead session's retry re-fetches from the
    /// co-home.
    fn fetch(&self, tier: u8, hash: lumen_fs::BlockHash) -> Result<(), FsError> {
        let ticket = self
            .shared
            .with_engine(|engine| engine.fetch_block(tier, hash))?;
        let mut reads = self.shared.reads.lock().unwrap();
        loop {
            if reads.outcomes.remove(&ticket).is_some() {
                return Ok(());
            }
            if self.shared.shutdown.load(Ordering::SeqCst) || self.cancelled.load(Ordering::SeqCst)
            {
                return Err(FsError::Suspended);
            }
            reads = self.shared.read_ready.wait_timeout(reads, TICK).unwrap().0;
        }
    }

    /// Stop parking I/O on this handle and every clone of it: the export
    /// it serves is being taken down.
    ///
    /// Suspended I/O waits for a verdict, and a verdict may never come —
    /// that is the whole point of it. So a teardown that waits for parked
    /// requests to finish waits forever: `unexport` on a node whose peer
    /// died unfenced would hang, and hang the control surface with it,
    /// since the teardown runs on the thread answering the operator.
    /// Cancelling turns those requests into errors, which is the honest
    /// answer for a device that is going away — nothing was acknowledged,
    /// so nothing is being taken back.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.shared.notify_all();
    }

    /// A mutation, blocking through suspension: a suspended node holds the
    /// request (DRBD's suspended I/O) rather than erroring the guest, and
    /// completes it when a verdict or a reconciliation says how — or when
    /// the export it belongs to is cancelled out from under it.
    fn blocking<T>(
        &self,
        mut f: impl FnMut(&mut ReplNode<FileDisk>) -> Result<T, FsError>,
    ) -> Result<T, FsError> {
        loop {
            let outcome = self.shared.with_engine(|engine| with_room(engine, &mut f));
            match outcome {
                Err(FsError::Suspended) => {
                    if self.cancelled.load(Ordering::SeqCst) || !self.shared.wait_change() {
                        return Err(FsError::Suspended);
                    }
                }
                other => return other,
            }
        }
    }

    pub fn write(&self, vdisk: u64, offset: u64, data: &[u8]) -> Result<(), FsError> {
        // The brake before anything else: a write accepted while the
        // outbound queues are over the cap would be one more block of
        // debt the next fsync inherits.
        self.shared.wait_send_room(&self.cancelled)?;
        // The whole blocks of this write are hashed HERE, on the export's
        // worker thread, before any engine lock — measured as the largest
        // single cost the lock was carrying. Geometry first, briefly:
        let geometry = self
            .shared
            .with_engine(|engine| -> Result<(u64, u64), FsError> {
                Ok((
                    engine.pool().block_size() as u64,
                    engine.pool().vdisk_size(vdisk)?,
                ))
            });
        let Ok((block_size, size)) = geometry else {
            // No such vdisk (or worse) — the plain path says it properly.
            return self.write_fallback(vdisk, offset, data);
        };
        let end = offset + data.len() as u64;
        if end > size || data.is_empty() {
            // Out of range errors exactly as before, from the engine.
            return self.write_fallback(vdisk, offset, data);
        }
        // [head partial][whole blocks][tail partial] — the whole blocks
        // are one run, prehashed; the edges are read-modify-writes that
        // keep the fetching path.
        let first_whole = offset.div_ceil(block_size);
        let head_len = ((first_whole * block_size).min(end) - offset) as usize;
        let mut items: Vec<(lumen_fs::BlockHash, &[u8])> = Vec::new();
        let mut from = head_len;
        let mut block = first_whole;
        loop {
            let start = block * block_size;
            let logical = block_size.min(size - start) as usize;
            if start >= end || (end - start) < logical as u64 {
                break;
            }
            let payload = &data[from..from + logical];
            items.push((lumen_fs::hash_block(payload), payload));
            from += logical;
            block += 1;
        }
        if items.is_empty() {
            return self.write_fallback(vdisk, offset, data);
        }
        if head_len > 0 {
            self.write_fallback(vdisk, offset, &data[..head_len])?;
        }
        self.blocking(|engine| engine.write_block_run_prehashed(vdisk, first_whole, &items))?;
        if from < data.len() {
            self.write_fallback(vdisk, offset + from as u64, &data[from..])?;
        }
        Ok(())
    }

    /// The unhoisted path: the engine chunks, hashes, and handles the
    /// read-modify-write edges — with the fetch loop those edges need on
    /// a member that does not home the block.
    fn write_fallback(&self, vdisk: u64, offset: u64, data: &[u8]) -> Result<(), FsError> {
        self.shared.wait_send_room(&self.cancelled)?;
        loop {
            match self.blocking(|engine| engine.write_bytes(vdisk, offset, data)) {
                Err(FsError::BlockElsewhere { tier, hash }) => self.fetch(tier, hash)?,
                other => return other,
            }
        }
    }

    pub fn trim(&self, vdisk: u64, offset: u64, len: u64) -> Result<(), FsError> {
        self.blocking(|engine| engine.trim_bytes(vdisk, offset, len))
    }

    /// The durability barrier: returns when every prior write is two-node
    /// durable, or single-node durable under a fence verdict — the
    /// engine's rule, waited on for real.
    pub fn flush(&self) -> Result<(), FsError> {
        let ticket = self.blocking(|engine| engine.flush())?;
        let mut flushes = self.shared.flushes.lock().unwrap();
        loop {
            if let Some(honest) = flushes.outcomes.remove(&ticket) {
                return if honest {
                    Ok(())
                } else {
                    // The writes this flush covered were discarded by an
                    // adoption; the guest hears an error, not a lie.
                    Err(FsError::Corrupt("a flush's writes did not survive"))
                };
            }
            if self.shared.shutdown.load(Ordering::SeqCst) || self.cancelled.load(Ordering::SeqCst)
            {
                return Err(FsError::Suspended);
            }
            flushes = self
                .shared
                .flush_ready
                .wait_timeout(flushes, TICK)
                .unwrap()
                .0;
        }
    }

    /// Take the writer lease. Blocks through suspension; refuses
    /// (`LeaseHeld`) if the peer holds the vdisk in this era.
    pub fn claim_writer(&self, vdisk: u64) -> Result<(), FsError> {
        self.blocking(|engine| engine.claim_writer(vdisk))
    }

    /// Open a vdisk for a guest, and say whether this node may write it.
    ///
    /// The distinction is live migration's whole shape. An ordinary attach
    /// takes the pen — that is what makes a running machine's disk
    /// single-writer. But a migration *destination* must open the disk
    /// while the source is still writing it, so an attach that found a
    /// window open toward this node opens **penless**: reads work, writes
    /// refuse with `NotWriter`, and the pen arrives when the *source*
    /// relinquishes it — one durable step, no instant with two writers. An
    /// attach where the peer holds the lease and no window names us is
    /// somebody else's disk, and is refused.
    pub fn attach(&self, vdisk: u64) -> Result<Attach, FsError> {
        // Existence first: a claim on a vdisk that is not here should say
        // so, not report a lease problem.
        self.vdisk_size(vdisk)?;
        self.blocking(|engine| {
            let node = engine.node();
            let era = engine.pool().era();
            match engine.lease(vdisk) {
                // A window from the era we are actually in. A window from
                // a retired era binds nothing — the survivor moved on.
                Some(lease) if lease.era >= era && lease.handing_to == Some(node) => {
                    Ok(Attach::Penless)
                }
                _ => {
                    engine.claim_writer(vdisk)?;
                    Ok(Attach::Writer)
                }
            }
        })
    }

    /// Open the migration window toward `to`. The guest keeps running and
    /// keeps writing here; the destination may open the disk alongside.
    pub fn begin_handover(&self, vdisk: u64, to: u8) -> Result<(), FsError> {
        self.blocking(|engine| engine.begin_handover(vdisk, to))
    }

    /// Hand the pen to the destination — the instant this node stops being
    /// the writer. Runs on the **source**, once its guest has stopped.
    pub fn relinquish(&self, vdisk: u64, to: u8) -> Result<(), FsError> {
        self.blocking(|engine| engine.relinquish(vdisk, to))
    }

    /// Ask whether the pen has arrived: the destination's half, and a
    /// question rather than an act. Refuses until the source has handed it
    /// over, which is what a caller waits on.
    pub fn accept_handover(&self, vdisk: u64) -> Result<(), FsError> {
        self.blocking(|engine| engine.accept_handover(vdisk))
    }

    /// The migration failed; the window closes and the disk stays put.
    /// Runs on the source, which never stopped being the writer.
    pub fn abort_handover(&self, vdisk: u64) -> Result<(), FsError> {
        self.blocking(|engine| engine.abort_handover(vdisk))
    }

    pub fn lease(&self, vdisk: u64) -> Option<Lease> {
        self.shared.with_engine(|engine| engine.lease(vdisk))
    }

    /// Create a vdisk. Replicated like any other operation, so it exists
    /// on both members or the call refuses.
    pub fn create_vdisk(&self, vdisk: u64, size_bytes: u64, tier: u8) -> Result<(), FsError> {
        self.blocking(|engine| engine.create_vdisk(vdisk, size_bytes, tier))
    }

    /// Destroy a vdisk. Refused while snapshots pin its history — the
    /// engine's rule, surfaced rather than worked around.
    pub fn delete_vdisk(&self, vdisk: u64) -> Result<(), FsError> {
        self.blocking(|engine| engine.delete_vdisk(vdisk))
    }

    pub fn vdisks(&self) -> Vec<(u64, u64)> {
        self.shared.with_engine(|engine| engine.pool().vdisks())
    }

    /// Take a snapshot: a retained map root, instant and crash-consistent.
    /// Replicated like every other operation, so it exists on both members
    /// or the call refuses — which is what keeps a snapshot from vanishing
    /// the moment the member that took it is the one that dies.
    pub fn snapshot_vdisk(&self, vdisk: u64, snapshot: u64) -> Result<(), FsError> {
        self.blocking(|engine| engine.snapshot_vdisk(vdisk, snapshot))
    }

    /// Drop a snapshot, releasing whatever it alone was pinning.
    pub fn delete_snapshot(&self, vdisk: u64, snapshot: u64) -> Result<(), FsError> {
        self.blocking(|engine| engine.delete_snapshot(vdisk, snapshot))
    }

    /// Put a vdisk back to a snapshot, discarding everything written since.
    ///
    /// The engine does exactly what it is told here — it swaps the map root
    /// and does not ask whether anything is using the disk. **The refusal
    /// while the disk is open lives above this**, in
    /// [`Daemon::rollback_vdisk`] for this member and in the orchestration
    /// layer for the pool, because only they know about exports.
    pub fn rollback_vdisk(&self, vdisk: u64, snapshot: u64) -> Result<(), FsError> {
        self.blocking(|engine| engine.rollback_vdisk(vdisk, snapshot))
    }

    /// Every snapshot the pool holds, as `(vdisk, snapshot, size_bytes)`.
    /// Replicated state, so any member answers the same.
    pub fn snapshots(&self) -> Vec<(u64, u64, u64)> {
        self.shared.with_engine(|engine| engine.pool().snapshots())
    }

    pub fn vdisk_size(&self, vdisk: u64) -> Result<u64, FsError> {
        self.shared.with_engine(|engine| engine.vdisk_size(vdisk))
    }

    /// The pool's block size — what exports report as optimal I/O.
    pub fn block_size(&self) -> u32 {
        self.shared.with_engine(|engine| engine.pool().block_size())
    }
}

// ---------------------------------------------------------------------------
// The peer link.

fn read_frame(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

/// How long a silent peer link is given before the kernel starts probing
/// it, how often it probes, and how many unanswered probes kill it — so a
/// session whose far end vanished dies in about fifteen seconds.
///
/// **This is what makes a power cut look different from a quiet pool.** A
/// peer that loses power sends no FIN: the survivor's socket stays open
/// forever, the daemon believes the session is live, and it never redials
/// — so the returning node sits Suspended waiting for a connection nobody
/// is going to make, while the survivor reports Synced. Observed exactly
/// that way on real hardware; the cure was restarting the daemon that
/// still thought it was connected, which is not a thing an appliance
/// should need an operator for.
///
/// Probing does not replace the fence verdict. It ends a dead *socket*
/// promptly; what that means for the data is still the engine's decision.
const KEEPALIVE_IDLE_SECS: libc::c_int = 5;
const KEEPALIVE_INTERVAL_SECS: libc::c_int = 2;
const KEEPALIVE_PROBES: libc::c_int = 5;

/// Ask the kernel to notice a peer that stopped answering.
///
/// Best-effort by design: a kernel that refuses one of these options
/// leaves the link exactly as it was before, which is the behaviour that
/// shipped for a year. There is nothing to report and nothing to fail.
fn keepalive(stream: &TcpStream) {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int, value: libc::c_int| {
        // SAFETY: setsockopt against a socket we own, with a c_int option
        // value of the length the option expects.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                &value as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, KEEPALIVE_IDLE_SECS);
    set(
        libc::IPPROTO_TCP,
        libc::TCP_KEEPINTVL,
        KEEPALIVE_INTERVAL_SECS,
    );
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, KEEPALIVE_PROBES);
    // A send that cannot be delivered must not park forever either: with
    // this the write half gives up on the same order of timescale as the
    // probes, so a wedged link cannot hold the writer thread indefinitely.
    set(
        libc::IPPROTO_TCP,
        libc::TCP_USER_TIMEOUT,
        (KEEPALIVE_IDLE_SECS + KEEPALIVE_INTERVAL_SECS * KEEPALIVE_PROBES) * 1000,
    );
}

/// One session, run to its death on the calling thread. The runner thread
/// runs sessions strictly one after another, which is what makes teardown
/// and the next session's hello naturally ordered.
fn run_session(shared: &Arc<Shared>, mut stream: TcpStream) {
    stream.set_nodelay(true).ok();
    keepalive(&stream);

    // Handshake first, engine second: a wrong pool or a self-connection
    // must die before the engine hears anything.
    let ours = Handshake {
        pool_uuid: shared.with_engine(|engine| engine.pool().pool_uuid()),
        node: shared.node_id,
    };
    if stream
        .write_all(&ours.encode())
        .and_then(|_| stream.flush())
        .is_err()
    {
        return;
    }
    let mut theirs = [0u8; HANDSHAKE_LEN];
    if stream.read_exact(&mut theirs).is_err() {
        return;
    }
    let theirs = match Handshake::decode(&theirs) {
        Ok(shake) => shake,
        Err(err) => {
            eprintln!("peer handshake refused: {err}");
            return;
        }
    };
    if theirs.pool_uuid != ours.pool_uuid {
        eprintln!("peer handshake refused: different pool");
        return;
    }
    if theirs.node == ours.node {
        eprintln!("peer handshake refused: same node id");
        return;
    }

    // Register the socket under this peer — replacing only this peer's
    // old session, never another's — then open the session in the engine.
    let peer_node = theirs.node;
    let incarnation = shared.session_up(peer_node, stream.try_clone().ok());
    eprintln!("peer session {incarnation} up (node {peer_node})");

    let writer = {
        let shared = Arc::clone(shared);
        let stream = stream.try_clone();
        std::thread::spawn(move || {
            let Ok(mut stream) = stream else { return };
            writer_loop(&shared, peer_node, &mut stream, incarnation);
        })
    };

    // Ingest is a two-stage pipeline: this thread receives, decodes, and
    // hashes; the applier owns the engine calls. One thread doing all
    // four was measured as the pool's whole write ceiling — a single
    // saturated core on the receiving node while the wire, the disks,
    // and every other core idled. The channel is bounded so a slow
    // engine still reaches the sender as backpressure — channel full,
    // reader stops reading, TCP window closes — the exact shape the
    // one-thread loop had.
    enum Ingest {
        /// A `Payloads` frame kept whole: the blocks are `(tier, hash,
        /// range into the frame)` — hashed by the reader, stored by the
        /// applier straight out of the frame buffer. The `Vec` per
        /// payload this used to carry was a measured copy tax.
        Payloads {
            frame: Vec<u8>,
            blocks: Vec<(u8, lumen_fs::BlockHash, std::ops::Range<usize>)>,
        },
        Message(PeerMessage),
    }
    let (work_tx, work_rx) = std::sync::mpsc::sync_channel::<Ingest>(INGEST_DEPTH);
    let applier = {
        let shared = Arc::clone(shared);
        let stream = stream.try_clone();
        std::thread::spawn(move || {
            // recv drains what the reader queued before reporting the
            // senders gone — nothing decoded is dropped by a clean EOF.
            while let Ok(work) = work_rx.recv() {
                let outcome = match work {
                    Ingest::Payloads { frame, blocks } => {
                        let refs: Vec<(u8, lumen_fs::BlockHash, &[u8])> = blocks
                            .iter()
                            .map(|(tier, hash, range)| (*tier, *hash, &frame[range.clone()]))
                            .collect();
                        shared.with_engine(|engine| engine.payloads_prehashed_ref(peer_node, &refs))
                    }
                    Ingest::Message(message) => {
                        shared.with_engine(|engine| engine.handle(peer_node, message))
                    }
                };
                if let Err(err) = outcome {
                    // A protocol violation or an engine refusal. Dropping
                    // the session is always safe: reconnect runs a resync,
                    // and the acknowledgement rule means both nodes hold
                    // every acknowledged write however the reconciliation
                    // resolves. Shutting the socket is how the reader
                    // hears it.
                    eprintln!("peer message refused by the engine: {err}");
                    if let Ok(stream) = &stream {
                        let _ = stream.shutdown(Shutdown::Both);
                    }
                    return;
                }
            }
        })
    };

    while let Ok(payload) = read_frame(&mut stream) {
        // Hashing lives here, overlapped with the applier's engine time:
        // the address still comes from the bytes on this node — never
        // the wire. A Payloads frame is kept whole and its blocks named
        // by range, so the bytes decoded from the socket are the bytes
        // the brick writes — no copy between.
        let work = match wire::decode_payload_ranges(&payload) {
            Ok(Some(ranges)) => {
                let blocks = ranges
                    .into_iter()
                    .map(|(tier, range)| {
                        let hash = lumen_fs::hash_block(&payload[range.clone()]);
                        (tier, hash, range)
                    })
                    .collect();
                Ingest::Payloads {
                    frame: payload,
                    blocks,
                }
            }
            Ok(None) => match wire::decode(&payload) {
                Ok(message) => Ingest::Message(message),
                Err(err) => {
                    eprintln!("peer sent an undecodable frame: {err}");
                    break;
                }
            },
            Err(err) => {
                eprintln!("peer sent an undecodable frame: {err}");
                break;
            }
        };
        // A dead applier already shut the socket and said why.
        if work_tx.send(work).is_err() {
            break;
        }
    }

    drop(work_tx);
    let _ = applier.join();
    let _ = stream.shutdown(Shutdown::Both);
    shared.session_down(peer_node, incarnation);
    eprintln!("peer session {incarnation} down");
    let _ = writer.join();
}

/// How much of the queue one send gathers, at most. A 1 MiB guest write
/// queues ~128 messages; sending them one frame per syscall on a NODELAY
/// socket made every block two packets. Bounded so a deep queue cannot
/// hold megabytes twice over (queue and send buffer both).
const SEND_BATCH_BYTES: usize = 4 << 20;

/// The ingest pipeline's depth, in decoded messages. Bounds the memory a
/// slow applier can pin at depth × MAX_FRAME; deep enough that the
/// reader's decode-and-hash keeps overlapping the applier's engine time.
const INGEST_DEPTH: usize = 8;

fn writer_loop(shared: &Arc<Shared>, peer: u8, stream: &mut TcpStream, incarnation: u64) {
    let mut messages: Vec<PeerMessage> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    let mut chunks: Vec<wire::Chunk> = Vec::new();
    loop {
        messages.clear();
        {
            let mut links = shared.links.lock().unwrap();
            loop {
                let link = links.entry(peer).or_default();
                if shared.shutdown.load(Ordering::SeqCst) || link.incarnation != incarnation {
                    return;
                }
                // Everything queued goes in one send, frames intact and in
                // order — the wire format is unchanged, only the syscall
                // boundaries move.
                let mut batched = 0usize;
                while batched < SEND_BATCH_BYTES {
                    let Some(message) = link.queue.pop_front() else {
                        break;
                    };
                    let cost = send_cost(&message);
                    link.queued_bytes = link.queued_bytes.saturating_sub(cost);
                    batched += cost;
                    messages.push(message);
                }
                if !messages.is_empty() {
                    break;
                }
                links = shared.out_ready.wait_timeout(links, TICK).unwrap().0;
            }
        };
        // The queue shrank; a guest write parked on the cap may proceed.
        shared.send_room.notify_all();
        // Scattered encode: framing and headers into the reused scratch
        // buffer, payload bytes named in place — the iovecs below are
        // what an encode copy and a batch copy per payload used to be.
        scratch.clear();
        chunks.clear();
        for (index, message) in messages.iter().enumerate() {
            wire::encode_frame_scattered(index, message, &mut scratch, &mut chunks);
        }
        let mut iovecs: Vec<std::io::IoSlice> = chunks
            .iter()
            .map(|chunk| match chunk {
                wire::Chunk::Scratch { start, end } => {
                    std::io::IoSlice::new(&scratch[*start..*end])
                }
                wire::Chunk::Payload { message, index } => std::io::IoSlice::new(
                    &wire::payload_list(&messages[*message]).expect("chunk names a payload")
                        [*index]
                        .1,
                ),
            })
            .collect();
        if write_all_vectored(stream, &mut iovecs)
            .and_then(|_| stream.flush())
            .is_err()
        {
            // The reader will hit the same corpse and run the teardown.
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
    }
}

/// `write_all` over a scattered batch: every byte of every slice, in
/// order, however many syscalls the socket wants to take it in.
fn write_all_vectored(
    stream: &mut TcpStream,
    mut iovecs: &mut [std::io::IoSlice<'_>],
) -> std::io::Result<()> {
    // Trailing empty slices would read as "nothing left" mid-loop;
    // advance_slices consumes empties as it goes, and a wholly empty
    // batch never reaches the wire.
    while iovecs.iter().any(|slice| !slice.is_empty()) {
        match stream.write_vectored(iovecs) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(written) => std::io::IoSlice::advance_slices(&mut iovecs, written),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The daemon.

/// A finished scrub pass: unix seconds it ended, records verified, found
/// corrupt, and found missing.
pub type ScrubOutcome = (u64, u64, u64, u64);

pub struct Daemon {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    /// Where the peer listener actually bound, for `listen` on port 0.
    peer_addr: Option<SocketAddr>,
    /// Live guest exports by vdisk. The daemon owns these so that stopping
    /// is possible at all: a block device that outlives the process
    /// serving it is a device whose next reader hangs.
    exports: Mutex<HashMap<u64, Box<dyn ExportControl>>>,
    /// Which path produced which brick — the join `brick-list` makes
    /// between the engine's uuids and the operator's device names. Fixed
    /// for the daemon's life, like the set itself.
    brick_paths: HashMap<[u8; 16], PathBuf>,
}

impl Daemon {
    pub fn start(config: Config) -> Result<Daemon, String> {
        if config.listen.is_none() && config.dials.is_empty() {
            return Err(
                "a pool daemon needs at least one peer endpoint: listen, dial, or both".into(),
            );
        }
        if config.bricks.is_empty() {
            return Err("a daemon with no bricks has nothing to serve".into());
        }
        let mut bricks = Vec::with_capacity(config.bricks.len());
        let mut brick_paths = HashMap::new();
        for path in &config.bricks {
            // The control surface reports these paths space-separated, so
            // a path with whitespace would corrupt every listing it ever
            // appeared in. Refused where it enters, not escaped downstream.
            if path.to_string_lossy().chars().any(char::is_whitespace) {
                return Err(format!(
                    "brick path contains whitespace: {}",
                    path.display()
                ));
            }
            let disk = FileDisk::open(path)
                .map_err(|err| format!("cannot open {}: {err}", path.display()))?;
            let brick = Brick::open(disk).map_err(|err| format!("{}: {err}", path.display()))?;
            brick_paths.insert(brick.roster_entry().brick_uuid, path.clone());
            bricks.push(brick);
        }
        let set = BrickSet::from_bricks(bricks).map_err(|err| err.to_string())?;
        let pool = if config.members.is_empty() {
            Pool::open_set(set).map_err(|err| err.to_string())?
        } else {
            // Placed at open — WAL replay's home-awareness needs the seat
            // before the first entry replays. The derived first map is
            // only a seed: an anchored (reassigned) map in the manifest
            // wins over it.
            let map = lumen_fs::SliceMap::for_members(1, &config.members)
                .map_err(|err| err.to_string())?;
            Pool::open_set_placed(set, config.node, map).map_err(|err| err.to_string())?
        };
        let engine = ReplNode::new(pool, config.node);

        let shared = Arc::new(Shared {
            node_id: config.node,
            engine: Mutex::new(engine),
            changed: Condvar::new(),
            links: Mutex::new(HashMap::new()),
            out_ready: Condvar::new(),
            send_room: Condvar::new(),
            flushes: Mutex::new(Flushes::default()),
            flush_ready: Condvar::new(),
            reads: Mutex::new(Reads::default()),
            read_ready: Condvar::new(),
            scrub: Mutex::new(ScrubBoard::default()),
            shutdown: AtomicBool::new(false),
        });

        let mut threads = Vec::new();
        let mut peer_addr = None;

        if let Some(listen) = config.listen {
            let listener =
                TcpListener::bind(listen).map_err(|err| format!("cannot bind {listen}: {err}"))?;
            peer_addr = Some(listener.local_addr().map_err(|err| err.to_string())?);

            // Accept thread: each connection gets its own session thread —
            // the mesh runs one live session per peer, concurrently, and
            // which existing session a newcomer replaces is decided by the
            // handshake's node id inside run_session, never here. The
            // accept is a nonblocking poll: a thread parked inside
            // accept() can only be woken by a connection, and "make a
            // connection to wake it" is exactly the kind of one-shot
            // signal a full backlog silently eats — this hung a shutdown
            // for real before it became a poll.
            listener
                .set_nonblocking(true)
                .map_err(|err| err.to_string())?;
            let accept_shared = Arc::clone(&shared);
            threads.push(std::thread::spawn(move || loop {
                if accept_shared.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                let (stream, _) = match listener.accept() {
                    Ok(accepted) => accepted,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(TICK);
                        continue;
                    }
                    Err(_) => continue,
                };
                // Sessions run blocking I/O; on some platforms an accepted
                // socket inherits the listener's nonblocking mode.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                // Session threads die with their sockets; shutdown kills
                // every registered socket, so none outlives the daemon.
                let session_shared = Arc::clone(&accept_shared);
                std::thread::spawn(move || run_session(&session_shared, stream));
            }));
        }

        for dial in config.dials {
            let dial_shared = Arc::clone(&shared);
            threads.push(std::thread::spawn(move || loop {
                if dial_shared.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(stream) = TcpStream::connect(dial) {
                    run_session(&dial_shared, stream);
                }
                // The pause covers every ending — connect refused, session
                // died, or handshake rejected. Without it a dialer whose
                // handshake is being *refused* (that is not a failed
                // connect) redials in a tight loop and floods the peer's
                // accept backlog.
                std::thread::sleep(REDIAL);
            }));
        }

        // Maintenance: periodic checkpoints bound WAL replay after a
        // crash; the space check collects on the way down rather than at
        // the bottom — the lesson the burn-in taught the smoke tool.
        let maint_shared = Arc::clone(&shared);
        threads.push(std::thread::spawn(move || {
            let mut ticks = 0u32;
            loop {
                std::thread::sleep(MAINTENANCE_TICK);
                if maint_shared.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                ticks += 1;
                let outcome = maint_shared.with_engine(|engine| {
                    // A pending reassignment is driven from here: each
                    // tick re-requests whatever moves are still owed —
                    // idempotent, so a lost batch just goes again.
                    if engine.reassign_pending().is_some() {
                        engine.step_reassign()?;
                    }
                    let space = engine.pool().space();
                    if space.segments_free < space.segments_total / 4 {
                        engine.collect_garbage().map(|_| None)
                    } else if ticks.is_multiple_of(CHECKPOINT_TICKS) {
                        // Two-phase: the buffered half here, the drain
                        // below with the lock free. The measured cost of
                        // the old way was the engine frozen up to a
                        // second per checkpoint while dirty writeback hit
                        // the platters — every guest write and peer
                        // apply queued behind it.
                        match engine.checkpoint_begin()? {
                            Some(ticket) => Ok(Some(ticket)),
                            // No detached barriers (a simulated disk):
                            // single-phase, whose folds the begin above
                            // already paid for.
                            None => engine.checkpoint().map(|_| None),
                        }
                    } else {
                        Ok(None)
                    }
                });
                let ticket = match outcome {
                    Ok(ticket) => ticket,
                    Err(err) => {
                        eprintln!("maintenance failed: {err}");
                        continue;
                    }
                };
                let Some(mut ticket) = ticket else { continue };
                // The drain, off the lock: guest writes and peer applies
                // flow while the platters catch up. Concurrent for the
                // same reason BrickSet::flush is — the slowest brick,
                // not the sum.
                let flushers = std::mem::take(&mut ticket.flushers);
                let mut drained = true;
                std::thread::scope(|scope| {
                    let handles: Vec<_> = flushers
                        .into_iter()
                        .map(|flush| scope.spawn(flush))
                        .collect();
                    for handle in handles {
                        if handle.join().expect("a checkpoint drain panicked").is_err() {
                            drained = false;
                        }
                    }
                });
                if !drained {
                    // Dropped, not committed: an anchor over an undrained
                    // snapshot would promise a durability nobody
                    // delivered. The bricks stay dirty; the next tick
                    // retries and surfaces a persistent error in-lock.
                    eprintln!("checkpoint drain failed; ticket dropped");
                    continue;
                }
                if let Err(err) =
                    maint_shared.with_engine(|engine| engine.checkpoint_commit(ticket))
                {
                    eprintln!("checkpoint commit failed: {err}");
                }
            }
        }));

        Ok(Daemon {
            shared,
            threads,
            peer_addr,
            exports: Mutex::new(HashMap::new()),
            brick_paths,
        })
    }

    /// Serve `vdisk` as a guest block device. One export per vdisk: a
    /// second would be a second writer wearing a different device name,
    /// which is the thing every layer under this exists to prevent.
    pub fn export(&self, vdisk: u64, dev_id: u32) -> Result<String, String> {
        let mut exports = self.exports.lock().unwrap();
        if exports.contains_key(&vdisk) {
            return Err(format!("vdisk {vdisk} is already exported"));
        }
        let export = self.start_export(vdisk, dev_id)?;
        let describe = export.describe();
        exports.insert(vdisk, export);
        Ok(describe)
    }

    #[cfg(target_os = "linux")]
    fn start_export(&self, vdisk: u64, dev_id: u32) -> Result<Box<dyn ExportControl>, String> {
        crate::export::ublk::start(self.guest(), vdisk, dev_id)
    }

    #[cfg(not(target_os = "linux"))]
    fn start_export(&self, _vdisk: u64, _dev_id: u32) -> Result<Box<dyn ExportControl>, String> {
        Err("the ublk export needs a Linux kernel".into())
    }

    /// Take an export down. The lease is deliberately *not* released: who
    /// may write outlives who is currently serving, which is what lets a
    /// node be re-exported after a daemon restart without a negotiation.
    pub fn unexport(&self, vdisk: u64) -> Result<(), String> {
        let export = self
            .exports
            .lock()
            .unwrap()
            .remove(&vdisk)
            .ok_or_else(|| format!("vdisk {vdisk} is not exported"))?;
        export.stop()
    }

    /// Roll a vdisk back to a snapshot — **refused while this member is
    /// serving it**.
    ///
    /// The engine will swap the map root whenever it is asked; it has no
    /// idea a guest is mounted on the other side of a ublk device. Rolling
    /// back underneath a running machine would replace every block beneath
    /// a live filesystem, and the guest would not be told. So the export
    /// map — which only the daemon has — is what turns the engine's
    /// unconditional operation into the contract the console promises.
    ///
    /// This covers *this* member. A pool has more than one, and the
    /// orchestration layer refuses if any member is serving the disk; the
    /// check lives in both places on purpose, because the daemon must not
    /// depend on being called politely.
    ///
    /// One honest limit: it sees the exports it owns, which is every ublk
    /// device. The `--nbd` bootstrap export is served on its own thread and
    /// was never in this map, so it is not covered — that path is a
    /// diagnostic one the shipped unit does not use, and making it a
    /// first-class export is a change to the bootstrap posture rather than
    /// something to do quietly here.
    pub fn rollback_vdisk(&self, vdisk: u64, snapshot: u64) -> Result<(), String> {
        if self.exports.lock().unwrap().contains_key(&vdisk) {
            return Err(format!(
                "vdisk {vdisk} is being served here — stop the machine using it before rolling back"
            ));
        }
        self.guest()
            .rollback_vdisk(vdisk, snapshot)
            .map_err(|err| err.to_string())
    }

    /// Every live export, `(vdisk, description)`, sorted.
    pub fn exports(&self) -> Vec<(u64, String)> {
        let exports = self.exports.lock().unwrap();
        let mut all: Vec<(u64, String)> = exports
            .iter()
            .map(|(vdisk, export)| (*vdisk, export.describe()))
            .collect();
        all.sort_by_key(|(vdisk, _)| *vdisk);
        all
    }

    /// Stop every export. Runs before the threads join at shutdown, and
    /// on its own when an operator wants the guests off this node.
    fn stop_all_exports(&self) {
        let taken: Vec<(u64, Box<dyn ExportControl>)> =
            self.exports.lock().unwrap().drain().collect();
        for (vdisk, export) in taken {
            if let Err(err) = export.stop() {
                eprintln!("stopping the export of vdisk {vdisk} failed: {err}");
            }
        }
    }

    /// Where the peer listener bound — what the other node dials.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }

    /// A fresh guest handle, with its own cancellation switch — so
    /// cancelling one export's handle never releases another's parked I/O.
    pub fn guest(&self) -> GuestHandle {
        GuestHandle {
            shared: Arc::clone(&self.shared),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The cluster's verdict, delivered over the control surface. The
    /// bare form is the two-member legacy: the sole peer, the local floor.
    pub fn fence_peer(&self) -> Result<(), FsError> {
        let peer = self.shared.sole_peer();
        self.shared.fence_member(peer, None)
    }

    /// The mesh form: the member and the era the verdict layer agreed.
    pub fn fence_member(&self, peer: u8, era: u64) -> Result<(), FsError> {
        self.shared.fence_member(peer, Some(era))
    }

    /// Open a reassignment; the maintenance loop drives the moves.
    pub fn reassign(&self, version: u64, members: &[u8]) -> Result<(), FsError> {
        self.shared
            .with_engine(|engine| engine.prepare_reassign(version, members))
    }

    /// `(pending version, blocks still owed)` — `None` when no
    /// reassignment is open. Asking also nudges the moves along.
    pub fn reassign_status(&self) -> Result<Option<(u64, usize)>, FsError> {
        self.shared
            .with_engine(|engine| match engine.reassign_pending() {
                Some(version) => {
                    let owed = engine.step_reassign()?;
                    Ok(Some((version, owed)))
                }
                None => Ok(None),
            })
    }

    /// Commit — refused while blocks are owed; the caller confirms every
    /// member first.
    pub fn commit_reassign(&self) -> Result<(), FsError> {
        self.shared.with_engine(|engine| engine.commit_reassign())
    }

    pub fn status(&self) -> Status {
        // The board first, on its own: the scrub thread takes these locks
        // in the other order (engine, released, then board), so holding
        // both here would be asking for the inversion.
        let scrub = {
            let board = self.shared.scrub.lock().unwrap();
            board.running.then_some((board.verified, board.total))
        };
        self.shared.with_engine(|engine| Status {
            scrub,
            node: engine.node(),
            state: engine.state(),
            era: engine.pool().era(),
            era_target: engine.era_target(),
            map_version: engine.pool().placement().map(|(_, map)| map.version()),
            seats: engine
                .pool()
                .placement()
                .map(|(node, map)| map.seats(node) as u64),
            pool_uuid: engine.pool().pool_uuid(),
            reassign_pending: engine.reassign_pending(),
            accepts_writes: engine.accepts_writes(),
            vdisks: engine.pool().vdisks(),
            leases: engine.pool().leases(),
            space: engine.pool().space(),
            report: engine.pool().space_report(),
            stream: engine.stream_counters(),
            peers: engine.peer_counters(),
        })
    }

    /// The path each brick was opened from, by brick uuid — what turns the
    /// engine's identities into the device names an operator recognizes.
    pub fn brick_path(&self, brick_uuid: &[u8; 16]) -> Option<&Path> {
        self.brick_paths.get(brick_uuid).map(PathBuf::as_path)
    }

    pub fn checkpoint(&self) -> Result<(), FsError> {
        self.shared.with_engine(|engine| engine.checkpoint())
    }

    pub fn collect_garbage(&self) -> Result<GcStats, FsError> {
        self.shared.with_engine(|engine| engine.collect_garbage())
    }

    /// Start a background scrub, or say why not. Returns how many records
    /// the pass will verify — the denominator the progress reads against.
    ///
    /// Background rather than inline because a scrub is minutes of reads
    /// on a real brick, and the old synchronous verb held the engine for
    /// all of them: every guest write parked behind an integrity check
    /// nobody was waiting on. The thread takes the engine one slice at a
    /// time instead ([`SCRUB_CHUNK_BLOCKS`]), so the pass shares the pool
    /// with the guests it exists to protect.
    pub fn start_scrub(&self) -> std::result::Result<u64, String> {
        let total = self
            .shared
            .with_engine(|engine| engine.pool().scrub_total());
        {
            let mut board = self.shared.scrub.lock().unwrap();
            if board.running {
                return Err(format!(
                    "a scrub is already running ({} of {} records verified)",
                    board.verified, board.total
                ));
            }
            board.running = true;
            board.verified = 0;
            board.total = total;
        }
        let shared = Arc::clone(&self.shared);
        std::thread::spawn(move || scrub_loop(&shared));
        Ok(total)
    }

    /// The scrub board, verbatim: running, progress, and the last finished
    /// pass.
    pub fn scrub_progress(&self) -> (bool, u64, u64, Option<ScrubOutcome>) {
        let board = self.shared.scrub.lock().unwrap();
        (board.running, board.verified, board.total, board.last)
    }

    /// Stop everything, settle the pool, release the brick. Exports go
    /// first and while the engine is still answering: a guest device
    /// whose server has already stopped is a device that hangs its
    /// reader, and tearing it down needs the very I/O path being closed.
    pub fn shutdown(self) {
        self.stop_all_exports();
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.kill_every_link();
        self.shared.notify_all();
        for thread in self.threads {
            let _ = thread.join();
        }
        // Settle: a clean stop should not leave the next open replaying a
        // long WAL. Crash safety never depends on this.
        let _ = self.shared.with_engine(|engine| engine.checkpoint());
    }
}

// ---------------------------------------------------------------------------
// Formatting — the shell's impure act: identity comes from the caller.

/// Format a brick for daemon use. `pool_uuid` must be shared by every
/// member of the pool (the handshake enforces it); `brick_uuid` must be
/// unique per brick.
///
/// A node's bricks are formatted one invocation each, **holder last**:
/// `roster` names the already-formatted non-holders (each printed its
/// identity when it was formatted), the holder adds itself, and the
/// anchor it writes rosters the whole set — so a crash anywhere in a
/// node's format leaves no anchor naming bricks that do not exist, just
/// orphans the retry reformats. `create_bytes` creates and sizes a
/// regular file first (the test and smoke path); a block device is always
/// formatted in place.
/// `bootstrap_vdisk_bytes` keeps the smoke tool's bootstrap vdisk, holder
/// only — a wizard-created pool has no place for it. The vdisk is created
/// unclaimed — the first attach claims the writer lease through
/// replication, not through the format.
#[allow(clippy::too_many_arguments)]
pub fn format_brick(
    path: &Path,
    create_bytes: Option<u64>,
    tier: u8,
    wal_holder: bool,
    roster: Vec<lumen_fs::RosterEntry>,
    bootstrap_vdisk_bytes: Option<u64>,
    pool_uuid: [u8; 16],
    brick_uuid: [u8; 16],
) -> Result<(), String> {
    if let Some(bytes) = create_bytes {
        if is_block_device(path) {
            return Err(format!(
                "{} is a block device; --bytes is for files",
                path.display()
            ));
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|err| format!("cannot create {}: {err}", path.display()))?;
        file.set_len(bytes)
            .map_err(|err| format!("cannot size {}: {err}", path.display()))?;
    }
    let disk = FileDisk::open(path).map_err(|err| err.to_string())?;
    let disk_bytes = disk.size();
    let big = disk_bytes >= 1 << 30;
    let params = BrickParams {
        pool_uuid,
        brick_uuid,
        block_size: 16 * 1024,
        segment_size: if big { 64 << 20 } else { 4 << 20 },
        wal_size: if wal_holder {
            if big {
                64 << 20
            } else {
                8 << 20
            }
        } else {
            0
        },
        tier,
        wal_holder,
    };
    let brick = if wal_holder {
        let mut full = roster;
        full.push(lumen_fs::RosterEntry { brick_uuid, tier });
        Brick::format_rostered(disk, params, full).map_err(|err| err.to_string())?
    } else {
        if !roster.is_empty() {
            return Err("only the WAL holder's format carries the roster".into());
        }
        Brick::format(disk, params).map_err(|err| err.to_string())?
    };
    if let Some(vdisk_bytes) = bootstrap_vdisk_bytes {
        if !wal_holder {
            return Err("the bootstrap vdisk needs the WAL holder".into());
        }
        let mut pool = Pool::create(brick).map_err(|err| err.to_string())?;
        pool.create_vdisk(crate::export::nbd::VDISK, vdisk_bytes, tier)
            .map_err(|err| err.to_string())?;
        pool.checkpoint().map_err(|err| err.to_string())?;
    }
    Ok(())
}
