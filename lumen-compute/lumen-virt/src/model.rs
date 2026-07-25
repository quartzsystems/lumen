//! What a virtual machine is, in Lumen's terms.
//!
//! This is the whole of the machine's configuration, and it is what
//! [`crate::domain_xml`] renders to a domain document and parses back out of
//! one. There is no database: the hypervisor holds the committed state, and
//! Lumen's own per-machine data — the VMID, the description, the tags — rides
//! along inside the document's `<metadata>` element in a Lumen namespace,
//! which the hypervisor preserves across define and undefine.
//!
//! Every type is `deny_unknown_fields`: a typo in an API request is a 400, not
//! a silently ignored setting.

use serde::{Deserialize, Serialize};

/// VMIDs start here, matching the number every operator who has run a
/// hypervisor before already expects the first machine to get.
pub const FIRST_VMID: u32 = 100;

/// The largest VMID Lumen will allocate. Two bytes, because the generated
/// hardware address encodes the VMID in two of its own.
pub const LAST_VMID: u32 = 65_535;

/// The machine type Lumen defines. Modern chipset, so a guest gets PCIe,
/// working UEFI, and hot-pluggable devices rather than a 1996 southbridge.
pub const DEFAULT_MACHINE: &str = "q35";

pub const DEFAULT_VCPUS: u32 = 1;
pub const DEFAULT_MEMORY_MIB: u64 = 2048;

/// The complete configuration of one machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmConfig {
    pub vmid: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub vcpus: u32,
    pub memory_mib: u64,
    #[serde(default)]
    pub cpu_model: CpuModel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topology: Option<CpuTopology>,
    #[serde(default = "default_machine")]
    pub machine: String,
    #[serde(default)]
    pub firmware: Firmware,
    #[serde(default)]
    pub boot_order: Vec<BootDevice>,
    /// Held by the hypervisor's autostart flag rather than by the document —
    /// so this field is read from and written to that flag, never rendered.
    #[serde(default)]
    pub start_on_boot: bool,
    #[serde(default = "default_true")]
    pub guest_agent: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub disks: Vec<VmDisk>,
    #[serde(default)]
    pub nics: Vec<VmNic>,
}

fn default_machine() -> String {
    DEFAULT_MACHINE.to_string()
}

fn default_true() -> bool {
    true
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            vmid: FIRST_VMID,
            name: String::new(),
            description: None,
            vcpus: DEFAULT_VCPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            cpu_model: CpuModel::default(),
            topology: None,
            machine: default_machine(),
            firmware: Firmware::default(),
            boot_order: vec![BootDevice::Disk],
            start_on_boot: false,
            guest_agent: true,
            tags: Vec::new(),
            disks: Vec::new(),
            nics: Vec::new(),
        }
    }
}

impl VmConfig {
    pub fn disk(&self, id: &str) -> Option<&VmDisk> {
        self.disks.iter().find(|d| d.id == id)
    }

    pub fn nic(&self, id: &str) -> Option<&VmNic> {
        self.nics.iter().find(|n| n.id.eq_ignore_ascii_case(id))
    }

    /// The disk the machine boots off: the lowest boot index, or the first
    /// disk when nothing sets one.
    pub fn boot_disk(&self) -> Option<&VmDisk> {
        self.disks
            .iter()
            .filter(|d| d.boot_index.is_some())
            .min_by_key(|d| d.boot_index)
            .or_else(|| self.disks.first())
    }

    pub fn total_disk_bytes(&self) -> u64 {
        self.disks.iter().map(|d| d.size).sum()
    }

    /// The next free target name for a disk on this bus. Targets must be
    /// unique across the whole machine, so the search looks at every disk and
    /// not just the ones sharing a bus.
    pub fn next_disk_target(&self, bus: DiskBus) -> String {
        let prefix = bus.target_prefix();
        // a..z is 26 disks on one prefix, which is well past anything this
        // stage will define; past that the name simply keeps counting.
        (0u32..)
            .map(|index| format!("{prefix}{}", disk_suffix(index)))
            .find(|candidate| !self.disks.iter().any(|d| &d.id == candidate))
            .expect("an unbounded search always finds a free target")
    }

