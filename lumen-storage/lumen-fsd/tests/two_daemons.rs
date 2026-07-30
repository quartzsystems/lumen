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

use lumen_fs::FsError;
use lumen_fsd::{control, daemon::Attach, format_brick, nbd::VDISK, Config, Daemon};

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

/// The vdisk id the lifecycle tests create beyond the formatted one.
const SECOND: u64 = 2;

#[test]
fn a_vdisks_life_replicates_from_either_end() {
    let (a, b, _ba, _bb) = synced_pair("lifecycle");
    a.guest().create_vdisk(SECOND, 4 << 20).unwrap();

    // Creation is a replicated operation: the peer knows the vdisk exists
    // without being asked, and knows who holds its pen.
    wait_until("the peer to learn the new vdisk", || {
        b.guest().vdisk_size(SECOND).is_ok()
    });
    assert_eq!(b.guest().lease(SECOND).unwrap().holder, 0);

    // And it is a real disk on both: written here, readable there after
    // the two-node barrier.
    let guest = a.guest();
    guest.write(SECOND, 0, b"a second disk").unwrap();
    guest.flush().unwrap();
    assert_eq!(b.guest().read(SECOND, 0, 13).unwrap(), b"a second disk");

    // Deletion replicates the same way.
    guest.delete_vdisk(SECOND).unwrap();
    wait_until("the peer to lose the vdisk", || {
        b.guest().vdisk_size(SECOND).is_err()
    });
    assert!(a.guest().vdisk_size(SECOND).is_err());

    b.shutdown();
    a.shutdown();
}

#[test]
fn a_live_migration_moves_the_pen_and_never_lends_it_twice() {
    // The shape the compute seam drives: the destination opens the disk
    // while the source is still writing it, and exactly one of them may
    // write at every instant.
    let (a, b, _ba, _bb) = synced_pair("migration");
    let source = a.guest();
    let destination = b.guest();
    source.create_vdisk(SECOND, 4 << 20).unwrap();
    wait_until("the peer to learn the vdisk", || {
        destination.vdisk_size(SECOND).is_ok()
    });

    // An ordinary attach takes the pen.
    assert_eq!(source.attach(SECOND).unwrap(), Attach::Writer);
    source.write(SECOND, 0, b"written before the move").unwrap();
    source.flush().unwrap();

    // Before the window, the destination cannot even open it: this is
    // somebody else's disk.
    assert!(
        destination.attach(SECOND).is_err(),
        "the disk opened on a node with no claim to it"
    );

    // The window opens. Now the destination may hold it open — penless.
    source.begin_handover(SECOND, 1).unwrap();
    wait_until("the window to reach the destination", || {
        destination
            .lease(SECOND)
            .is_some_and(|lease| lease.handing_to == Some(1))
    });
    assert_eq!(destination.attach(SECOND).unwrap(), Attach::Penless);

    // Mid-window: the source still writes, the destination still cannot.
    source
        .write(SECOND, 4096, b"written during the window")
        .unwrap();
    source.flush().unwrap();
    assert_eq!(
        destination
            .write(SECOND, 8192, b"not yours yet")
            .unwrap_err(),
        FsError::NotWriter(SECOND)
    );

    // The instant of handover, and it is exactly an instant: after it the
    // source is the one refused.
    destination.accept_handover(SECOND).unwrap();
    destination
        .write(SECOND, 8192, b"written after the move")
        .unwrap();
    destination.flush().unwrap();
    assert_eq!(
        source
            .write(SECOND, 12288, b"not mine any more")
            .unwrap_err(),
        FsError::NotWriter(SECOND)
    );

    // Everything from both sides of the move is on both nodes.
    for (node, name) in [(&a, "source"), (&b, "destination")] {
        let guest = node.guest();
        assert_eq!(
            guest.read(SECOND, 0, 23).unwrap(),
            b"written before the move",
            "{name} lost the pre-move write"
        );
        assert_eq!(
            guest.read(SECOND, 4096, 25).unwrap(),
            b"written during the window",
            "{name} lost the in-window write"
        );
        assert_eq!(
            guest.read(SECOND, 8192, 22).unwrap(),
            b"written after the move",
            "{name} lost the post-move write"
        );
    }

    b.shutdown();
    a.shutdown();
}

#[test]
fn an_aborted_migration_leaves_the_disk_where_it_started() {
    let (a, b, _ba, _bb) = synced_pair("abort");
    let source = a.guest();
    let destination = b.guest();
    source.create_vdisk(SECOND, 4 << 20).unwrap();
    wait_until("the peer to learn the vdisk", || {
        destination.vdisk_size(SECOND).is_ok()
    });
    source.attach(SECOND).unwrap();
    source.begin_handover(SECOND, 1).unwrap();
    wait_until("the window to reach the destination", || {
        destination
            .lease(SECOND)
            .is_some_and(|lease| lease.handing_to == Some(1))
    });
    assert_eq!(destination.attach(SECOND).unwrap(), Attach::Penless);

    // The migration fails, so the window closes from the side that never
    // stopped being the writer.
    source.abort_handover(SECOND).unwrap();
    wait_until("the closed window to reach the destination", || {
        destination
            .lease(SECOND)
            .is_some_and(|lease| lease.handing_to.is_none())
    });

    // The destination can no longer accept, and can no longer even open
    // the disk: the offer is withdrawn.
    assert_eq!(
        destination.accept_handover(SECOND).unwrap_err(),
        FsError::NoSuchHandover(SECOND)
    );
    assert!(destination.attach(SECOND).is_err());

    // And the source is still the writer, and still works.
    source.write(SECOND, 0, b"still mine").unwrap();
    source.flush().unwrap();
    assert_eq!(destination.read(SECOND, 0, 10).unwrap(), b"still mine");

    b.shutdown();
    a.shutdown();
}

