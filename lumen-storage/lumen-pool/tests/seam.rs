//! The compute seam over a pool, exercised the way `VirtService` uses it.
//!
//! The mock fleet refuses what a real daemon refuses — an export of a vdisk
//! whose pen another member holds, a handover nobody opened, a device that
//! is not there — so these tests fail for the reasons production would
//! rather than passing on a stub's goodwill.

use std::sync::Arc;

use lumen_pool::{
    MemberStatus, MemberView, MigrationWindow, MockFleet, PoolFleet, PoolHealth, PoolService,
    Replication, VmDiskRequest, VmVolumes,
};

const HERE: &str = "lumen01";
const THERE: &str = "lumen02";

fn pooled() -> (Arc<MockFleet>, PoolService) {
    let fleet = Arc::new(MockFleet::pooled(&[HERE, THERE]));
    let service = PoolService::new(fleet.clone(), "pool0");
    (fleet, service)
}

fn request(name: &str, size_bytes: u64) -> VmDiskRequest {
    VmDiskRequest {
        name: name.to_string(),
        size_bytes,
    }
}

#[tokio::test]
async fn a_created_disk_names_a_device_that_exists_here_and_a_vdisk_that_exists_everywhere() {
    let (fleet, service) = pooled();
    let disk = service
        .create_disk(&request("vm-7-disk-0", 8 << 30))
        .await
        .unwrap();

    // The identity is derived, so the device path says which machine's disk
    // it is without anything having been written down.
    assert_eq!(disk.name, "vm-7-disk-0");
    assert_eq!(disk.device, "/dev/ublkb1792");
    assert_eq!(disk.size_bytes, 8 << 30);
    assert_eq!(disk.cluster, "pool0");
    assert_eq!(disk.members, vec![HERE, THERE]);

    // The vdisk replicated by itself; the export did not.
    assert_eq!(fleet.existing(), vec![1792]);
    assert_eq!(fleet.exported_on(HERE), vec![1792]);
    assert!(
        fleet.exported_on(THERE).is_empty(),
        "the device is materialized where the machine is, not everywhere"
    );

    // And the seam can find it again from the path alone.
    let found = service.disk_of(&disk.device).await.unwrap().unwrap();
    assert_eq!(found.name, disk.name);
    assert_eq!(found.size_bytes, disk.size_bytes);
    assert_eq!(found.members, disk.members);
}

#[tokio::test]
async fn a_failed_export_takes_the_vdisk_with_it() {
    // A disk nobody can open is worse than no disk: it looks like progress.
    let (fleet, service) = pooled();
    fleet.fail_next_export();
    let err = service
        .create_disk(&request("vm-9-disk-1", 1 << 30))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("export"), "{err}");
    assert!(
        fleet.existing().is_empty(),
        "the vdisk outlived the failed export"
    );
}

#[tokio::test]
async fn only_names_the_compute_domain_uses_are_accepted() {
    let (fleet, service) = pooled();
    for wrong in ["scratch", "vm-0-disk-0", "vm-7-disk-256", "vm-7"] {
        let err = service
            .create_disk(&request(wrong, 1 << 30))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("vm-<vmid>-disk-<n>"),
            "{wrong}: {err}"
        );
    }
    assert!(fleet.existing().is_empty());
}

#[tokio::test]
async fn disk_of_is_a_predicate_and_says_no_to_everything_else() {
    let (_fleet, service) = pooled();
    // Local disks, raw devices, and our own bootstrap vdisk — id 1 decodes
    // to machine 0, which is no machine, so it is not a pooled machine disk.
    for foreign in ["/dev/zvol/lumen/vm-7-disk-0", "/dev/ublkb1", "/dev/sda", ""] {
        assert!(
            service.disk_of(foreign).await.unwrap().is_none(),
            "{foreign} was claimed"
        );
    }
    // A well-formed path for a vdisk that does not exist is also `None` —
    // not an error, because the question was only whether it is ours.
    assert!(service.disk_of("/dev/ublkb1792").await.unwrap().is_none());
}

#[tokio::test]
async fn a_node_with_no_pool_explains_itself() {
    let fleet = Arc::new(MockFleet::standalone());
    let service = PoolService::new(fleet.clone(), "pool0");
    let err = service
        .create_disk(&request("vm-1-disk-0", 1 << 30))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no LumenFS pool"), "{err}");
    // And a path cannot be ours if there is no pool to own it.
    assert!(service.disk_of("/dev/ublkb256").await.unwrap().is_none());
}