    /// The next free disk index inside the storage namespace, so
    /// `vm-<vmid>-disk-<n>` never collides with a volume this machine already
    /// has — including after a disk in the middle has been removed.
    pub fn next_disk_index(&self) -> u32 {
        (0u32..)
            .find(|index| {
                let suffix = format!("-disk-{index}");
                !self.disks.iter().any(|d| d.source.ends_with(&suffix))
            })
            .expect("an unbounded search always finds a free index")
    }
}

/// `a`, `b`, … `z`, `aa`, `ab`, … — the way disk targets have always counted.
fn disk_suffix(index: u32) -> String {
    let mut n = index;
    let mut out = String::new();
    loop {
        out.insert(0, (b'a' + (n % 26) as u8) as char);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    out
}

/// How the guest sees the host's processor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CpuModel {
    /// Everything the host CPU can do that can also be migrated. The right
    /// default: full speed, and it survives moving to a similar host.
    #[default]
    HostModel,
    /// Exactly the host CPU, migratable nowhere else.
    HostPassthrough,
    /// A named model, for a cluster of unlike hosts.
    Named(String),
}

impl CpuModel {
    /// The `mode` attribute this model renders as, and the model name when it
    /// needs one.
    pub fn as_parts(&self) -> (&'static str, Option<&str>) {
        match self {
            CpuModel::HostModel => ("host-model", None),
            CpuModel::HostPassthrough => ("host-passthrough", None),
            CpuModel::Named(name) => ("custom", Some(name.as_str())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuTopology {
    pub sockets: u32,
    pub cores: u32,
    pub threads: u32,
}

impl CpuTopology {
    pub fn total(&self) -> u32 {
        self.sockets
            .saturating_mul(self.cores)
            .saturating_mul(self.threads)
    }
}

/// What starts the machine. UEFI by default: it is what a current guest
/// expects, it is what Secure Boot will need, and choosing it at define time
/// is the only time it can be chosen at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Firmware {
    #[default]
    Uefi,
    Bios,
}

impl Firmware {
    pub fn as_str(self) -> &'static str {
        match self {
            Firmware::Uefi => "uefi",
            Firmware::Bios => "bios",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BootDevice {
    Disk,
    Cdrom,
    Network,
}

impl BootDevice {
    /// The word the domain document uses.
    pub fn as_str(self) -> &'static str {
        match self {
            BootDevice::Disk => "hd",
            BootDevice::Cdrom => "cdrom",
            BootDevice::Network => "network",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "hd" => Some(BootDevice::Disk),
            "cdrom" => Some(BootDevice::Cdrom),
            "network" => Some(BootDevice::Network),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskBus {
    /// The fastest path, and the one a current guest already has drivers for.
    #[default]
    VirtioBlk,
    /// Adds discard/TRIM pass-through and more than 26 disks.
    VirtioScsi,
    /// For a guest too old to know about either of the above.
    Sata,
}

impl DiskBus {
    /// The `bus` attribute the document uses.
    pub fn as_str(self) -> &'static str {
        match self {
            DiskBus::VirtioBlk => "virtio",
            DiskBus::VirtioScsi => "scsi",
            DiskBus::Sata => "sata",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "virtio" => Some(DiskBus::VirtioBlk),
            "scsi" => Some(DiskBus::VirtioScsi),
            "sata" => Some(DiskBus::Sata),
            _ => None,
        }
    }

    /// Target names start with this. Both non-virtio buses use `sd`, which is
    /// why [`VmConfig::next_disk_target`] searches every disk rather than only
    /// the ones on the same bus.
    pub fn target_prefix(self) -> &'static str {
        match self {
            DiskBus::VirtioBlk => "vd",
            DiskBus::VirtioScsi | DiskBus::Sata => "sd",
        }
    }
}

/// How the host caches a guest's writes. `None` is the default and the right
/// one for a volume: the host page cache would only buffer the same blocks the
/// guest is already buffering, and a host that loses power mid-write must not
/// have told the guest the write was done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheMode {
    #[default]
    None,
    Writeback,
    Writethrough,
    Directsync,
    Unsafe,
}

impl CacheMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheMode::None => "none",
            CacheMode::Writeback => "writeback",
            CacheMode::Writethrough => "writethrough",
            CacheMode::Directsync => "directsync",
            CacheMode::Unsafe => "unsafe",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(CacheMode::None),
            "writeback" => Some(CacheMode::Writeback),
            "writethrough" => Some(CacheMode::Writethrough),
            "directsync" => Some(CacheMode::Directsync),
            "unsafe" => Some(CacheMode::Unsafe),
            _ => None,
        }
    }
}

/// One disk. The volume underneath it is the truth about its size; `size` is
/// what the machine was told to ask for, and the console shows both when they
/// disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmDisk {
    /// The target name — `vda`, `sdb`. Unique within the machine, stable for
    /// its lifetime, and what the API addresses a disk by.
    pub id: String,
    pub bus: DiskBus,
    /// The block device the volume appears as.
    pub source: String,
    /// Bytes.
    pub size: u64,
    #[serde(default)]
    pub cache: CacheMode,
    #[serde(default = "default_true")]
    pub discard: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_index: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NicModel {
    #[default]
    Virtio,
    E1000e,
    Rtl8139,
}

impl NicModel {
    pub fn as_str(self) -> &'static str {
        match self {
            NicModel::Virtio => "virtio",
            NicModel::E1000e => "e1000e",
            NicModel::Rtl8139 => "rtl8139",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "virtio" => Some(NicModel::Virtio),
            "e1000e" => Some(NicModel::E1000e),
            "rtl8139" => Some(NicModel::Rtl8139),
            _ => None,
        }
    }
}

