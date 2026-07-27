//! The HA manager's rules, end to end over the in-memory backends: a lost
//! member's HA machine restarts on the survivor — but only after the loss is
//! *clean*, never while fencing is unfinished — and the definition re-homes
//! to where the machine now lives.

use std::sync::Arc;

use lumen_cluster::backend::mock::{membership_of, MockBackend as ClusterMockBackend};
use lumen_cluster::backend::ClusterBackend;
use lumen_cluster::networks::{AddressedMember, ClusterNetworks, CoreNetwork, ManagementNetwork};
use lumen_cluster::{
    BmcConfig, ClusterDefinition, ClusterRecord, ClusterService, EnvironmentMembership, MemberNode,
    StoredDefinition, VolumeRecord, VolumeSeat,
};
use lumen_controlplane::config::Config;
use lumen_controlplane::realm::RealmRegistry;
use lumen_controlplane::{ha, security, AppState};
use lumen_net::NetworkService;
use lumen_virt::model::{VmConfig, VmDisk};
use lumen_virt::VirtService;
use lumen_zfs::StorageService;

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lumen-ha-flow-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

/// A two-node cluster "alpha" recorded whole, with one replicated volume on
/// minor 1 — the disk of the machine the tests move around.
fn alpha_membership() -> EnvironmentMembership {
    let mut membership = membership_of(&[("alpha-1", Some("alpha")), ("alpha-2", Some("alpha"))]);
    let member = |name: &str, octet: u8| MemberNode {
        name: name.into(),
        ring0: std::net::Ipv4Addr::new(10, 10, 0, octet),
        ring1: std::net::Ipv4Addr::new(192, 168, 10, octet),
        bmc: BmcConfig {
            address: format!("10.20.0.{octet}"),
            username: "ADMIN".into(),
        },
    };
    let core = |name: &str, octet: u8| AddressedMember {
        node: name.into(),
        interface: "nic1".into(),
        address: std::net::Ipv4Addr::new(10, 10, 0, octet),
    };
    let mut record = ClusterRecord::new(
        ClusterDefinition {
            name: "alpha".into(),
            nodes: vec![member("alpha-1", 1), member("alpha-2", 2)],
            preferred_node: Some("alpha-1".into()),
        },
        ClusterNetworks {
            core: CoreNetwork {
                subnet: "10.10.0.0/24".parse().unwrap(),
                mtu: 9000,
                members: vec![core("alpha-1", 1), core("alpha-2", 2)],
            },
            management: ManagementNetwork {
                subnet: "192.168.10.0/24".parse().unwrap(),
                vip: None,
                members: Vec::new(),
            },
            external: Vec::new(),
        },
    );
    record.volumes.push(VolumeRecord {
        name: "vm-101-disk-0".into(),
        size_bytes: 1 << 30,
        zvol_bytes: (1 << 30) + (1 << 20),
        port: 7788,
        minor: 1,
        replicas: vec![
            VolumeSeat {
                node: "alpha-1".into(),
                pool: "boot".into(),
            },
            VolumeSeat {
                node: "alpha-2".into(),
                pool: "boot".into(),
            },
        ],
    });
    membership.clusters.push(record);
    membership
}

fn disk_on(source: &str) -> VmDisk {
    VmDisk {
        id: "vda".into(),
        bus: Default::default(),
        source: source.into(),
        size: 1 << 30,
        cache: Default::default(),
        discard: true,
        boot_index: None,
    }
}

fn machine(vmid: u32, name: &str, ha: bool, source: &str) -> VmConfig {
    VmConfig {
        vmid,
        name: name.into(),
        vcpus: 1,
        memory_mib: 1024,
        ha,
        disks: vec![disk_on(source)],
        ..VmConfig::default()
    }
}

/// The document a define on alpha-2 would have replicated: an HA machine
/// whose one disk is the replicated volume above.
fn ha_definition() -> StoredDefinition {
    StoredDefinition {
        vmid: 101,
        node: "alpha-2".into(),
        xml: lumen_virt::domain_xml::render(&machine(101, "web01", true, "/dev/drbd1")),
    }
}

struct Harness {
    state: Arc<AppState>,
    cluster_backend: Arc<ClusterMockBackend>,
    virt_backend: Arc<lumen_virt::backend::mock::MockBackend>,
    _state_dir: TempDir,
}

