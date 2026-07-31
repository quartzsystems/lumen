//! The fleet against two real daemons, over real sockets.
//!
//! The mock in `seam.rs` proves the service's logic; this proves the
//! transport under it — that the verbs reach a daemon, that its answers
//! parse, and that a lease really moves between two engines replicating to
//! each other. Between them the only untested seam is the ublk export,
//! which needs a Linux kernel with `ublk_drv` and so lives in the
//! appliance smoke scripts rather than here.
//!
//! The daemons are leaked deliberately. `control::serve` borrows a `Daemon`
//! for as long as it serves, and the serve loop never returns; leaking gives
//! it the `'static` it needs, and a test process reclaiming nothing at exit
//! is the cheapest correct answer.

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_fsd::{control, format_brick, Config, Daemon};
use lumen_pool::{PoolFleet, Replication, SocketFleet};

const DISK_BYTES: u64 = 64 << 20;
const VDISK_BYTES: u64 = 8 << 20;
const POOL_UUID: [u8; 16] = [0xC7; 16];

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    let mut path = std::env::temp_dir();
    // Per test, not just per process: these run in parallel, and
    // `format_brick` rightly refuses to clobber a brick that exists.
    path.push(format!(
        "lumen-pool-test-{}-{name}.brick",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    Scratch(path)
}

fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// Serve a daemon's control surface on an ephemeral port, and say where.
fn control_on(daemon: Daemon) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let daemon: &'static Daemon = Box::leak(Box::new(daemon));
    std::thread::spawn(move || control::serve(listener, daemon));
    addr
}