/// One network adapter, attached to a bridge on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmNic {
    /// The hardware address, which is both the adapter's identity to the guest
    /// and the handle the hypervisor detaches it by — so it is the id.
    pub id: String,
    #[serde(default)]
    pub model: NicModel,
    /// The host bridge this adapter is a port of.
    pub bridge: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlan_tag: Option<u16>,
    /// Where this adapter sits in the boot order. Derived from
    /// [`VmConfig::boot_order`] by [`VmConfig::normalized`] rather than set by
    /// hand — an adapter's place in the order is a property of the machine,
    /// not of the adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_index: Option<u32>,
}

/// A stable hardware address for machine `vmid`'s adapter number `index`.
///
/// `52:54:00` is the prefix every hypervisor on Linux uses, and the low bit of
/// its first byte is set, which is what makes it locally administered — the
/// range reserved for addresses nobody bought. Deriving the rest from the VMID
/// and the adapter index rather than from a random number means the same
/// machine gets the same address every time it is defined, so a reservation on
/// the network keeps working after the machine is rebuilt.
pub fn generate_mac(vmid: u32, index: u32) -> String {
    format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        (vmid >> 8) & 0xff,
        vmid & 0xff,
        index & 0xff
    )
}

/// A usable machine name: 1–63 characters, letters, digits, dot, dash, and
/// underscore. It becomes the domain's name, which appears in paths and in
/// process arguments, so nothing here may be a separator or a shell character.
pub fn valid_vm_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// A tag: short, lower-case-ish, and free of the comma the tag list is joined
/// with.
pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 32
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_addresses_are_stable_and_locally_administered() {
        assert_eq!(generate_mac(101, 0), "52:54:00:00:65:00");
        assert_eq!(generate_mac(101, 1), "52:54:00:00:65:01");
        assert_eq!(generate_mac(300, 0), "52:54:00:01:2c:00");
        // Same machine, same adapter, same address — every time.
        assert_eq!(generate_mac(101, 0), generate_mac(101, 0));
        assert_ne!(generate_mac(101, 0), generate_mac(102, 0));

        // The locally-administered bit is the low bit of the first octet.
        let first = u8::from_str_radix(&generate_mac(101, 0)[0..2], 16).unwrap();
        assert_eq!(
            first & 0b0000_0010,
            0b0000_0010,
            "must be locally administered"
        );
        assert_eq!(first & 0b0000_0001, 0, "must not be a multicast address");
    }

    #[test]
    fn disk_targets_count_the_way_disk_targets_have_always_counted() {
        assert_eq!(disk_suffix(0), "a");
        assert_eq!(disk_suffix(25), "z");
        assert_eq!(disk_suffix(26), "aa");
        assert_eq!(disk_suffix(27), "ab");
    }

    fn disk(id: &str, source: &str) -> VmDisk {
        VmDisk {
            id: id.into(),
            bus: DiskBus::VirtioBlk,
            source: source.into(),
            size: 1024,
            cache: CacheMode::None,
            discard: true,
            boot_index: None,
        }
    }

    #[test]
    fn a_new_target_never_collides_with_one_the_machine_already_has() {
        let mut config = VmConfig::default();
        assert_eq!(config.next_disk_target(DiskBus::VirtioBlk), "vda");
        config
            .disks
            .push(disk("vda", "/dev/zvol/rpool/lumen/vm-101-disk-0"));
        assert_eq!(config.next_disk_target(DiskBus::VirtioBlk), "vdb");
        // A different bus gets its own prefix…
        assert_eq!(config.next_disk_target(DiskBus::VirtioScsi), "sda");
        // …but the two non-virtio buses share one, so they must not collide.
        config
            .disks
            .push(disk("sda", "/dev/zvol/rpool/lumen/vm-101-disk-1"));
        assert_eq!(config.next_disk_target(DiskBus::Sata), "sdb");
    }

    /// Removing the disk in the middle frees its number, and the next disk
    /// takes it — so the names stay dense and a machine cannot end up with a
    /// volume named after a disk it does not have.
    #[test]
    fn a_removed_disk_frees_its_place_in_the_storage_namespace() {
        let mut config = VmConfig::default();
        assert_eq!(config.next_disk_index(), 0);
        config
            .disks
            .push(disk("vda", "/dev/zvol/rpool/lumen/vm-101-disk-0"));
        config
            .disks
            .push(disk("vdb", "/dev/zvol/rpool/lumen/vm-101-disk-1"));
        assert_eq!(config.next_disk_index(), 2);

        config.disks.retain(|d| d.id != "vda");
        assert_eq!(config.next_disk_index(), 0);
    }

    #[test]
    fn the_boot_disk_is_the_lowest_boot_index_or_simply_the_first() {
        let mut config = VmConfig::default();
        assert!(config.boot_disk().is_none());

        config.disks.push(disk("vda", "/a"));
        config.disks.push(disk("vdb", "/b"));
        assert_eq!(config.boot_disk().unwrap().id, "vda");

        config.disks[1].boot_index = Some(1);
        assert_eq!(
            config.boot_disk().unwrap().id,
            "vdb",
            "an explicit boot index wins over position"
        );
    }

    #[test]
    fn names_that_would_become_something_else_on_disk_are_refused() {
        assert!(valid_vm_name("web01"));
        assert!(valid_vm_name("db-primary_2"));
        assert!(!valid_vm_name(""));
        assert!(!valid_vm_name(".."));
        assert!(!valid_vm_name("-rf"));
        assert!(!valid_vm_name("has space"));
        assert!(!valid_vm_name("has/slash"));
        assert!(!valid_vm_name("$(reboot)"));
        assert!(!valid_vm_name(&"x".repeat(64)));

        assert!(valid_tag("production"));
        assert!(!valid_tag("two words"));
        assert!(!valid_tag("a,b"));
        assert!(!valid_tag(""));
    }

    #[test]
    fn a_typo_in_a_request_is_rejected_rather_than_ignored() {
        let err = serde_json::from_str::<VmDisk>(
            r#"{"id":"vda","bus":"virtio-blk","source":"/dev/x","size":1,"discardd":true}"#,
        );
        assert!(err.is_err(), "typo must not be silently ignored");
    }

    #[test]
    fn the_defaults_are_the_machine_an_operator_would_have_asked_for() {
        let config = VmConfig::default();
        assert_eq!(config.machine, "q35");
        assert_eq!(config.firmware, Firmware::Uefi);
        assert_eq!(config.cpu_model, CpuModel::HostModel);
        assert!(config.guest_agent);
        assert_eq!(config.boot_order, vec![BootDevice::Disk]);
        assert_eq!(CacheMode::default(), CacheMode::None);
        assert_eq!(DiskBus::default(), DiskBus::VirtioBlk);
        assert_eq!(NicModel::default(), NicModel::Virtio);
    }

    #[test]
    fn a_topology_multiplies_out_to_the_thread_count() {
        let topology = CpuTopology {
            sockets: 1,
            cores: 4,
            threads: 2,
        };
        assert_eq!(topology.total(), 8);
    }
}
