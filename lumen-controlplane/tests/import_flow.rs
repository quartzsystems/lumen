//! The import path end to end over the real router: an OVA is uploaded and
//! read back as the machine it describes, the commit defines that machine
//! and fills its disks from the archive, and a fill that fails removes the
//! half-imported machine again. The converter is the injected seam — the
//! same shape as the mock hypervisor beside it — so `make test` needs no
//! qemu-img and touches no block device.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use lumen_controlplane::config::Config;
use lumen_controlplane::realm::{AuthFailure, Realm, RealmKind, RealmRegistry};
use lumen_controlplane::vm_import::{ImportState, MockConvert};
use lumen_controlplane::{app, AppState};
use lumen_net::NetworkService;
use lumen_virt::backend::mock::MockBackend as MockVirtBackend;
use lumen_virt::VirtService;
use lumen_zfs::backend::mock::MockBackend as MockZfsBackend;
use lumen_zfs::StorageService;

const TICKET_SECRET: &[u8] = b"test-secret-test-secret-test-secret!";

struct MockRealm;

#[async_trait]
impl Realm for MockRealm {
    fn id(&self) -> &str {
        "lumen"
    }
    fn name(&self) -> &str {
        "Lumen"
    }
    fn kind(&self) -> RealmKind {
        RealmKind::Builtin
    }
    async fn authenticate(&self, username: &str, password: &str) -> Result<(), AuthFailure> {
        if username == "root" && password == "correct-horse" {
            Ok(())
        } else {
            Err(AuthFailure::Denied)
        }
    }
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "lumen-cp-import-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    router: axum::Router,
    virt: Arc<MockVirtBackend>,
    converter: Arc<MockConvert>,
    spool: std::path::PathBuf,
    cookie: String,
    _state_dir: TempDir,
}