fn harness(tag: &str, cluster_backend: ClusterMockBackend) -> Harness {
    let mut config = Config::from_env();
    config.webui_dir = std::env::temp_dir().join("lumen-webui-none");
    config.no_tls = true;
    let state_dir = TempDir::new(tag);
    let network = Arc::new(NetworkService::new(
        Arc::new(lumen_net::backend::mock::MockBackend::appliance()),
        &state_dir.0.join("net"),
        60,
    ));
    let storage = Arc::new(StorageService::new(Arc::new(
        lumen_zfs::backend::mock::MockBackend::appliance(),
    )));
    let sys = Arc::new(lumen_sys::SysService::new(
        Arc::new(lumen_sys::backend::mock::MockPower::appliance()),
        Arc::new(lumen_sys::exec::MockExec::new()),
    ));
    let cluster_backend = Arc::new(cluster_backend);
    let cluster = Arc::new(
        ClusterService::new(
            cluster_backend.clone(),
            Arc::new(lumen_cluster::MockPeers::new()),
            network.clone(),
            &state_dir.0,
            "test",
        )
        .with_node("alpha-1")
        .with_environment(&alpha_membership()),
    );
    let drbd = Arc::new(lumen_drbd::DrbdService::new(
        Arc::new(lumen_drbd::backend::mock::MockBackend::appliance()),
        Arc::new(lumen_drbd::MockVolumePeers::new()),
        cluster.clone(),
        storage.clone(),
    ));
    let virt_backend = Arc::new(lumen_virt::backend::mock::MockBackend::appliance());
    let virt = Arc::new(VirtService::new(
        virt_backend.clone(),
        storage.clone(),
        network.clone(),
        Arc::new(lumen_drbd::MockVmVolumes::standalone()),
    ));
    let state = Arc::new(AppState {
        config,
        jwt_secret: security::session_secret(b"test-secret-test-secret-test-secret!".to_vec()),
        tls: None,
        realms: RealmRegistry::new(),
        sys,
        network,
        storage,
        virt,
        cluster,
        drbd,
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
    });
    Harness {
        state,
        cluster_backend,
        virt_backend,
        _state_dir: state_dir,
    }
}

#[tokio::test]
async fn an_unclean_member_is_waited_on_and_a_clean_loss_restarts_here() {
    // alpha-2 is partitioned away: offline and unclean — fencing unfinished.
    let harness = harness(
        "restart",
        ClusterMockBackend::environment().with_partition("alpha", "alpha-2"),
    );
    harness
        .state
        .cluster
        .peer_store_definition(&ha_definition())
        .unwrap();

    // The fence-confirmed rule: unclean means wait, however long it takes.
    ha::sweep(&harness.state).await;
    assert!(
        harness.virt_backend.names().is_empty(),
        "an unclean loss must never trigger a restart"
    );

    // The operator's break-glass (or a completed fence) makes the loss
    // clean, and the next sweep acts.
    harness
        .cluster_backend
        .confirm_node_dead("alpha-2")
        .await
        .unwrap();
    ha::sweep(&harness.state).await;
    assert!(harness.virt_backend.is_defined("web01"), "adopted here");
    assert!(
        harness
            .virt_backend
            .state_of("web01")
            .is_some_and(|s| s.is_running()),
        "and started"
    );

    // The definition re-homed: the machine lives here now, and the record
    // says so before the next failure.
    let stored = harness.state.cluster.stored_definitions().unwrap();
    assert_eq!(
        stored[0].node, "alpha-1",
        "the adopting node is the new home"
    );

    // A second sweep changes nothing — the machine is already adopted.
    ha::sweep(&harness.state).await;
    assert_eq!(harness.virt_backend.names().len(), 1);
}

#[tokio::test]
async fn a_machine_without_the_flag_or_without_replicas_stays_down() {
    let harness = harness(
        "no-flag",
        ClusterMockBackend::environment().with_partition("alpha", "alpha-2"),
    );
    // Clean loss from the start.
    harness
        .cluster_backend
        .confirm_node_dead("alpha-2")
        .await
        .unwrap();

    // Not flagged: stays down.
    harness
        .state
        .cluster
        .peer_store_definition(&StoredDefinition {
            vmid: 102,
            node: "alpha-2".into(),
            xml: lumen_virt::domain_xml::render(&machine(102, "db01", false, "/dev/drbd1")),
        })
        .unwrap();

    // Flagged, but its disk is a local zvol on the dead node: nowhere to go.
    harness
        .state
        .cluster
        .peer_store_definition(&StoredDefinition {
            vmid: 103,
            node: "alpha-2".into(),
            xml: lumen_virt::domain_xml::render(&machine(
                103,
                "cache01",
                true,
                "/dev/zvol/boot/lumen/vm-103-disk-0",
            )),
        })
        .unwrap();

    ha::sweep(&harness.state).await;
    assert!(
        harness.virt_backend.names().is_empty(),
        "neither machine may restart: {:?}",
        harness.virt_backend.names()
    );
}