/// A real two-node pool, each with a control surface, and the fleet that
/// speaks to them.
fn real_pool(tag: &str) -> (SocketFleet, Scratch, Scratch) {
    let brick_a = scratch(&format!("{tag}-a"));
    let brick_b = scratch(&format!("{tag}-b"));
    for (brick, uuid) in [(&brick_a, [0xA1; 16]), (&brick_b, [0xB1; 16])] {
        format_brick(
            &brick.0,
            Some(DISK_BYTES),
            0,
            true,
            Vec::new(),
            Some(VDISK_BYTES),
            POOL_UUID,
            uuid,
        )
        .unwrap();
    }

    let a = Daemon::start(Config {
        node: 0,
        bricks: vec![brick_a.0.clone()],
        listen: Some("127.0.0.1:0".parse().unwrap()),
        dial: None,
    })
    .unwrap();
    let peer = a.peer_addr().unwrap();
    let b = Daemon::start(Config {
        node: 1,
        bricks: vec![brick_b.0.clone()],
        listen: None,
        dial: Some(peer),
    })
    .unwrap();

    let a_control = control_on(a);
    let b_control = control_on(b);
    let fleet = SocketFleet::new(vec![
        ("lumen01".to_string(), a_control),
        ("lumen02".to_string(), b_control),
    ]);
    (fleet, brick_a, brick_b)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_fleet_reaches_both_daemons_and_their_ids_are_their_own() {
    let (fleet, _a, _b) = real_pool("ids");

    // The ids the leases speak come from each daemon, not from the config
    // that told us where to find it.
    wait_until("both daemons to answer", || {
        futures_lite_block(fleet.node_id("lumen01")).is_ok()
    });
    assert_eq!(fleet.node_id("lumen01").await.unwrap(), 0);
    assert_eq!(fleet.node_id("lumen02").await.unwrap(), 1);

    // A member nobody configured is named rather than dialed.
    assert!(fleet.node_id("lumen09").await.is_err());

    // Both see the brick's own bootstrap vdisk.
    let here = fleet.vdisks("lumen01").await.unwrap();
    assert_eq!(here, vec![(1, VDISK_BYTES)]);
    assert_eq!(fleet.vdisks("lumen02").await.unwrap(), here);
}

/// The status line is written by the daemon and read by the fleet, and the
/// two halves live in different crates — so the round trip is pinned against
/// a **real** daemon rather than a canned string. A formatter and a parser
/// that agree only in a unit test are one edit from agreeing nowhere.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_daemon_describes_itself_and_the_fleet_reads_every_field() {
    let (fleet, _a, _b) = real_pool("status");
    wait_until("the pair to sync", || {
        futures_lite_block(fleet.status("lumen01"))
            .is_ok_and(|s| s.replication == Replication::Synced)
    });

    let status = fleet.status("lumen01").await.unwrap();
    assert_eq!(status.node, 0);
    assert_eq!(status.replication, Replication::Synced);
    assert!(status.accepts_writes, "a synced member takes writes");
    assert!(status.era >= 1, "era should be a real generation");
    assert!(
        status.segments_total > 0 && status.segments_free <= status.segments_total,
        "brick space came back nonsense: {status:?}"
    );
    assert!(status.free_percent().is_some());

    // The peer answers for itself, with its own id.
    assert_eq!(fleet.status("lumen02").await.unwrap().node, 1);

    // And the listings inside the line parse: a real write moves the stream
    // counters, which is the field that convicted the elided-flush bug.
    let vdisk = 2;
    fleet
        .create_vdisk("lumen01", vdisk, VDISK_BYTES, 0)
        .await
        .unwrap();
    wait_until("the vdisk to reach both members", || {
        futures_lite_block(fleet.status("lumen02"))
            .is_ok_and(|s| s.vdisks.iter().any(|(id, _)| *id == vdisk))
    });
    let status = fleet.status("lumen01").await.unwrap();
    assert!(
        status.vdisks.contains(&(vdisk, VDISK_BYTES)),
        "the vdisk listing did not survive the round trip: {status:?}"
    );
    assert!(
        status.leases.iter().any(|(id, _)| *id == vdisk),
        "the lease listing did not survive the round trip: {status:?}"
    );
    assert!(
        status.stream.0 > 0,
        "creating a vdisk should have crossed the wire: {status:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vdisk_created_through_the_fleet_replicates_and_its_lease_moves() {
    let (fleet, _a, _b) = real_pool("lease");
    wait_until("both daemons to answer", || {
        futures_lite_block(fleet.node_id("lumen02")).is_ok()
    });
    // Wait for the pair to be in lockstep, or a create would be refused as
    // suspended — which is the engine's contract, not a flake.
    wait_until("the pair to sync", || {
        futures_lite_block(fleet.create_vdisk("lumen01", 1795, 4 << 20, 0)).is_ok()
    });

    // Creation replicated: the peer knows without being told.
    wait_until("the peer to learn the vdisk", || {
        futures_lite_block(fleet.vdisks("lumen02"))
            .map(|all| all.iter().any(|(id, _)| *id == 1795))
            .unwrap_or(false)
    });
    // And the maker holds the pen, as both members see it.
    assert_eq!(fleet.lease("lumen01", 1795).await.unwrap(), Some((0, None)));
    wait_until("the lease to reach the peer", || {
        futures_lite_block(fleet.lease("lumen02", 1795))
            .map(|lease| lease == Some((0, None)))
            .unwrap_or(false)
    });

    // The window, the handover, and the confirmation — over sockets, between
    // two engines, exactly as the seam drives them.
    fleet.handover("lumen01", 1795, 1).await.unwrap();
    assert_eq!(
        fleet.lease("lumen01", 1795).await.unwrap(),
        Some((0, Some(1)))
    );
    assert!(
        fleet.accept("lumen02", 1795).await.is_err(),
        "the destination confirmed a pen the source still held"
    );
    fleet.relinquish("lumen01", 1795, 1).await.unwrap();
    assert_eq!(fleet.lease("lumen01", 1795).await.unwrap(), Some((1, None)));
    wait_until("the destination to see the pen arrive", || {
        futures_lite_block(fleet.accept("lumen02", 1795)).is_ok()
    });

    // Deleting replicates too.
    fleet.delete_vdisk("lumen02", 1795).await.unwrap();
    wait_until("the deletion to reach the other member", || {
        futures_lite_block(fleet.vdisks("lumen01"))
            .map(|all| all.iter().all(|(id, _)| *id != 1795))
            .unwrap_or(false)
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_is_not_listening_is_a_backend_failure_not_a_refusal() {
    // The distinction the console renders differently: "the pool says no"
    // versus "the pool did not answer".
    let fleet = SocketFleet::new(vec![(
        "lumen01".to_string(),
        "127.0.0.1:1".parse::<SocketAddr>().unwrap(),
    )]);
    let err = fleet.vdisks("lumen01").await.unwrap_err();
    assert!(
        err.to_string().contains("cannot reach"),
        "an unreachable daemon should say so: {err}"
    );
}

/// Block on a future from inside a sync closure — `wait_until` polls, and
/// these calls are all short administrative round trips.
fn futures_lite_block<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

/// The seam itself, over the real fleet, for the paths that do not need a
/// ublk device. `create_disk` and `migration_window` both export, so they
/// belong to the appliance smoke scripts; everything else is here.
#[tokio::test(flavor = "multi_thread")]
async fn the_seam_over_real_daemons_answers_for_paths_that_need_no_device() {
    use lumen_drbd::VmVolumes;
    let (fleet, _a, _b) = real_pool("seam");
    wait_until("both daemons to answer", || {
        futures_lite_block(fleet.node_id("lumen02")).is_ok()
    });
    wait_until("the pair to sync", || {
        futures_lite_block(fleet.create_vdisk("lumen01", 1795, 4 << 20, 0)).is_ok()
    });

    let service = lumen_pool::PoolService::new(Arc::new(fleet), "pool0");

    // A real vdisk, found from its path alone and named by derivation.
    let disk = service
        .disk_of("/dev/ublkb1795")
        .await
        .unwrap()
        .expect("the vdisk exists, so the device is ours");
    assert_eq!(disk.name, "vm-7-disk-3");
    assert_eq!(disk.size_bytes, 4 << 20);
    assert_eq!(disk.members, vec!["lumen01", "lumen02"]);

    // The bootstrap vdisk is not a machine disk, and a path we never made
    // is not ours — both answered against a live pool.
    assert!(service.disk_of("/dev/ublkb1").await.unwrap().is_none());
    assert!(service.disk_of("/dev/drbd1").await.unwrap().is_none());

    // Placement is the pool, which is all pooled HA eligibility needs.
    assert_eq!(
        service
            .common_members(std::slice::from_ref(&disk.device))
            .await
            .unwrap(),
        vec!["lumen01", "lumen02"]
    );
}
