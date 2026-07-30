//! The two-node contract against reality: real sockets, real files, real
//! threads — the same rules tests/repl_two_node.rs pins under simulation,
//! one layer closer to the metal. This is docs/lumenfs.md phase 2's exit
//! test in its harness form: write on one node, read on the other, kill
//! the link and watch I/O suspend rather than diverge, fence and watch it
//! continue, return and watch it resync.
//!
//! Determinism note: these tests wait on observable states with generous
//! deadlines rather than sleeping fixed amounts, and every negative check
//! ("did not complete") bounds a *suspension* the positive path then
//! releases — so a slow machine makes them slower, not flaky.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lumen_fsd::{format_brick, nbd::VDISK, Config, Daemon};

const DISK_BYTES: u64 = 64 << 20;
const VDISK_BYTES: u64 = 8 << 20;
const POOL: [u8; 16] = [0xA5; 16];

/// A scratch path unique to this test run; best-effort removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "lumen-fsd-test-{}-{}-{name}.brick",
            std::process::id(),
            std::thread::current().name().unwrap_or("t").len()
        ));
        let _ = std::fs::remove_file(&path);
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn listener_daemon(brick: &Scratch, node: u8) -> Daemon {
    Daemon::start(Config {
        node,
        brick: brick.0.clone(),
        listen: Some("127.0.0.1:0".parse().unwrap()),
        dial: None,
    })
    .unwrap()
}

fn dialer_daemon(brick: &Scratch, node: u8, peer: SocketAddr) -> Daemon {
    Daemon::start(Config {
        node,
        brick: brick.0.clone(),
        listen: None,
        dial: Some(peer),
    })
    .unwrap()
}

/// Two fresh bricks of one pool, one synced pair of daemons.
fn synced_pair(name: &str) -> (Daemon, Daemon, Scratch, Scratch) {
    let brick_a = Scratch::new(&format!("{name}-a"));
    let brick_b = Scratch::new(&format!("{name}-b"));
    format_brick(&brick_a.0, DISK_BYTES, VDISK_BYTES, POOL, [0xB0; 16]).unwrap();
    format_brick(&brick_b.0, DISK_BYTES, VDISK_BYTES, POOL, [0xB1; 16]).unwrap();
    let a = listener_daemon(&brick_a, 0);
    let b = dialer_daemon(&brick_b, 1, a.peer_addr().unwrap());
    wait_until("the pair to sync", || {
        a.status().state == lumen_fs::ReplState::Synced
            && b.status().state == lumen_fs::ReplState::Synced
    });
    (a, b, brick_a, brick_b)
}

#[test]
fn a_write_on_one_node_is_read_on_the_other() {
    let (a, b, _ba, _bb) = synced_pair("basic");
    let guest = a.guest();
    guest.claim_writer(VDISK).unwrap();
    guest
        .write(VDISK, 4096, b"crossed the wire for real")
        .unwrap();
    guest.flush().unwrap();

    // The flush was the two-node barrier: the peer holds it *now*, not
    // eventually.
    let over_there = b.guest().read(VDISK, 4096, 25).unwrap();
    assert_eq!(over_there, b"crossed the wire for real");

    // And the lease crossed with it: the peer knows who holds the pen.
    let lease = b.status().leases;
    assert_eq!(lease.len(), 1);
    assert_eq!(lease[0].0, VDISK);
    assert_eq!(lease[0].1.holder, 0);

    b.shutdown();
    a.shutdown();
}

#[test]
fn a_dead_peer_suspends_io_and_a_verdict_releases_it() {
    let (a, b, _ba, brick_b) = synced_pair("suspend");
    let guest = a.guest();
    guest.claim_writer(VDISK).unwrap();
    guest.write(VDISK, 0, b"before the failure").unwrap();
    guest.flush().unwrap();

    // The peer dies. No verdict: writes must block, not error, not ack.
    b.shutdown();
    wait_until("the survivor to notice", || {
        a.status().state == lumen_fs::ReplState::Suspended
    });

    let (tx, rx) = mpsc::channel();
    let blocked_guest = a.guest();
    std::thread::spawn(move || {
        let outcome = blocked_guest
            .write(VDISK, 8192, b"held until the verdict")
            .and_then(|_| blocked_guest.flush());
        let _ = tx.send(outcome);
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(500)),
        Err(mpsc::RecvTimeoutError::Timeout),
        "suspended i/o completed without a verdict"
    );

    // The verdict arrives; the parked write completes single-copy.
    a.fence_peer().unwrap();
    rx.recv_timeout(Duration::from_secs(10))
        .expect("the verdict did not release the write")
        .expect("the released write failed");
    assert_eq!(a.status().state, lumen_fs::ReplState::Degraded);
    assert_eq!(a.status().era, 2);

    // The dead node returns from its own brick, resyncs, rejoins — and
    // holds everything acknowledged while it was gone.
    let b = dialer_daemon(&brick_b, 1, a.peer_addr().unwrap());
    wait_until("the returned node to rejoin", || {
        a.status().state == lumen_fs::ReplState::Synced
            && b.status().state == lumen_fs::ReplState::Synced
    });
    assert_eq!(b.status().era, 2);
    assert_eq!(
        b.guest().read(VDISK, 8192, 22).unwrap(),
        b"held until the verdict"
    );

    // Lockstep is real again: a fresh write completes two-node.
    guest.write(VDISK, 16384, b"after the storm").unwrap();
    guest.flush().unwrap();
    assert_eq!(
        b.guest().read(VDISK, 16384, 15).unwrap(),
        b"after the storm"
    );

    b.shutdown();
    a.shutdown();
}

#[test]
fn a_verdict_with_the_link_still_up_forces_it_down_first() {
    let (a, b, _ba, _bb) = synced_pair("live-fence");
    let guest = a.guest();
    guest.claim_writer(VDISK).unwrap();

    // The cluster says the peer is dead while the socket still looks
    // alive. The daemon must not wait for TCP to agree.
    a.fence_peer().unwrap();
    assert_eq!(a.status().state, lumen_fs::ReplState::Degraded);
    guest.write(VDISK, 0, b"alone but moving").unwrap();
    guest.flush().unwrap();

    // The fenced side saw its session die and suspended — and then the
    // dialer reconnects, the eras order the two histories, and the pair
    // heals into the survivor's era on its own.
    wait_until("the pair to heal after the verdict", || {
        a.status().state == lumen_fs::ReplState::Synced
            && b.status().state == lumen_fs::ReplState::Synced
    });
    assert_eq!(a.status().era, b.status().era);
    assert_eq!(b.guest().read(VDISK, 0, 16).unwrap(), b"alone but moving");

    b.shutdown();
    a.shutdown();
}

#[test]
fn a_brick_from_a_different_pool_is_refused_at_the_door() {
    let brick_a = Scratch::new("refuse-a");
    let brick_c = Scratch::new("refuse-c");
    format_brick(&brick_a.0, DISK_BYTES, VDISK_BYTES, POOL, [0xB0; 16]).unwrap();
    format_brick(&brick_c.0, DISK_BYTES, VDISK_BYTES, [0x5A; 16], [0xB2; 16]).unwrap();
    let a = listener_daemon(&brick_a, 0);
    let c = dialer_daemon(&brick_c, 1, a.peer_addr().unwrap());

    // The imposter keeps dialing and keeps being refused: neither side
    // ever leaves Suspended, and neither engine ever hears a hello.
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(a.status().state, lumen_fs::ReplState::Suspended);
    assert_eq!(c.status().state, lumen_fs::ReplState::Suspended);

    c.shutdown();
    a.shutdown();
}