async fn harness(tag: &str, converter: MockConvert) -> Harness {
    let mut config = Config::from_env();
    config.webui_dir = std::env::temp_dir().join("lumen-webui-none");
    config.no_tls = true;
    config.session_ttl_secs = 3600;
    config.net_confirm_secs = 60;

    let state_dir = TempDir::new(tag);
    let spool = state_dir.0.join("spool");
    let converter = Arc::new(converter);
    let virt_backend = Arc::new(MockVirtBackend::appliance());

    let network = Arc::new(NetworkService::new(
        Arc::new(lumen_net::backend::mock::MockBackend::appliance()),
        &state_dir.0,
        60,
    ));
    network.management_bridge().await.unwrap();
    network.confirm().await.unwrap();

    let storage = Arc::new(
        StorageService::new(Arc::new(MockZfsBackend::appliance()))
            .with_iso_root(state_dir.0.join("iso"))
            .with_root_pool(Some("boot".into())),
    );
    let virt = Arc::new(
        VirtService::new(
            virt_backend.clone(),
            storage.clone(),
            network.clone(),
            Arc::new(lumen_pool::MockVmVolumes::standalone()),
        )
        .with_osinfo_root(state_dir.0.join("osinfo")),
    );

    let sys = Arc::new(lumen_sys::SysService::new(
        Arc::new(lumen_sys::backend::mock::MockPower::appliance()),
        Arc::new(lumen_sys::exec::MockExec::new()),
    ));
    let cluster = Arc::new(lumen_cluster::ClusterService::new(
        Arc::new(lumen_cluster::backend::mock::MockBackend::appliance()),
        Arc::new(lumen_cluster::MockPeers::new()),
        network.clone(),
        &state_dir.0,
        "test",
    ));
    let router = app(Arc::new(AppState {
        config,
        jwt_secret: lumen_controlplane::security::session_secret(TICKET_SECRET.to_vec()),
        tls: None,
        realms: RealmRegistry::new().register(Box::new(MockRealm)),
        sys,
        network,
        storage,
        virt,
        cluster,
        peers: Arc::new(lumen_controlplane::inventory::NoPeers),
        pool: lumen_controlplane::pool::PoolPresence::Absent,
        tasks: lumen_controlplane::tasks::TaskLog::ephemeral(),
        updates: Arc::new(lumen_update::UpdateService::new(
            Arc::new(lumen_update::MockUpdates::new()),
            "test-node",
        )),
        drain: Default::default(),
        update_job: Default::default(),
        roll: Default::default(),
        pool_deploy: Arc::new(lumen_pool::PoolDeploy::new(
            lumen_sys::exec::MockExec::working(),
        )),
        pool_peers: Arc::new(lumen_controlplane::inventory::NoPeers),
        pool_job: Default::default(),
        import: ImportState::new(&spool, converter.clone()),
    }));

    let response = router
        .clone()
        .oneshot(
            Request::post("/api/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"username":"root","password":"correct-horse"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("login must set the session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    Harness {
        router,
        virt: virt_backend,
        converter,
        spool,
        cookie,
        _state_dir: state_dir,
    }
}

impl Harness {
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().method(method).uri(path);
        request = request.header(header::COOKIE, &self.cookie);
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => request.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    /// The upload: raw bytes, the way the console's XMLHttpRequest sends
    /// them.
    async fn upload(&self, name: &str, bytes: Vec<u8>) -> (StatusCode, serde_json::Value) {
        let request = Request::put(format!("/api/vms/import/{name}"))
            .header(header::COOKIE, &self.cookie)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, bytes.len())
            .body(Body::from(bytes))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    /// Poll the pending feed until the job leaves `running`.
    async fn settled(&self) -> serde_json::Value {
        for _ in 0..250 {
            let (status, progress) = self.call("GET", "/api/vms/import/pending", None).await;
            assert_eq!(status, StatusCode::OK, "{progress}");
            if progress["phase"] != "running" {
                return progress;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the import never settled");
    }
}

// --- the archive fixture ------------------------------------------------------

/// The descriptor a vCenter export writes: two disks behind a paravirtual
/// SCSI controller, a vmxnet3 adapter with a reserved address, EFI firmware.
const DESCRIPTOR: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Envelope xmlns="http://schemas.dmtf.org/ovf/envelope/1"
          xmlns:ovf="http://schemas.dmtf.org/ovf/envelope/1"
          xmlns:rasd="http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_ResourceAllocationSettingData"
          xmlns:vmw="http://www.vmware.com/schema/ovf">
  <References>
    <File ovf:id="file1" ovf:href="web-disk1.vmdk" ovf:size="700"/>
    <File ovf:id="file2" ovf:href="web-disk2.vmdk" ovf:size="512"/>
  </References>
  <DiskSection>
    <Info>Virtual disk information</Info>
    <Disk ovf:capacity="40" ovf:capacityAllocationUnits="byte * 2^30" ovf:diskId="vmdisk1" ovf:fileRef="file1"/>
    <Disk ovf:capacity="16" ovf:capacityAllocationUnits="byte * 2^30" ovf:diskId="vmdisk2" ovf:fileRef="file2"/>
  </DiskSection>
  <NetworkSection>
    <Info>The list of logical networks</Info>
    <Network ovf:name="VM Network"/>
  </NetworkSection>
  <VirtualSystem ovf:id="web-frontend">
    <Info>A virtual machine</Info>
    <Name>web-frontend</Name>
    <OperatingSystemSection ovf:id="101" vmw:osType="rhel9_64Guest">
      <Info>The operating system installed</Info>
    </OperatingSystemSection>
    <VirtualHardwareSection>
      <Info>Virtual hardware requirements</Info>
      <Item>
        <rasd:InstanceID>1</rasd:InstanceID>
        <rasd:ResourceType>3</rasd:ResourceType>
        <rasd:VirtualQuantity>2</rasd:VirtualQuantity>
      </Item>
      <Item>
        <rasd:AllocationUnits>byte * 2^20</rasd:AllocationUnits>
        <rasd:InstanceID>2</rasd:InstanceID>
        <rasd:ResourceType>4</rasd:ResourceType>
        <rasd:VirtualQuantity>4096</rasd:VirtualQuantity>
      </Item>
      <Item>
        <rasd:InstanceID>3</rasd:InstanceID>
        <rasd:ResourceSubType>VirtualSCSI</rasd:ResourceSubType>
        <rasd:ResourceType>6</rasd:ResourceType>
      </Item>
      <Item>
        <rasd:HostResource>ovf:/disk/vmdisk1</rasd:HostResource>
        <rasd:InstanceID>5</rasd:InstanceID>
        <rasd:Parent>3</rasd:Parent>
        <rasd:ResourceType>17</rasd:ResourceType>
      </Item>
      <Item>
        <rasd:HostResource>ovf:/disk/vmdisk2</rasd:HostResource>
        <rasd:InstanceID>6</rasd:InstanceID>
        <rasd:Parent>3</rasd:Parent>
        <rasd:ResourceType>17</rasd:ResourceType>
      </Item>
      <Item>
        <rasd:Address>00:50:56:9a:12:34</rasd:Address>
        <rasd:Connection>VM Network</rasd:Connection>
        <rasd:InstanceID>8</rasd:InstanceID>
        <rasd:ResourceSubType>VMXNET3</rasd:ResourceSubType>
        <rasd:ResourceType>10</rasd:ResourceType>
      </Item>
      <vmw:Config ovf:required="false" vmw:key="firmware" vmw:value="efi"/>
    </VirtualHardwareSection>
  </VirtualSystem>
</Envelope>"#;

/// A ustar archive with the given members — the same builder the ovf
/// module's own tests use, because an OVA is exactly this.
fn tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in entries {
        let mut header = [0u8; 512];
        header[0..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", data.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|b| *b as u32).sum();
        let checksum = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
    }
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

fn web_frontend_ova() -> Vec<u8> {
    tar(&[
        ("web.ovf", DESCRIPTOR.as_bytes()),
        ("web-disk1.vmdk", &[0xAAu8; 700]),
        ("web-disk2.vmdk", &[0xBBu8; 512]),
    ])
}

// --- the path the whole feature exists for ------------------------------------

#[tokio::test]
async fn an_ova_becomes_a_machine_with_its_disks_filled() {
    let h = harness("whole", MockConvert::default()).await;

    // The upload answers with the machine inside the archive.
    let (status, body) = h.upload("web.ova", web_frontend_ova()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let appliance = &body["appliance"];
    assert_eq!(appliance["name"], "web-frontend");
    assert_eq!(appliance["vcpus"], 2);
    assert_eq!(appliance["memory_mib"], 4096);
    assert_eq!(appliance["firmware"], "uefi");
    assert_eq!(appliance["scsi_controller"], "pvscsi");
    assert_eq!(appliance["disks"][0]["file"], "web-disk1.vmdk");
    assert_eq!(appliance["disks"][0]["capacity"], 40u64 * 1024 * 1024 * 1024);
    assert_eq!(appliance["disks"][0]["bus"], "virtio-scsi");
    assert_eq!(appliance["nics"][0]["network"], "VM Network");
    assert_eq!(appliance["nics"][0]["model"], "vmxnet3");
    assert_eq!(appliance["nics"][0]["mac"], "00:50:56:9a:12:34");

    // And the spool lists it, for a console that comes back later.
    let (status, listing) = h.call("GET", "/api/vms/import", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listing["imports"][0]["name"], "web.ova");

    // A commit that disagrees with the archive about what is in it is
    // refused before anything exists.
    let (status, refusal) = h
        .call(
            "POST",
            "/api/vms/import/web.ova",
            Some(r#"{"name":"web01","disks":[{"pool":"boot"}],"nics":[{"bridge":"br0"}]}"#),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refusal}");

    // The real commit: both disks to the boot pool, the adapter onto br0.
    let (status, accepted) = h
        .call(
            "POST",
            "/api/vms/import/web.ova",
            Some(
                r#"{"name":"web01",
                    "disks":[{"pool":"boot"},{"pool":"boot"}],
                    "nics":[{"bridge":"br0"}],
                    "start":false}"#,
            ),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["phase"], "running");

    let done = h.settled().await;
    assert_eq!(done["phase"], "complete", "{done}");
    assert_eq!(done["vmid"], 100);

    // The machine exists, shaped by the archive: its firmware, its
    // controller, its bus, its reserved hardware address.
    let (status, vm) = h.call("GET", "/api/vms/100", None).await;
    assert_eq!(status, StatusCode::OK, "{vm}");
    assert_eq!(vm["name"], "web01");
    assert_eq!(vm["firmware"], "uefi");
    assert_eq!(vm["scsi_controller"], "pvscsi");
    assert_eq!(vm["vcpus"], 2);
    assert_eq!(vm["memory_mib"], 4096);
    assert_eq!(vm["disks"][0]["bus"], "virtio-scsi");
    assert_eq!(vm["disks"][1]["bus"], "virtio-scsi");
    assert_eq!(vm["nics"][0]["model"], "vmxnet3");
    assert_eq!(vm["nics"][0]["id"], "00:50:56:9a:12:34");
    assert_eq!(vm["state"], "shut_off");

    // Each volume was filled from the member that describes it, in place —
    // the offsets point into the archive, and the devices are the machine's
    // own.
    let calls = h.converter.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].2, 700);
    assert_eq!(calls[0].3, "/dev/zvol/boot/lumen/vm-100-disk-0");
    assert_eq!(calls[1].2, 512);
    assert_eq!(calls[1].3, "/dev/zvol/boot/lumen/vm-100-disk-1");
    assert!(calls[0].1 > 0 && calls[1].1 > calls[0].1, "{calls:?}");

    // The archive left the spool with the import that consumed it.
    assert!(!h.spool.join("web.ova").exists());
    let (_, listing) = h.call("GET", "/api/vms/import", None).await;
    assert!(listing["imports"].as_array().unwrap().is_empty());

    // And the machine's history says where it came from.
    let (_, tasks) = h.call("GET", "/api/vms/100/tasks", None).await;
    let entries = tasks["tasks"].as_array().unwrap();
    assert!(
        entries.iter().any(|t| t["action"] == "import"),
        "{entries:?}"
    );
}

#[tokio::test]
async fn a_failed_fill_removes_the_half_imported_machine() {
    let h = harness(
        "unwind",
        MockConvert {
            fail_on: Some(1),
            ..Default::default()
        },
    )
    .await;

    let (status, _) = h.upload("web.ova", web_frontend_ova()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .call(
            "POST",
            "/api/vms/import/web.ova",
            Some(
                r#"{"name":"web01",
                    "disks":[{"pool":"boot"},{"pool":"boot"}],
                    "nics":[{"bridge":"br0"}]}"#,
            ),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let done = h.settled().await;
    assert_eq!(done["phase"], "failed", "{done}");
    assert!(
        done["error"].as_str().unwrap().contains("pretend converter"),
        "{done}"
    );

    // The machine is gone again — nothing half-imported is left wearing a
    // machine's name — and the archive stays for the next attempt.
    let (status, _) = h.call("GET", "/api/vms/100", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(h.virt.names().is_empty());
    assert!(h.spool.join("web.ova").exists());
}

#[tokio::test]
async fn what_is_not_an_ova_is_refused_and_not_spooled() {
    let h = harness("refuse", MockConvert::default()).await;

    // Not a tar at all.
    let (status, body) = h.upload("junk.ova", vec![0x51u8; 2048]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(!h.spool.join("junk.ova").exists());
    let (_, listing) = h.call("GET", "/api/vms/import", None).await;
    assert!(listing["imports"].as_array().unwrap().is_empty());

    // A name that is really a path never becomes one.
    let (status, body) = h.upload("..%2F..%2Fetc%2Fcron.ova", vec![0u8; 512]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Committing an archive that is not there is a 404, not a job.
    let (status, _) = h
        .call(
            "POST",
            "/api/vms/import/ghost.ova",
            Some(r#"{"name":"x","disks":[],"nics":[]}"#),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