#[tokio::test]
async fn destroying_takes_every_device_down_before_the_vdisk() {
    let (fleet, service) = pooled();
    let disk = service
        .create_disk(&request("vm-3-disk-2", 1 << 30))
        .await
        .unwrap();
    // Put a device on the far side too, the way an open window would.
    service
        .migration_window(
            &disk.device,
            MigrationWindow::Open {
                destination: THERE.to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(fleet.exported_on(THERE), vec![770]);

    service.destroy_disk(&disk.device).await.unwrap();
    assert!(fleet.existing().is_empty(), "the vdisk survived");
    assert!(fleet.exported_on(HERE).is_empty(), "a device outlived it");
    assert!(fleet.exported_on(THERE).is_empty(), "a device outlived it");

    // And destroying what is not there is a refusal, not a silence.
    assert!(service.destroy_disk(&disk.device).await.is_err());
}

#[tokio::test]
async fn a_migration_opens_a_window_hands_the_pen_over_and_lands() {
    let (fleet, service) = pooled();
    let disk = service
        .create_disk(&request("vm-5-disk-0", 4 << 30))
        .await
        .unwrap();
    let vdisk = 5 * 256;
    assert_eq!(
        fleet.lease(vdisk),
        Some((0, None)),
        "the maker holds the pen"
    );

    // Open: the window opens *and* the destination gets its device, penless.
    service
        .migration_window(
            &disk.device,
            MigrationWindow::Open {
                destination: THERE.to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(fleet.lease(vdisk), Some((0, Some(1))));
    assert_eq!(
        fleet.exported_on(THERE),
        vec![vdisk],
        "the destination needs its device before the domain starts there"
    );

    // Accepted: the source hands the pen over, and where it goes is read
    // from the open window rather than passed in again.
    service
        .migration_window(&disk.device, MigrationWindow::Accepted)
        .await
        .unwrap();
    assert_eq!(
        fleet.lease(vdisk),
        Some((1, None)),
        "the pen did not move, or the window stayed open"
    );
    // And the source's device came down with the handover — the export
    // left standing is what would refuse the machine's way back.
    assert!(
        fleet.exported_on(HERE).is_empty(),
        "the source kept a device for a machine that left"
    );
    assert_eq!(
        fleet.exported_on(THERE),
        vec![vdisk],
        "the destination serves it now"
    );
}

#[tokio::test]
async fn an_abandoned_migration_leaves_the_pen_and_takes_the_far_device_down() {
    let (fleet, service) = pooled();
    let disk = service
        .create_disk(&request("vm-6-disk-0", 1 << 30))
        .await
        .unwrap();
    let vdisk = 6 * 256;
    service
        .migration_window(
            &disk.device,
            MigrationWindow::Open {
                destination: THERE.to_string(),
            },
        )
        .await
        .unwrap();

    service
        .migration_window(&disk.device, MigrationWindow::Aborted)
        .await
        .unwrap();
    assert_eq!(
        fleet.lease(vdisk),
        Some((0, None)),
        "the machine never left, so the pen must not have"
    );
    assert!(
        fleet.exported_on(THERE).is_empty(),
        "the destination kept a device for a machine that never arrived"
    );
    assert_eq!(
        fleet.exported_on(HERE),
        vec![vdisk],
        "the source still runs"
    );
}

#[tokio::test]
async fn accepting_without_a_window_is_refused_rather_than_guessed_at() {
    let (_fleet, service) = pooled();
    let disk = service
        .create_disk(&request("vm-8-disk-0", 1 << 30))
        .await
        .unwrap();
    let err = service
        .migration_window(&disk.device, MigrationWindow::Accepted)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no migration window"), "{err}");
}

#[tokio::test]
async fn placement_is_every_member_and_a_foreign_disk_is_named() {
    let (_fleet, service) = pooled();
    let disk = service
        .create_disk(&request("vm-4-disk-0", 1 << 30))
        .await
        .unwrap();

    // Placement by content hash means no member is a better host, so
    // eligibility is simply the pool — which is all pooled HA needs.
    assert_eq!(
        service
            .common_members(std::slice::from_ref(&disk.device))
            .await
            .unwrap(),
        vec![HERE, THERE]
    );

    // And a device this pool did not make is refused by name, the way the
    // sweep and the drain both rely on.
    let err = service
        .common_members(&[disk.device, "/dev/zvol/lumen/vm-4-disk-1".to_string()])
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("/dev/zvol/lumen/vm-4-disk-1"),
        "{err}"
    );
}

/// The observed view is what a console page renders, so what matters is that
/// it describes the pool an operator actually has: every member named, every
/// vdisk carrying the machine disk it backs, and the pen shown as a member's
/// name rather than an engine node id.
#[tokio::test]
async fn the_observed_view_describes_the_pool_a_console_would_draw() {
    let (fleet, service) = pooled();
    let disk = service
        .create_disk(&request("vm-7-disk-3", 512 << 20))
        .await
        .unwrap();

    let state = service.state().await;
    assert_eq!(state.health, PoolHealth::Healthy);
    assert_eq!(
        state.members.iter().map(|m| &m.name).collect::<Vec<_>>(),
        vec![HERE, THERE]
    );
    assert_eq!(state.answering(), vec![HERE, THERE]);

    let vdisk = state
        .vdisks
        .iter()
        .find(|v| v.device == disk.device)
        .expect("the created disk should be in the view");
    assert_eq!(vdisk.label(), "vm-7-disk-3");
    assert_eq!(vdisk.size_bytes, 512 << 20);
    assert!(!vdisk.migrating());
    // Created here, so served here and nowhere else — the asymmetry the
    // whole service is shaped around, visible in the view.
    assert_eq!(vdisk.exported_on, vec![HERE]);
    // And the pen reads back as a member's name, which is what an operator
    // recognises.
    assert_eq!(state.member_named(vdisk.holder.unwrap()), Some(HERE));

    // A window in flight is visible as one.
    service
        .migration_window(
            &disk.device,
            MigrationWindow::Open {
                destination: THERE.to_string(),
            },
        )
        .await
        .unwrap();
    let migrating = service.state().await;
    let vdisk = migrating
        .vdisks
        .iter()
        .find(|v| v.device == disk.device)
        .unwrap();
    assert!(vdisk.migrating(), "an open window should show as one");
    assert_eq!(
        migrating.member_named(vdisk.handing_to.unwrap()),
        Some(THERE)
    );
    // Both members serve the disk mid-window: that is what penless means.
    assert_eq!(vdisk.exported_on, vec![HERE, THERE]);
    let _ = fleet;
}

/// The case the view exists to get right: one member unreachable. A pool of
/// two with a silent member is not a healthy pool of one, and the page an
/// operator opened to find that out must not be the thing that blanks.
#[tokio::test]
async fn a_silent_member_downgrades_the_verdict_without_blanking_the_view() {
    let (fleet, service) = pooled();
    let disk = service
        .create_disk(&request("vm-9-disk-0", 1 << 30))
        .await
        .unwrap();
    fleet.silence(
        THERE,
        "cannot reach lumen02's pool daemon: connection refused",
    );

    let state = service.state().await;
    assert_eq!(
        state.health,
        PoolHealth::Unknown,
        "a pool with a member it cannot see does not get to claim health"
    );
    assert_eq!(state.members.len(), 2, "the silent member was dropped");
    assert_eq!(state.answering(), vec![HERE]);
    // The reason reaches the console instead of becoming "unknown".
    let MemberView::Silent(why) = &state.members[1].view else {
        panic!("expected lumen02 to be silent, got {:?}", state.members[1]);
    };
    assert!(why.contains("lumen02"), "{why}");

    // And the rest of the pool is still described — read from the member
    // that did answer, because the listings are replicated.
    let vdisk = state
        .vdisks
        .iter()
        .find(|v| v.device == disk.device)
        .expect("the view should still describe the pool's disks");
    assert_eq!(vdisk.label(), "vm-9-disk-0");
    assert_eq!(
        vdisk.exported_on,
        vec![HERE],
        "a silent member should contribute nothing rather than an empty list"
    );
}

/// Trouble that was actually observed is reported as trouble — distinct from
/// not being able to see.
#[tokio::test]
async fn a_fenced_survivor_reads_as_degraded_rather_than_unknown() {
    let (fleet, service) = pooled();
    for (member, node) in [(HERE, 0u8), (THERE, 1u8)] {
        fleet.set_status(
            member,
            MemberStatus {
                node,
                replication: Replication::Degraded,
                era: 2,
                accepts_writes: true,
                segments_free: 3,
                segments_total: 30,
                usable_bytes: 0,
                free_bytes: 0,
                tiers: Vec::new(),
                // The mock fills the listings from its own pool state, so
                // these are what a test pins health with, not the data.
                vdisks: Vec::new(),
                leases: Vec::new(),
                stream: (12, 12, 0),
                peers: Vec::new(),
                map_version: None,
                seats: None,
                reassign_pending: None,
                pool_uuid: None,
                scrub: None,
            },
        );
    }
    let state = service.state().await;
    assert_eq!(state.health, PoolHealth::Degraded);
    let status = state.members[0].view.status().unwrap();
    assert_eq!(status.era, 2, "a bumped era is what a survivor runs at");
    assert_eq!(status.free_percent(), Some(10));
}

/// Snapshots are replicated, so taking one is a one-member call — and the
/// observed view carries each disk's history so the dialog needs no second
/// round of questions.
#[tokio::test]
async fn a_snapshot_is_taken_once_and_shows_up_on_the_disk_it_belongs_to() {
    let (_fleet, service) = pooled();
    let disk = service
        .create_disk(&request("vm-7-disk-3", 512 << 20))
        .await
        .unwrap();
    let other = service
        .create_disk(&request("vm-8-disk-0", 1 << 30))
        .await
        .unwrap();

    service.snapshot(&disk.device, 1_700_000_100).await.unwrap();
    service.snapshot(&disk.device, 1_700_000_200).await.unwrap();
    service
        .snapshot(&other.device, 1_700_000_300)
        .await
        .unwrap();

    let listed = service.snapshots(&disk.device).await.unwrap();
    assert_eq!(
        listed.iter().map(|s| s.snapshot).collect::<Vec<_>>(),
        vec![1_700_000_100, 1_700_000_200],
        "one disk's history, oldest first, and nobody else's"
    );

    // And the same history reaches the page without asking again.
    let state = service.state().await;
    let view = state
        .vdisks
        .iter()
        .find(|v| v.device == disk.device)
        .unwrap();
    assert_eq!(view.snapshots.len(), 2);

    // Taking the same id twice is refused rather than silently replacing a
    // snapshot somebody may be relying on.
    assert!(service.snapshot(&disk.device, 1_700_000_100).await.is_err());

    // Deleting takes exactly the one named.
    service
        .delete_snapshot(&disk.device, 1_700_000_100)
        .await
        .unwrap();
    let listed = service.snapshots(&disk.device).await.unwrap();
    assert_eq!(
        listed.iter().map(|s| s.snapshot).collect::<Vec<_>>(),
        vec![1_700_000_200]
    );
}

/// The contract the console promises, and the one the engine will not keep
/// on its own: a rollback replaces every block under a live filesystem, so
/// it must be refused while anything is serving the disk.
#[tokio::test]
async fn a_rollback_is_refused_while_the_disk_is_open_anywhere_in_the_pool() {
    let (fleet, service) = pooled();
    let disk = service
        .create_disk(&request("vm-7-disk-0", 1 << 30))
        .await
        .unwrap();
    service.snapshot(&disk.device, 1_700_000_100).await.unwrap();

    // Created here, so it is being served here: refused, and the member
    // holding it is named rather than left to be guessed at.
    let err = service
        .rollback(&disk.device, 1_700_000_100)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("in use on lumen01"),
        "the member serving it should be named: {err}"
    );

    // Now take the local device down and serve it on the FAR member only —
    // the case a check that only looked at this node would wave through,
    // and the reason every member is asked.
    let vdisk = 7 * 256;
    fleet.unexport(HERE, vdisk).await.unwrap();
    service
        .migration_window(
            &disk.device,
            MigrationWindow::Open {
                destination: THERE.to_string(),
            },
        )
        .await
        .unwrap();
    assert!(fleet.exported_on(HERE).is_empty());
    assert_eq!(fleet.exported_on(THERE), vec![vdisk]);

    let err = service
        .rollback(&disk.device, 1_700_000_100)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("in use on lumen02"),
        "a disk open only on the far member must still refuse: {err}"
    );

    // With it served nowhere, the same rollback goes through.
    fleet.unexport(THERE, vdisk).await.unwrap();
    service.rollback(&disk.device, 1_700_000_100).await.unwrap();
}

/// "I could not check" must never read as "nobody has it": a member that
/// cannot answer refuses the rollback, because it might be the one serving
/// the disk.
#[tokio::test]
async fn a_member_that_cannot_be_asked_refuses_the_rollback() {
    let (fleet, service) = pooled();
    let disk = service
        .create_disk(&request("vm-7-disk-0", 1 << 30))
        .await
        .unwrap();
    service.snapshot(&disk.device, 1_700_000_100).await.unwrap();
    // Nothing is serving it anywhere — so the only thing standing between
    // this rollback and the engine is the member that cannot answer.
    fleet.unexport(HERE, 7 * 256).await.unwrap();
    fleet.silence(THERE, "connection refused");

    let err = service
        .rollback(&disk.device, 1_700_000_100)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("could not say"),
        "an unreachable member should block a rollback, not be assumed idle: {err}"
    );
}

#[tokio::test]
async fn a_node_with_no_pool_has_a_view_that_says_so() {
    let service = PoolService::new(Arc::new(MockFleet::standalone()), "pool0");
    let state = service.state().await;
    assert_eq!(state.health, PoolHealth::None);
    assert!(state.members.is_empty());
    assert!(state.vdisks.is_empty());
}

#[test]
fn the_service_is_a_drop_in_for_the_seam() {
    // The whole point of the crate: `VirtService` holds the seam as
    // `Arc<dyn VmVolumes>`, so this must coerce without the compute domain
    // knowing which engine is underneath. If this stops compiling, the
    // integration is broken however well the logic tests pass.
    let fleet = Arc::new(MockFleet::pooled(&[HERE, THERE]));
    let service: Arc<dyn VmVolumes> = Arc::new(PoolService::new(fleet, "pool0"));
    assert_eq!(Arc::strong_count(&service), 1);
}