#[test]
fn an_orchestrator_drives_a_whole_migration_over_the_control_protocol() {
    // Every step the compute seam will take, taken through the surface it
    // will use — the library's real dispatcher, against two real daemons.
    let (a, b, _ba, _bb) = synced_pair("control");
    let source = |verb: &str| control::command(&a, verb);
    let destination = |verb: &str| control::command(&b, verb);

    assert_eq!(
        source(&format!("vdisk-create {SECOND} 4194304")),
        format!("ok: vdisk {SECOND} of 4194304 bytes")
    );
    wait_until("the peer to learn the vdisk", || {
        destination("vdisks").contains(&format!("{SECOND}=4194304"))
    });

    // Created means claimed, and the peer can see whose it is.
    assert_eq!(
        destination(&format!("lease {SECOND}")),
        "ok: holder=0 era=1"
    );

    // The window, as three acts across two members.
    assert!(source(&format!("handover {SECOND} 1")).starts_with("ok"));
    wait_until("the window to reach the destination", || {
        destination(&format!("lease {SECOND}")) == "ok: holder=0 era=1 handing=1"
    });
    // Accepting is the destination's act, and refused on the source.
    assert!(
        source(&format!("accept {SECOND}")).starts_with("error"),
        "the source accepted its own handover"
    );
    assert!(destination(&format!("accept {SECOND}")).starts_with("ok"));
    // The destination holds the pen the instant it accepts; the source
    // learns by replication, so its view catches up rather than changing
    // with it. That gap is real and is discussed in docs/lumenfs.md — the
    // guest being paused on the source is what keeps it from mattering.
    assert_eq!(
        destination(&format!("lease {SECOND}")),
        "ok: holder=1 era=1",
        "the destination did not take the pen on accepting"
    );
    wait_until("the source to learn the pen moved", || {
        source(&format!("lease {SECOND}")) == "ok: holder=1 era=1"
    });

    // Errors carry reasons, and unknown verbs do not panic.
    let complaint = source("lease 404");
    assert!(
        complaint.starts_with("error") && complaint.contains("404"),
        "an unknown vdisk should be named: {complaint}"
    );
    assert!(source("nonsense").starts_with("error"));
    assert!(source("vdisk-create").starts_with("error"));
    assert!(source("vdisk-create x 1").starts_with("error"));

    // And the node-wide verbs answer, including the cross-member
    // diagnostic that says whether the two really agree.
    assert!(source("status").contains("state Synced"));
    assert_eq!(source("checkpoint"), "ok");
    assert!(source("scrub").contains("corrupt=0"));
    assert_eq!(
        source(&format!("hash {SECOND}")),
        destination(&format!("hash {SECOND}")),
        "the two members disagree about the vdisk's contents"
    );

    b.shutdown();
    a.shutdown();
}

#[test]
fn an_aborted_window_is_visible_to_both_members_over_the_protocol() {
    let (a, b, _ba, _bb) = synced_pair("control-abort");
    let source = |verb: &str| control::command(&a, verb);
    let destination = |verb: &str| control::command(&b, verb);

    source(&format!("vdisk-create {SECOND} 4194304"));
    wait_until("the peer to learn the vdisk", || {
        destination(&format!("lease {SECOND}")).starts_with("ok: holder")
    });
    source(&format!("handover {SECOND} 1"));
    wait_until("the window to reach the destination", || {
        destination(&format!("lease {SECOND}")).contains("handing=1")
    });

    assert!(source(&format!("abort {SECOND}")).starts_with("ok"));
    wait_until("the closed window to reach the destination", || {
        destination(&format!("lease {SECOND}")) == "ok: holder=0 era=1"
    });
    assert!(
        destination(&format!("accept {SECOND}")).starts_with("error"),
        "the destination accepted a withdrawn offer"
    );

    b.shutdown();
    a.shutdown();
}

#[test]
fn a_cancelled_handle_releases_parked_io_without_a_verdict() {
    // Suspended I/O waits for a verdict, and a verdict may never come.
    // So taking an export down cannot mean waiting for its parked
    // requests: `unexport` on a node whose peer died unfenced would hang,
    // and hang the control surface with it. Cancelling is the release.
    let (a, b, _ba, _bb) = synced_pair("cancel");
    let guest = a.guest();
    guest.claim_writer(VDISK).unwrap();
    b.shutdown();
    wait_until("the survivor to notice", || {
        a.status().state == lumen_fs::ReplState::Suspended
    });

    let (tx, rx) = mpsc::channel();
    let parked = guest.clone();
    std::thread::spawn(move || {
        let _ = tx.send(parked.write(VDISK, 0, b"never acknowledged"));
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(400)),
        Err(mpsc::RecvTimeoutError::Timeout),
        "the write did not park, so this test proves nothing"
    );

    // The switch a teardown throws — and it reaches the clone servicing
    // the export, which is the whole point.
    guest.cancel();
    let outcome = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("cancelling did not release the parked write");
    assert_eq!(
        outcome.unwrap_err(),
        FsError::Suspended,
        "a released write must fail, never silently succeed"
    );

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
