//! The compute seam over a pool, exercised the way `VirtService` uses it.
//!
//! The mock fleet refuses what a real daemon refuses — an export of a vdisk
//! whose pen another member holds, a handover nobody opened, a device that
//! is not there — so these tests fail for the reasons production would
//! rather than passing on a stub's goodwill.

use std::sync::Arc;

use lumen_drbd::{MigrationWindow, VmDiskRequest, VmVolumes};
use lumen_pool::{MockFleet, PoolService};

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
        members: Vec::new(),
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
    // Another engine's disks, and our own bootstrap vdisk — id 1 decodes to
    // machine 0, which is no machine, so it is not a pooled machine disk.
    for foreign in [
        "/dev/drbd1",
        "/dev/zvol/lumen/vm-7-disk-0",
        "/dev/ublkb1",
        "/dev/sda",
        "",
    ] {
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
        .common_members(&[disk.device, "/dev/drbd1".to_string()])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("/dev/drbd1"), "{err}");
}

#[test]
fn the_service_is_a_drop_in_for_the_seam() {
    // The whole point of the crate: `VirtService` holds the seam as
    // `Arc<dyn VmVolumes>`, so this must coerce without the compute domain
    // knowing which engine is underneath. If this stops compiling, the
    // integration is broken however well the logic tests pass.
    let fleet = Arc::new(MockFleet::pooled(&[HERE, THERE]));
    let service: Arc<dyn VmVolumes> = Arc::new(PoolService::new(fleet, "pool0"));
    // And the DRBD implementation still satisfies the same shape, so both
    // engines remain interchangeable behind it.
    let drbd: Arc<dyn VmVolumes> = Arc::new(lumen_drbd::MockVmVolumes::standalone());
    assert_eq!(
        [Arc::strong_count(&service), Arc::strong_count(&drbd)],
        [1, 1]
    );
}
