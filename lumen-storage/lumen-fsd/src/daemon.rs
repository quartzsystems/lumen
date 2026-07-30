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
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lumen_fs::file_disk::{is_block_device, FileDisk};
use lumen_fs::{
    Brick, BrickParams, BrickStats, ByteView, Disk, Effect, FsError, GcStats, Lease, PeerMessage,
    Pool, ReplNode, ReplState, ScrubReport,
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
    pub brick: PathBuf,
    /// Accept the peer here. Exactly one of `listen`/`dial` is set — the
    /// two ends of one link, decided by deployment rather than guessed.
    pub listen: Option<SocketAddr>,
    /// Dial the peer there.
    pub dial: Option<SocketAddr>,
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
}

struct Outbound {
    queue: VecDeque<PeerMessage>,
    /// The session generation. Bumped at every session start, every
    /// session death, and every forced link reset — and checked by the
    /// writer and by teardown, so nothing written for one session can act
    /// on another.
    incarnation: u64,
}

#[derive(Default)]
struct Flushes {
    /// Ticket → completed honestly. Entries are removed by the waiter.
    outcomes: HashMap<u64, bool>,
}

struct Shared {
    node_id: u8,
    engine: Mutex<ReplNode<FileDisk>>,
    /// Notified after every engine call — the wakeable "something changed"
    /// every blocked guest operation waits on. Paired with `engine`.
    changed: Condvar,
    outbound: Mutex<Outbound>,
    out_ready: Condvar,
    flushes: Mutex<Flushes>,
    flush_ready: Condvar,
    /// The live peer socket, if any — held so a verdict or a replacement
    /// connection can kill it from outside its own threads.
    live_socket: Mutex<Option<TcpStream>>,
    shutdown: AtomicBool,
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
        let mut ob = self.outbound.lock().unwrap();
        let mut fl = self.flushes.lock().unwrap();
        for effect in effects {
            match effect {
                Effect::Send(message) => ob.queue.push_back(message),
                Effect::FlushDone(ticket) => {
                    fl.outcomes.insert(ticket, true);
                }
                Effect::FlushFailed(ticket) => {
                    fl.outcomes.insert(ticket, false);
                }
            }
        }
    }

    fn notify_all(&self) {
        self.out_ready.notify_all();
        self.flush_ready.notify_all();
        self.changed.notify_all();
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

    /// A session is up: open a fresh incarnation (dropping anything queued
    /// for a dead one) and say hello. Returns the incarnation this session
    /// owns — its identity for the writer and for teardown.
    fn session_up(&self) -> u64 {
        let mut engine = self.engine.lock().unwrap();
        let incarnation = {
            let mut ob = self.outbound.lock().unwrap();
            ob.incarnation += 1;
            ob.queue.clear();
            ob.incarnation
        };
        engine.connect();
        self.drain(&mut engine);
        drop(engine);
        self.notify_all();
        incarnation
    }

    /// A session died. Exactly-once semantics ride the incarnation: if it
    /// has already moved on — a replacement session started, or a fence
    /// verdict forced the link down — this teardown is history's and does
    /// nothing, which is what keeps it from knocking a Degraded node back
    /// to Suspended after a verdict already landed.
    fn session_down(&self, my_incarnation: u64) {
        let mut engine = self.engine.lock().unwrap();
        let stale = {
            let mut ob = self.outbound.lock().unwrap();
            if ob.incarnation != my_incarnation {
                true
            } else {
                ob.incarnation += 1;
                ob.queue.clear();
                false
            }
        };
        if !stale {
            engine.peer_lost();
            self.drain(&mut engine);
        }
        drop(engine);
        self.notify_all();
    }

    /// The cluster's verdict, applied: force the link down and continue
    /// alone at the bumped era.
    fn fence_peer(&self) -> Result<(), FsError> {
        self.kill_link();
        let mut engine = self.engine.lock().unwrap();
        {
            let mut ob = self.outbound.lock().unwrap();
            ob.incarnation += 1;
            ob.queue.clear();
        }
        if engine.state() != ReplState::Suspended {
            engine.peer_lost();
        }
        let outcome = engine.set_peer_fenced();
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
    fn kill_link(&self) {
        if let Some(socket) = self.live_socket.lock().unwrap().take() {
            let _ = socket.shutdown(Shutdown::Both);
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
}

impl GuestHandle {
    /// Reads are always local and never block on the peer.
    pub fn read(&self, vdisk: u64, offset: u64, len: u64) -> Result<Vec<u8>, FsError> {
        self.shared
            .with_engine(|engine| engine.read_bytes(vdisk, offset, len))
    }

    /// A mutation, blocking through suspension: a suspended node holds the
    /// request (DRBD's suspended I/O) rather than erroring the guest, and
    /// completes it when a verdict or a reconciliation says how.
    fn blocking<T>(
        &self,
        mut f: impl FnMut(&mut ReplNode<FileDisk>) -> Result<T, FsError>,
    ) -> Result<T, FsError> {
        loop {
            let outcome = self.shared.with_engine(|engine| with_room(engine, &mut f));
            match outcome {
                Err(FsError::Suspended) => {
                    if !self.shared.wait_change() {
                        return Err(FsError::Suspended);
                    }
                }
                other => return other,
            }
        }
    }

    pub fn write(&self, vdisk: u64, offset: u64, data: &[u8]) -> Result<(), FsError> {
        self.blocking(|engine| engine.write_bytes(vdisk, offset, data))
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
            if self.shared.shutdown.load(Ordering::SeqCst) {
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

    /// Take the writer lease — the attach step. Blocks through suspension;
    /// refuses (`LeaseHeld`) if the peer holds the vdisk in this era.
    pub fn claim_writer(&self, vdisk: u64) -> Result<(), FsError> {
        self.blocking(|engine| engine.claim_writer(vdisk))
    }

    pub fn vdisk_size(&self, vdisk: u64) -> Result<u64, FsError> {
        self.shared.with_engine(|engine| engine.vdisk_size(vdisk))
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

fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

/// One session, run to its death on the calling thread. The runner thread
/// runs sessions strictly one after another, which is what makes teardown
/// and the next session's hello naturally ordered.
fn run_session(shared: &Arc<Shared>, mut stream: TcpStream) {
    stream.set_nodelay(true).ok();

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

    // Register the socket so a verdict or a replacement can kill it, then
    // open the session in the engine.
    *shared.live_socket.lock().unwrap() = stream.try_clone().ok();
    let incarnation = shared.session_up();

    let writer = {
        let shared = Arc::clone(shared);
        let stream = stream.try_clone();
        std::thread::spawn(move || {
            let Ok(mut stream) = stream else { return };
            writer_loop(&shared, &mut stream, incarnation);
        })
    };

    while let Ok(payload) = read_frame(&mut stream) {
        let message = match wire::decode(&payload) {
            Ok(message) => message,
            Err(err) => {
                eprintln!("peer sent an undecodable frame: {err}");
                break;
            }
        };
        let outcome = shared.with_engine(|engine| engine.handle(message));
        if let Err(err) = outcome {
            // A protocol violation or an engine refusal. Dropping the
            // session is always safe: reconnect runs a resync, and the
            // acknowledgement rule means both nodes hold every
            // acknowledged write however the reconciliation resolves.
            eprintln!("peer message refused by the engine: {err}");
            break;
        }
    }

    let _ = stream.shutdown(Shutdown::Both);
    shared.session_down(incarnation);
    let _ = writer.join();
}

fn writer_loop(shared: &Arc<Shared>, stream: &mut TcpStream, incarnation: u64) {
    loop {
        let message = {
            let mut ob = shared.outbound.lock().unwrap();
            loop {
                if shared.shutdown.load(Ordering::SeqCst) || ob.incarnation != incarnation {
                    return;
                }
                if let Some(message) = ob.queue.pop_front() {
                    break message;
                }
                ob = shared.out_ready.wait_timeout(ob, TICK).unwrap().0;
            }
        };
        let payload = wire::encode(&message);
        if write_frame(stream, &payload).is_err() {
            // The reader will hit the same corpse and run the teardown.
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// The daemon.

pub struct Daemon {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    /// Where the peer listener actually bound, for `listen` on port 0.
    peer_addr: Option<SocketAddr>,
}

impl Daemon {
    pub fn start(config: Config) -> Result<Daemon, String> {
        if config.listen.is_some() == config.dial.is_some() {
            return Err("exactly one of listen/dial must be configured".into());
        }
        let disk = FileDisk::open(&config.brick)
            .map_err(|err| format!("cannot open {}: {err}", config.brick.display()))?;
        let pool = Pool::open(Brick::open(disk).map_err(|err| err.to_string())?)
            .map_err(|err| err.to_string())?;
        let engine = ReplNode::new(pool, config.node);

        let shared = Arc::new(Shared {
            node_id: config.node,
            engine: Mutex::new(engine),
            changed: Condvar::new(),
            outbound: Mutex::new(Outbound {
                queue: VecDeque::new(),
                incarnation: 0,
            }),
            out_ready: Condvar::new(),
            flushes: Mutex::new(Flushes::default()),
            flush_ready: Condvar::new(),
            live_socket: Mutex::new(None),
            shutdown: AtomicBool::new(false),
        });

        let mut threads = Vec::new();
        let mut peer_addr = None;

        if let Some(listen) = config.listen {
            let listener =
                TcpListener::bind(listen).map_err(|err| format!("cannot bind {listen}: {err}"))?;
            peer_addr = Some(listener.local_addr().map_err(|err| err.to_string())?);
            let (tx, rx) = mpsc::channel::<TcpStream>();

            // Accept thread: replaces the live session by killing its
            // socket, so a reconnecting peer is never stuck behind a
            // half-open corpse. The accept is a nonblocking poll: a thread
            // parked inside accept() can only be woken by a connection,
            // and "make a connection to wake it" is exactly the kind of
            // one-shot signal a full backlog silently eats — this hung a
            // shutdown for real before it became a poll.
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
                accept_shared.kill_link();
                if tx.send(stream).is_err() {
                    return;
                }
            }));

            // Runner thread: sessions strictly in sequence.
            let run_shared = Arc::clone(&shared);
            threads.push(std::thread::spawn(move || loop {
                let stream = match rx.recv_timeout(TICK) {
                    Ok(stream) => stream,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if run_shared.shutdown.load(Ordering::SeqCst) {
                            return;
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                };
                run_session(&run_shared, stream);
            }));
        }

        if let Some(dial) = config.dial {
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
                    let space = engine.pool().space();
                    if space.segments_free < space.segments_total / 4 {
                        engine.collect_garbage().map(|_| ())
                    } else if ticks.is_multiple_of(CHECKPOINT_TICKS) {
                        engine.checkpoint()
                    } else {
                        Ok(())
                    }
                });
                if let Err(err) = outcome {
                    eprintln!("maintenance failed: {err}");
                }
            }
        }));

        Ok(Daemon {
            shared,
            threads,
            peer_addr,
        })
    }

    /// Where the peer listener bound — what the other node dials.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }

    pub fn guest(&self) -> GuestHandle {
        GuestHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The cluster's verdict, delivered over the control surface.
    pub fn fence_peer(&self) -> Result<(), FsError> {
        self.shared.fence_peer()
    }

    pub fn status(&self) -> Status {
        self.shared.with_engine(|engine| Status {
            node: engine.node(),
            state: engine.state(),
            era: engine.pool().era(),
            accepts_writes: engine.accepts_writes(),
            vdisks: engine.pool().vdisks(),
            leases: engine.pool().leases(),
            space: engine.pool().space(),
        })
    }

    pub fn checkpoint(&self) -> Result<(), FsError> {
        self.shared.with_engine(|engine| engine.checkpoint())
    }

    pub fn collect_garbage(&self) -> Result<GcStats, FsError> {
        self.shared.with_engine(|engine| engine.collect_garbage())
    }

    pub fn scrub(&self) -> Result<ScrubReport, FsError> {
        self.shared.with_engine(|engine| engine.pool().scrub())
    }

    /// Stop everything, settle the pool, release the brick.
    pub fn shutdown(self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.kill_link();
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
/// unique per brick. The vdisk is created unclaimed — the first attach
/// claims the writer lease through replication, not through the format.
pub fn format_brick(
    path: &Path,
    disk_bytes: u64,
    vdisk_bytes: u64,
    pool_uuid: [u8; 16],
    brick_uuid: [u8; 16],
) -> Result<(), String> {
    if !is_block_device(path) {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|err| format!("cannot create {}: {err}", path.display()))?;
        file.set_len(disk_bytes)
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
        wal_size: if big { 64 << 20 } else { 8 << 20 },
    };
    let brick = Brick::format(disk, params).map_err(|err| err.to_string())?;
    let mut pool = Pool::create(brick).map_err(|err| err.to_string())?;
    pool.create_vdisk(crate::nbd::VDISK, vdisk_bytes)
        .map_err(|err| err.to_string())?;
    pool.checkpoint().map_err(|err| err.to_string())?;
    Ok(())
}
