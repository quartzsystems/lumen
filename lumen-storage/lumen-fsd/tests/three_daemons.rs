//! Three daemons over real sockets: the mesh — every member listening,
//! each dialing every lower id — carrying placement, fetched reads, a
//! per-member fence verdict, and a reassignment driven over the control
//! surface. The simulation pinned the semantics; this pins the plumbing.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lumen_fsd::{control, export::nbd::VDISK, format_brick, Config, Daemon};

const DISK_BYTES: u64 = 64 << 20;
const VDISK_BYTES: u64 = 8 << 20;
const POOL: [u8; 16] = [0x3A; 16];
const MEMBERS: [u8; 3] = [0, 1, 2];

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "lumen-fsd-mesh-{}-{name}.brick",
            std::process::id()
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
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn format_single(path: &std::path::Path, brick_uuid: [u8; 16]) {
    format_brick(
        path,
        Some(DISK_BYTES),
        0,
        true,
        Vec::new(),
        Some(VDISK_BYTES),
        POOL,
        brick_uuid,
    )
    .unwrap();
}

fn mesh_daemon(brick: &Scratch, node: u8, dials: Vec<SocketAddr>) -> Daemon {
    Daemon::start(Config {
        node,
        bricks: vec![brick.0.clone()],
        listen: Some("127.0.0.1:0".parse().unwrap()),
        dials,
        members: MEMBERS.to_vec(),
    })
    .unwrap()
}

fn all_synced(daemons: &[&Daemon]) -> bool {
    daemons
        .iter()
        .all(|d| d.status().state == lumen_fs::ReplState::Synced)
}

/// Three fresh bricks, one mesh: node 0 listens; node 1 listens and dials
/// 0; node 2 dials 0 and 1.
fn synced_trio(name: &str) -> (Daemon, Daemon, Daemon, Scratch, Scratch, Scratch) {
    let brick_a = Scratch::new(&format!("{name}-a"));
    let brick_b = Scratch::new(&format!("{name}-b"));
    let brick_c = Scratch::new(&format!("{name}-c"));
    format_single(&brick_a.0, [0xC0; 16]);
    format_single(&brick_b.0, [0xC1; 16]);
    format_single(&brick_c.0, [0xC2; 16]);
    let a = mesh_daemon(&brick_a, 0, Vec::new());
    let b = mesh_daemon(&brick_b, 1, vec![a.peer_addr().unwrap()]);
    let c = mesh_daemon(
        &brick_c,
        2,
        vec![a.peer_addr().unwrap(), b.peer_addr().unwrap()],
    );
    wait_until("the trio to sync", || all_synced(&[&a, &b, &c]));
    (a, b, c, brick_a, brick_b, brick_c)
}

#[test]
fn a_mesh_of_three_places_data_and_serves_every_read() {
    let (a, b, c, _ba, _bb, brick_c) = synced_trio("place");
    assert_eq!(a.status().map_version, Some(1));

    let guest = a.guest();
    guest.claim_writer(VDISK).unwrap();
    // Enough distinct blocks that placement provably spreads: some land
    // away from the writer, some away from each reader.
    let block = 4096u64;
    for index in 0..64u64 {
        let mut data = vec![0u8; block as usize];
        data[0..8].copy_from_slice(&index.to_le_bytes());
        data[8] = 0xEE;
        guest.write(VDISK, index * block, &data).unwrap();
    }
    guest.flush().unwrap();

    // Every member answers every read — locally where it homes the block,
    // by a real fetch over a real socket where it does not.
    for (name, daemon) in [("b", &b), ("c", &c)] {
        for index in 0..64u64 {
            let found = daemon.guest().read(VDISK, index * block, block).unwrap();
            assert_eq!(
                u64::from_le_bytes(found[0..8].try_into().unwrap()),
                index,
                "member {name} misread block {index}"
            );
            assert_eq!(found[8], 0xEE);
        }
    }

    // A per-member verdict: node 2 dies, the survivors continue at one
    // agreed era, and writes still acknowledge.
    c.shutdown();
    wait_until("the survivors to notice", || {
        a.status().state != lumen_fs::ReplState::Synced
            || b.status().state != lumen_fs::ReplState::Synced
    });
    let era = a.status().era_target.max(b.status().era_target);
    assert_eq!(
        control::command(&a, &format!("fence-peer 2 {era}")),
        format!("ok: continuing without node 2 at era {era}")
    );
    assert_eq!(
        control::command(&b, &format!("fence-peer 2 {era}")),
        format!("ok: continuing without node 2 at era {era}")
    );
    let guest = a.guest();
    let mut data = vec![0u8; block as usize];
    data[8] = 0xDD;
    guest.write(VDISK, 0, &data).unwrap();
    guest.flush().unwrap();

    // The member returns, rejoins the mesh, and serves the write it
    // missed.
    let c = mesh_daemon(
        &brick_c,
        2,
        vec![a.peer_addr().unwrap(), b.peer_addr().unwrap()],
    );
    wait_until("the trio to heal", || all_synced(&[&a, &b, &c]));
    let found = c.guest().read(VDISK, 0, block).unwrap();
    assert_eq!(found[8], 0xDD, "the revenant serves a stale block");

    // The attach contract still holds across the mesh.
    assert_eq!(
        c.guest().attach(VDISK),
        Err(lumen_fs::FsError::LeaseHeld {
            vdisk: VDISK,
            holder: 0
        })
    );
    c.shutdown();
    b.shutdown();
    a.shutdown();
}

#[test]
fn a_reassignment_runs_over_the_control_surface() {
    let (a, b, c, _ba, _bb, _bc) = synced_trio("reassign");
    let guest = a.guest();
    guest.claim_writer(VDISK).unwrap();
    let block = 4096u64;
    for index in 0..32u64 {
        let mut data = vec![0u8; block as usize];
        data[0..8].copy_from_slice(&index.to_le_bytes());
        guest.write(VDISK, index * block, &data).unwrap();
    }
    guest.flush().unwrap();

    // Shrink to two members, entirely over the control verbs — prepare on
    // every member, the maintenance loops move the blocks, commit when
    // every member reports nothing owed.
    for daemon in [&a, &b, &c] {
        let reply = control::command(daemon, "reassign 2 0 1");
        assert!(reply.starts_with("ok"), "prepare refused: {reply}");
    }
    wait_until("every member to owe nothing", || {
        [&a, &b, &c]
            .iter()
            .all(|d| control::command(d, "reassign-status").contains("owed=0"))
    });
    for daemon in [&a, &b, &c] {
        let reply = control::command(daemon, "reassign-commit");
        assert!(reply.starts_with("ok"), "commit refused: {reply}");
    }
    for daemon in [&a, &b, &c] {
        assert_eq!(daemon.status().map_version, Some(2));
        assert_eq!(daemon.status().reassign_pending, None);
    }

    // Under the new map the leaver still answers every read, by fetch.
    for index in 0..32u64 {
        let found = c.guest().read(VDISK, index * block, block).unwrap();
        assert_eq!(
            u64::from_le_bytes(found[0..8].try_into().unwrap()),
            index,
            "the leaver misread block {index} after the shrink"
        );
    }
    c.shutdown();
    b.shutdown();
    a.shutdown();
}
