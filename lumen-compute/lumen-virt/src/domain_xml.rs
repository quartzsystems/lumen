//! The domain document: rendered from a [`VmConfig`], and parsed back into
//! one.
//!
//! Both directions live here and nowhere else. There is no other place in the
//! tree that builds or reads this document, which is what makes the round-trip
//! tests at the bottom of this file meaningful: if a field survives
//! `parse(render(config))`, it survives the hypervisor, because the hypervisor
//! is the only thing in between.
//!
//! ## Why the document is the database
//!
//! The hypervisor already stores a machine's definition durably, hands it back
//! verbatim, and preserves elements it does not understand. Lumen's own
//! per-machine data — the VMID, the description, the tags — therefore lives in
//! a `<metadata>` element in a namespace of ours rather than in a table
//! somewhere that could disagree with it. An operator can read the whole truth
//! with `virsh dumpxml`, and there is nothing to keep in sync.
//!
//! ## Boot order
//!
//! Per-device `<boot order='N'/>` only. The hypervisor rejects mixing that
//! with `<os><boot dev='…'/></os>`, and the per-device form is the one that
//! can express "this disk, then that adapter" rather than "some disk, then
//! some adapter". [`VmConfig::normalized`] is what turns the model's
//! device-class order into those numbers, so rendering is a pure function of a
//! normalized configuration.

use std::fmt::Write as _;

use quick_xml::escape::escape;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::{Result, VirtError};
use crate::model::{
    BootDevice, CacheMode, CpuModel, CpuTopology, DiskBus, Firmware, NicModel, VideoModel, VmCdrom,
    VmConfig, VmDisk, VmNic, CDROM_BUS,
};

/// The namespace Lumen's own per-machine data lives in. Versioned in the URI
/// so a later shape can be told from this one by anything reading the document
/// — including a human with `virsh dumpxml`.
pub const LUMEN_NS: &str = "https://www.quartzsystems.net/xmlns/lumen/1.0";

/// libosinfo's namespace, which is not ours and is not versioned by us. The
/// guest operating system is recorded here as well as in Lumen's own element,
/// so every tool that reads a domain document sees the same answer.
pub const OSINFO_NS: &str = "http://libosinfo.org/xmlns/libvirt/domain/1.0";

/// The channel name a guest agent is reached on. Fixed by the agent itself.
const GUEST_AGENT_CHANNEL: &str = "org.qemu.guest_agent.0";

/// The channel a guest's clipboard agent is reached on.
///
/// The name is SPICE's and looks out of place on a VNC machine, but it is what
/// `spice-vdagent` binds to inside the guest and it is not ours to choose.
/// QEMU's `qemu-vdagent` backend speaks the same protocol over it and hands the
/// clipboard to the VNC server, which is how a console with no SPICE in it
/// anywhere ends up with copy and paste.
const CLIPBOARD_CHANNEL: &str = "com.redhat.spice.0";

/// Where a machine's console socket lives. Predictable rather than assigned by
/// the hypervisor, so the console viewer can find it without a lookup, and so a
/// policy rule can name it.
pub fn vnc_socket_path(vmid: u32) -> String {
    format!("/var/lib/libvirt/qemu/lumen-{vmid}-vnc.sock")
}

/// Where a machine's console socket actually is, according to its own document.
///
/// [`vnc_socket_path`] is where Lumen *puts* one, and for a machine this
/// appliance defined the two agree. They need not: a domain someone edited with
/// `virsh`, or one defined by an older version of this appliance, carries
/// whatever it carries — and a viewer has to connect to the socket the
/// hypervisor is listening on rather than to the one we would have chosen.
///
/// The path may sit on the `<graphics>` element, on the `<listen>` child the
/// hypervisor adds when it hands the document back, or on both. The child wins,
/// because it is the hypervisor's own answer rather than the one it was given.
pub fn vnc_socket_of(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut path: Vec<String> = Vec::new();
    let mut in_vnc = false;
    let mut on_element: Option<String> = None;
    let mut on_listen: Option<String> = None;

    loop {
        let Ok(event) = reader.read_event() else {
            break;
        };
        match event {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => {
                path.push(local_name(e.name().as_ref()));
                match path.join("/").as_str() {
                    "domain/devices/graphics" => {
                        // A machine may carry more than one graphics device;
                        // only the VNC one has a socket this speaks to.
                        in_vnc = attr(e, "type").as_deref() == Some("vnc");
                        if in_vnc {
                            on_element = attr(e, "socket");
                        }
                    }
                    "domain/devices/graphics/listen" if in_vnc => {
                        if attr(e, "type").as_deref() == Some("socket") {
                            on_listen = attr(e, "socket");
                        }
                    }
                    _ => {}
                }
                if matches!(event, Event::Empty(_)) {
                    path.pop();
                }
            }
            Event::End(_) => {
                if path.join("/") == "domain/devices/graphics" && in_vnc {
                    in_vnc = false;
                }
                path.pop();
            }
            _ => {}
        }
        if on_listen.is_some() {
            break;
        }
    }

    on_listen
        .or(on_element)
        .filter(|socket| !socket.trim().is_empty())
}

/// Whether this document gives the machine a screen at all.
///
/// The distinction [`VmConfig::video`] cannot make. A document with no
/// `<video>` in it reads back as the default card — see the note on
/// `config.video` in [`read`] — so a machine that predates consoles and a
/// machine deliberately set to virtio-gpu are the same value by the time
/// anything sees a `VmConfig`. That is fine for rendering, where the default
/// is what the next save will write, and wrong everywhere an operator is being
/// told what the machine *has*: the console page showed "VirtIO GPU" for a
/// machine with no display device and no VNC server on it, which reads as a
/// broken console rather than as a machine that needs saving.
///
/// So this asks the document rather than the parsed configuration, and it asks
/// about `<graphics>` rather than `<video>` — the VNC server is the half the
/// viewer connects to, and it is the half [`vnc_socket_of`] has to find.
pub fn has_screen(xml: &str) -> bool {
    vnc_socket_of(xml).is_some()
}

/// Everything the document carries, on the way back in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDomain {
    /// The machine's configuration. `start_on_boot` is always false here: it
    /// is the hypervisor's autostart flag, not part of the document, and the
    /// service fills it in from there.
    pub config: VmConfig,
    pub uuid: Option<String>,
    /// When the machine was started, seconds since the epoch. Present only on
    /// the live document of a running machine — see
    /// [`live_metadata`].
    pub started_at: Option<u64>,
}

// --- rendering ---------------------------------------------------------------

fn text(value: &str) -> String {
    escape(value).into_owned()
}

/// Render `config` as a domain document.
///
/// Normalizes first, so the boot numbers in the output are a function of the
/// configuration alone and two equal configurations always render identically.
pub fn render(config: &VmConfig) -> String {
    let config = config.normalized();
    let mut out = String::with_capacity(2048);

    out.push_str("<domain type='kvm'>\n");
    let _ = writeln!(out, "  <name>{}</name>", text(&config.name));
    render_metadata(&mut out, &config, None);

    let _ = writeln!(out, "  <memory unit='MiB'>{}</memory>", config.memory_mib);
    let _ = writeln!(
        out,
        "  <currentMemory unit='MiB'>{}</currentMemory>",
        config.memory_mib
    );
    let _ = writeln!(
        out,
        "  <vcpu placement='static'>{}</vcpu>",
        config.vcpus.max(1)
    );
    render_cpu(&mut out, &config);
    render_os(&mut out, &config);

    // Without <acpi/> the guest never hears a shutdown request, so an ACPI
    // shutdown silently does nothing. It is not optional.
    out.push_str("  <features>\n    <acpi/>\n    <apic/>\n  </features>\n");
    out.push_str("  <clock offset='utc'/>\n");
    out.push_str("  <on_poweroff>destroy</on_poweroff>\n");
    out.push_str("  <on_reboot>restart</on_reboot>\n");
    out.push_str("  <on_crash>destroy</on_crash>\n");

    out.push_str("  <devices>\n");
    if config.disks.iter().any(|d| d.bus == DiskBus::VirtioScsi) {
        out.push_str("    <controller type='scsi' index='0' model='virtio-scsi'/>\n");
    }
    for disk in &config.disks {
        render_disk(&mut out, disk);
    }
    for cdrom in &config.cdroms {
        render_cdrom(&mut out, cdrom);
    }
    for nic in &config.nics {
        render_nic(&mut out, nic);
    }
    if config.guest_agent {
        out.push_str("    <channel type='unix'>\n");
        out.push_str("      <source mode='bind'/>\n");
        let _ = writeln!(
            out,
            "      <target type='virtio' name='{GUEST_AGENT_CHANNEL}'/>"
        );
        out.push_str("    </channel>\n");
    }
    // The clipboard. Unconditional, like the console it belongs to: it is not a
    // choice an operator would know how to make, and a machine that turns out
    // not to have `spice-vdagent` in it has an idle virtio port rather than a
    // problem — the channel simply never connects.
    //
    // `mouse mode='server'` is deliberate. The agent can also own the pointer,
    // which gets absolute positioning without a tablet device, but it means a
    // guest that has not started its agent yet — an installer, a machine at a
    // firmware prompt, exactly when the console matters most — has no working
    // mouse at all. The clipboard is worth having; the pointer is already
    // solved.
    out.push_str("    <channel type='qemu-vdagent'>\n");
    out.push_str("      <source>\n");
    out.push_str("        <clipboard copypaste='yes'/>\n");
    out.push_str("        <mouse mode='server'/>\n");
    out.push_str("      </source>\n");
    let _ = writeln!(
        out,
        "      <target type='virtio' name='{CLIPBOARD_CHANNEL}'/>"
    );
    out.push_str("    </channel>\n");
    // Defined from the very first machine even though the console arrives in
    // part 2: adding it later would mean redefining every domain that already
    // exists, and a redefine is the one operation an operator has no reason to
    // expect.
    let _ = writeln!(
        out,
        "    <graphics type='vnc' socket='{}'/>",
        text(&vnc_socket_path(config.vmid))
    );
    let _ = writeln!(
        out,
        "    <video>\n      <model type='{}'/>\n    </video>",
        config.video.as_str()
    );
    out.push_str(
        "    <console type='pty'>\n      <target type='serial' port='0'/>\n    </console>\n",
    );
    out.push_str("    <memballoon model='virtio'/>\n");
    out.push_str("    <rng model='virtio'>\n      <backend model='random'>/dev/urandom</backend>\n    </rng>\n");
    out.push_str("  </devices>\n");
    out.push_str("</domain>\n");
    out
}

fn render_metadata(out: &mut String, config: &VmConfig, started_at: Option<u64>) {
    out.push_str("  <metadata>\n");
    let _ = writeln!(out, "    <lumen:vm xmlns:lumen='{LUMEN_NS}'>");
    let _ = writeln!(out, "      <lumen:vmid>{}</lumen:vmid>", config.vmid);
    if let Some(description) = config.description.as_deref().filter(|d| !d.is_empty()) {
        let _ = writeln!(
            out,
            "      <lumen:description>{}</lumen:description>",
            text(description)
        );
    }
    if !config.tags.is_empty() {
        let _ = writeln!(
            out,
            "      <lumen:tags>{}</lumen:tags>",
            text(&config.tags.join(","))
        );
    }
    if let Some(started) = started_at {
        let _ = writeln!(out, "      <lumen:started>{started}</lumen:started>");
    }
    out.push_str("    </lumen:vm>\n");
    // The same answer in libosinfo's namespace as well as ours. It costs one
    // element and it is what makes `virt-manager` and `virt-install` show the
    // guest this machine was built for instead of "unknown" — the whole point
    // of recording an operator's choice in a shared vocabulary.
    if let Some(os_id) = config.os_id.as_deref().filter(|id| !id.is_empty()) {
        let _ = writeln!(
            out,
            "    <libosinfo:libosinfo xmlns:libosinfo='{OSINFO_NS}'>"
        );
        let _ = writeln!(out, "      <libosinfo:os id='{}'/>", text(os_id));
        out.push_str("    </libosinfo:libosinfo>\n");
    }
    out.push_str("  </metadata>\n");
}

/// The metadata element on its own, for the running machine only.
///
/// The hypervisor keeps live-only metadata for exactly as long as the machine
/// is up, which is what an uptime needs: it survives a control-plane restart
/// (the hypervisor is holding it, not us) and it disappears by itself when the
/// machine stops, so there is no stale start time to clean up.
///
/// ## No prefix, and no declaration — unlike [`render_metadata`]
///
/// Inside a document the element is written `<lumen:vm xmlns:lumen='…'>`,
/// because there it has to carry its own namespace. Here it must not, and the
/// difference is not cosmetic: `virDomainSetMetadata` takes the namespace as
/// *arguments* — the prefix and the URI — copies this element, and then calls
/// `xmlNewNs` on the copy to attach them. libxml2 refuses to declare a prefix
/// that the node already declares and answers with NULL, which libvirt reports
/// as `internal error: failed to create a new XML namespace`. So a metadata
/// element that declares the namespace itself can never be set, on any machine,
/// ever — the start time simply never records.
///
/// Reading it back is unaffected either way: [`parse`] matches on local names,
/// so `vm/started` and `lumen:vm/lumen:started` are the same path to it.
pub fn live_metadata(config: &VmConfig, started_at: u64) -> String {
    let mut out = String::new();
    out.push_str("<vm>\n");
    let _ = writeln!(out, "  <vmid>{}</vmid>", config.vmid);
    if let Some(description) = config.description.as_deref().filter(|d| !d.is_empty()) {
        let _ = writeln!(out, "  <description>{}</description>", text(description));
    }
    if !config.tags.is_empty() {
        let _ = writeln!(out, "  <tags>{}</tags>", text(&config.tags.join(",")));
    }
    let _ = writeln!(out, "  <started>{started_at}</started>");
    out.push_str("</vm>");
    out
}

fn render_cpu(out: &mut String, config: &VmConfig) {
    let (mode, named) = config.cpu_model.as_parts();
    let has_body = named.is_some() || config.topology.is_some();
    if !has_body {
        let _ = writeln!(out, "  <cpu mode='{mode}'/>");
        return;
    }
    let _ = writeln!(out, "  <cpu mode='{mode}'>");
    if let Some(name) = named {
        let _ = writeln!(out, "    <model fallback='allow'>{}</model>", text(name));
    }
    if let Some(CpuTopology {
        sockets,
        cores,
        threads,
    }) = config.topology
    {
        let _ = writeln!(
            out,
            "    <topology sockets='{sockets}' cores='{cores}' threads='{threads}'/>"
        );
    }
    out.push_str("  </cpu>\n");
}

fn render_os(out: &mut String, config: &VmConfig) {
    // firmware='efi' is the hypervisor's own firmware selection: it picks a
    // matching image and manages the variable store, rather than us naming a
    // path that differs between distributions.
    match config.firmware {
        Firmware::Uefi => out.push_str("  <os firmware='efi'>\n"),
        Firmware::Bios => out.push_str("  <os>\n"),
    }
    let _ = writeln!(
        out,
        "    <type arch='x86_64' machine='{}'>hvm</type>",
        text(&config.machine)
    );
    out.push_str("  </os>\n");
}

fn render_disk(out: &mut String, disk: &VmDisk) {
    out.push_str("    <disk type='block' device='disk'>\n");
    let discard = if disk.discard { " discard='unmap'" } else { "" };
    let _ = writeln!(
        out,
        "      <driver name='qemu' type='raw' cache='{}'{discard}/>",
        disk.cache.as_str()
    );
    let _ = writeln!(out, "      <source dev='{}'/>", text(&disk.source));
    let _ = writeln!(
        out,
        "      <target dev='{}' bus='{}'/>",
        text(&disk.id),
        disk.bus.as_str()
    );
    if let Some(order) = disk.boot_index {
        let _ = writeln!(out, "      <boot order='{order}'/>");
    }
    out.push_str("    </disk>\n");
}

/// An optical drive. `type='file'` with no `<source>` at all is how an empty
/// drive is written — a `<source file=''/>` is not an empty tray, it is a
/// document the hypervisor rejects.
fn render_cdrom(out: &mut String, cdrom: &VmCdrom) {
    out.push_str("    <disk type='file' device='cdrom'>\n");
    out.push_str("      <driver name='qemu' type='raw'/>\n");
    if let Some(source) = cdrom.source.as_deref().filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "      <source file='{}'/>", text(source));
    }
    let _ = writeln!(
        out,
        "      <target dev='{}' bus='{}'/>",
        text(&cdrom.id),
        CDROM_BUS.as_str()
    );
    if let Some(order) = cdrom.boot_index {
        let _ = writeln!(out, "      <boot order='{order}'/>");
    }
    // A guest must not be able to write to its own installation media.
    out.push_str("      <readonly/>\n");
    out.push_str("    </disk>\n");
}

fn render_nic(out: &mut String, nic: &VmNic) {
    out.push_str("    <interface type='bridge'>\n");
    let _ = writeln!(out, "      <mac address='{}'/>", text(&nic.id));
    let _ = writeln!(out, "      <source bridge='{}'/>", text(&nic.bridge));
    let _ = writeln!(out, "      <model type='{}'/>", nic.model.as_str());
    if let Some(tag) = nic.vlan_tag {
        let _ = writeln!(
            out,
            "      <vlan>\n        <tag id='{tag}'/>\n      </vlan>"
        );
    }
    if let Some(order) = nic.boot_index {
        let _ = writeln!(out, "      <boot order='{order}'/>");
    }
    out.push_str("    </interface>\n");
}

/// The document fragment for one disk, for attach and detach. The hypervisor
/// matches a detach on the target name, so the same fragment serves both.
pub fn disk_fragment(disk: &VmDisk) -> String {
    let mut out = String::new();
    render_disk(&mut out, disk);
    out
}

pub fn cdrom_fragment(cdrom: &VmCdrom) -> String {
    let mut out = String::new();
    render_cdrom(&mut out, cdrom);
    out
}

pub fn nic_fragment(nic: &VmNic) -> String {
    let mut out = String::new();
    render_nic(&mut out, nic);
    out
}

// --- parsing -----------------------------------------------------------------

#[derive(Default)]
struct PartialDisk {
    target: Option<String>,
    bus: Option<DiskBus>,
    source: Option<String>,
    cache: Option<CacheMode>,
    discard: bool,
    boot_index: Option<u32>,
    is_disk: bool,
    is_cdrom: bool,
}

#[derive(Default)]
struct PartialNic {
    mac: Option<String>,
    bridge: Option<String>,
    model: Option<NicModel>,
    vlan_tag: Option<u16>,
    boot_index: Option<u32>,
    is_bridge: bool,
}

/// The parts of a domain document that any domain has, Lumen's or not.
///
/// A node may hold machines somebody defined with the hypervisor's own tools.
/// Those are not Lumen's to manage, but they are still on the node — they own
/// their name and they use its memory — so there has to be a way to read the
/// little that is true of every domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainSummary {
    pub name: String,
    pub memory_mib: u64,
    pub vcpus: u32,
    /// Present only when the document carries a Lumen identifier.
    pub vmid: Option<u32>,
}

/// Read a domain document back into a configuration.
///
/// Tolerant by design: the hypervisor adds elements of its own on the way
/// through (firmware image paths, addresses it assigned, the live state of a
/// running machine), and anything not named here is simply not looked at. What
/// it will not do is guess — a document with no VMID in it is not a Lumen
/// machine, and says so. Use [`summarize`] for the parts every domain has.
pub fn parse(xml: &str) -> Result<ParsedDomain> {
    let (parsed, vmid) = read(xml)?;
    let Some(vmid) = vmid else {
        return Err(VirtError::Conflict(format!(
            "\"{}\" is a machine this appliance did not define — it carries no Lumen identifier, \
             so it is not managed here.",
            parsed.config.name
        )));
    };
    Ok(ParsedDomain {
        config: VmConfig {
            vmid,
            ..parsed.config
        },
        ..parsed
    })
}

/// The name, size, and identifier of any domain document.
pub fn summarize(xml: &str) -> Result<DomainSummary> {
    let (parsed, vmid) = read(xml)?;
    Ok(DomainSummary {
        name: parsed.config.name,
        memory_mib: parsed.config.memory_mib,
        vcpus: parsed.config.vcpus,
        vmid,
    })
}

/// The one reader. Returns the identifier separately, because whether a
/// document has one is the difference between a machine Lumen manages and a
/// machine that merely lives on the same node.
fn read(xml: &str) -> Result<(ParsedDomain, Option<u32>)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut path: Vec<String> = Vec::new();
    let mut config = VmConfig {
        // Overwritten below; a document with no boot elements really does have
        // an empty boot order.
        boot_order: Vec::new(),
        guest_agent: false,
        ..VmConfig::default()
    };
    let mut uuid: Option<String> = None;
    let mut started_at: Option<u64> = None;
    let mut vmid: Option<u32> = None;
    let mut memory: Option<u64> = None;
    let mut memory_unit = String::from("KiB");
    let mut cpu_mode: Option<String> = None;
    let mut cpu_named: Option<String> = None;
    let mut firmware = Firmware::Bios;
    // Optional rather than defaulted, so "the first one wins" is expressible:
    // a machine may carry several <video> devices, and the extra ones are more
    // heads on the same machine rather than a second opinion about the card.
    let mut video: Option<VideoModel> = None;

    let mut disk = PartialDisk::default();
    let mut nic = PartialNic::default();
    let mut disks: Vec<(VmDisk, Option<u32>)> = Vec::new();
    let mut cdroms: Vec<(VmCdrom, Option<u32>)> = Vec::new();
    let mut nics: Vec<(VmNic, Option<u32>)> = Vec::new();

    loop {
        let event = reader.read_event().map_err(|err| {
            VirtError::Backend(anyhow::anyhow!("unreadable machine document: {err}"))
        })?;

        match event {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => {
                let name = local_name(e.name().as_ref());
                path.push(name.clone());
                let here = path.join("/");

                match here.as_str() {
                    "domain/memory" => memory_unit = attr(e, "unit").unwrap_or(memory_unit.clone()),
                    "domain/os" => {
                        firmware = match attr(e, "firmware").as_deref() {
                            Some("efi") => Firmware::Uefi,
                            _ => Firmware::Bios,
                        }
                    }
                    "domain/os/type" => {
                        if let Some(machine) = attr(e, "machine") {
                            config.machine = machine;
                        }
                    }
                    "domain/cpu" => cpu_mode = attr(e, "mode"),
                    "domain/cpu/topology" => {
                        config.topology = Some(CpuTopology {
                            sockets: number(attr(e, "sockets")).unwrap_or(1) as u32,
                            cores: number(attr(e, "cores")).unwrap_or(1) as u32,
                            threads: number(attr(e, "threads")).unwrap_or(1) as u32,
                        })
                    }
                    "domain/devices/disk" => {
                        // A disk and an optical drive are the same element
                        // with a different `device`; anything else (a floppy,
                        // a LUN) is a device this appliance does not define
                        // and does not claim to understand.
                        let device = attr(e, "device");
                        disk = PartialDisk {
                            is_disk: device.as_deref() == Some("disk"),
                            is_cdrom: device.as_deref() == Some("cdrom"),
                            ..PartialDisk::default()
                        }
                    }
                    "domain/devices/disk/driver" => {
                        disk.cache = attr(e, "cache").as_deref().and_then(CacheMode::parse);
                        disk.discard = attr(e, "discard").as_deref() == Some("unmap");
                    }
                    "domain/devices/disk/source" => {
                        // A block-backed disk names a device; a file-backed one
                        // names a file. Either is the source as far as the
                        // console is concerned.
                        disk.source = attr(e, "dev").or_else(|| attr(e, "file"));
                    }
                    "domain/devices/disk/target" => {
                        disk.target = attr(e, "dev");
                        disk.bus = attr(e, "bus").as_deref().and_then(DiskBus::parse);
                    }
                    "domain/devices/disk/boot" => {
                        disk.boot_index = number(attr(e, "order")).map(|v| v as u32)
                    }
                    "domain/devices/video/model" => {
                        if video.is_none() {
                            video = attr(e, "type").as_deref().and_then(VideoModel::parse);
                        }
                    }
                    "domain/devices/interface" => {
                        nic = PartialNic {
                            is_bridge: attr(e, "type").as_deref() == Some("bridge"),
                            ..PartialNic::default()
                        }
                    }
                    "domain/devices/interface/mac" => nic.mac = attr(e, "address"),
                    "domain/devices/interface/source" => nic.bridge = attr(e, "bridge"),
                    "domain/devices/interface/model" => {
                        nic.model = attr(e, "type").as_deref().and_then(NicModel::parse)
                    }
                    "domain/devices/interface/vlan/tag" => {
                        nic.vlan_tag = number(attr(e, "id")).map(|v| v as u16)
                    }
                    "domain/devices/interface/boot" => {
                        nic.boot_index = number(attr(e, "order")).map(|v| v as u32)
                    }
                    // Read from libosinfo's element, not Lumen's: it is the
                    // shared vocabulary, and a machine defined by another tool
                    // carries it too.
                    "domain/metadata/libosinfo/os" => config.os_id = attr(e, "id"),
                    "domain/devices/channel/target" => {
                        if attr(e, "name").as_deref() == Some(GUEST_AGENT_CHANNEL) {
                            config.guest_agent = true;
                        }
                    }
                    _ => {}
                }

                // An empty element never produces an End, so it leaves the
                // path itself.
                if matches!(event, Event::Empty(_)) {
                    path.pop();
                }
            }
            Event::End(_) => {
                let here = path.join("/");
                match here.as_str() {
                    "domain/devices/disk" => {
                        if let (true, Some(target), Some(source)) =
                            (disk.is_disk, disk.target.clone(), disk.source.clone())
                        {
                            disks.push((
                                VmDisk {
                                    id: target,
                                    bus: disk.bus.unwrap_or_default(),
                                    source,
                                    // The volume underneath is the authority on
                                    // size; the document does not carry one for
                                    // a block-backed disk, and inventing a
                                    // number here would be a number that is
                                    // wrong.
                                    size: 0,
                                    cache: disk.cache.unwrap_or_default(),
                                    discard: disk.discard,
                                    boot_index: disk.boot_index,
                                },
                                disk.boot_index,
                            ));
                        } else if disk.is_cdrom {
                            // A drive with nothing in it is still a drive, so
                            // unlike a disk it needs no source to be real —
                            // only a target to be addressed by.
                            if let Some(target) = disk.target.clone() {
                                cdroms.push((
                                    VmCdrom {
                                        id: target,
                                        source: disk.source.clone().filter(|s| !s.is_empty()),
                                        boot_index: disk.boot_index,
                                    },
                                    disk.boot_index,
                                ));
                            }
                        }
                        disk = PartialDisk::default();
                    }
                    "domain/devices/interface" => {
                        if let (true, Some(mac), Some(bridge)) =
                            (nic.is_bridge, nic.mac.clone(), nic.bridge.clone())
                        {
                            nics.push((
                                VmNic {
                                    id: mac,
                                    model: nic.model.unwrap_or_default(),
                                    bridge,
                                    vlan_tag: nic.vlan_tag,
                                    boot_index: nic.boot_index,
                                },
                                nic.boot_index,
                            ));
                        }
                        nic = PartialNic::default();
                    }
                    _ => {}
                }
                path.pop();
            }
            Event::Text(ref t) => {
                let value = t
                    .unescape()
                    .map_err(|err| {
                        VirtError::Backend(anyhow::anyhow!("unreadable machine document: {err}"))
                    })?
                    .into_owned();
                let here = path.join("/");
                match here.as_str() {
                    "domain/name" => config.name = value,
                    "domain/uuid" => uuid = Some(value),
                    "domain/memory" => memory = value.parse().ok(),
                    "domain/vcpu" => {
                        if let Ok(count) = value.parse() {
                            config.vcpus = count;
                        }
                    }
                    "domain/cpu/model" => cpu_named = Some(value),
                    "domain/metadata/vm/vmid" => vmid = value.parse().ok(),
                    "domain/metadata/vm/description" => config.description = Some(value),
                    "domain/metadata/vm/tags" => {
                        config.tags = value
                            .split(',')
                            .map(str::trim)
                            .filter(|t| !t.is_empty())
                            .map(String::from)
                            .collect()
                    }
                    "domain/metadata/vm/started" => started_at = value.parse().ok(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    config.memory_mib = to_mib(memory.unwrap_or(0), &memory_unit);
    config.firmware = firmware;
    // A machine defined before this appliance put a screen on one has no
    // <video> at all. The default is the honest answer to "what card does it
    // have" only once it has been saved again — which is precisely what
    // `VirtService::console` tells an operator to do.
    config.video = video.unwrap_or_default();
    config.cpu_model = match (cpu_mode.as_deref(), cpu_named) {
        (Some("host-passthrough"), _) => CpuModel::HostPassthrough,
        (Some("custom"), Some(name)) => CpuModel::Named(name),
        _ => CpuModel::HostModel,
    };

    // Boot order comes back out of the numbers on the devices themselves: the
    // device classes in the order their lowest-numbered member boots.
    let mut ordered: Vec<(u32, BootDevice)> = disks
        .iter()
        .filter_map(|(_, order)| order.map(|o| (o, BootDevice::Disk)))
        .chain(
            cdroms
                .iter()
                .filter_map(|(_, order)| order.map(|o| (o, BootDevice::Cdrom))),
        )
        .chain(
            nics.iter()
                .filter_map(|(_, order)| order.map(|o| (o, BootDevice::Network))),
        )
        .collect();
    ordered.sort_by_key(|(order, _)| *order);
    for (_, class) in ordered {
        if !config.boot_order.contains(&class) {
            config.boot_order.push(class);
        }
    }

    config.disks = disks.into_iter().map(|(disk, _)| disk).collect();
    config.cdroms = cdroms.into_iter().map(|(cdrom, _)| cdrom).collect();
    config.nics = nics.into_iter().map(|(nic, _)| nic).collect();

    Ok((
        ParsedDomain {
            config,
            uuid,
            started_at,
        },
        vmid,
    ))
}

/// The start time out of a bare metadata element, as the hypervisor hands it
/// back on its own rather than inside a document.
///
/// Wrapping it and reusing the one reader keeps a single parser in the tree: a
/// change to how the metadata is written cannot make this and the document
/// disagree.
///
/// [`read`] rather than [`parse`], and the difference is not cosmetic: `parse`
/// refuses a document with no VMID in it, because a *domain* without one is not
/// a machine this appliance manages. That rule has no meaning here. This is one
/// element, handed back by the hypervisor for a machine the caller already
/// named, and asking it to prove its identity again would mean an uptime that
/// silently reads as absent whenever the element happens not to carry one.
pub fn started_from_metadata(metadata_xml: &str) -> Option<u64> {
    let wrapped =
        format!("<domain type='kvm'><name/><metadata>{metadata_xml}</metadata><devices/></domain>");
    read(&wrapped)
        .ok()
        .and_then(|(parsed, _)| parsed.started_at)
}

/// Local name of a possibly-namespaced element: `lumen:vmid` is `vmid`. The
/// prefix a document uses for our namespace is not ours to depend on — the
/// hypervisor is free to rewrite it.
fn local_name(raw: &[u8]) -> String {
    let name = String::from_utf8_lossy(raw);
    match name.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => name.into_owned(),
    }
}

fn attr(element: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    element.attributes().flatten().find_map(|a| {
        (local_name(a.key.as_ref()) == name)
            .then(|| a.unescape_value().ok().map(|v| v.into_owned()))
            .flatten()
    })
}

fn number(value: Option<String>) -> Option<u64> {
    value.and_then(|v| v.trim().parse().ok())
}

/// The hypervisor normalizes memory to KiB on the way back out, whatever unit
/// it was given, so the reader has to handle every unit it might see.
fn to_mib(value: u64, unit: &str) -> u64 {
    match unit {
        "b" | "bytes" => value / (1024 * 1024),
        "KiB" | "k" | "KB" => value / 1024,
        "MiB" | "M" | "MB" => value,
        "GiB" | "G" | "GB" => value * 1024,
        "TiB" | "T" | "TB" => value * 1024 * 1024,
        _ => value / 1024,
    }
}

impl VmConfig {
    /// The configuration as it will actually be rendered: boot numbers filled
    /// in from the device-class order, and boot entries for device classes the
    /// machine does not have dropped.
    ///
    /// Rendering normalizes, so `parse(render(c)) == c.normalized()` for every
    /// configuration — which is what the round-trip tests assert.
    pub fn normalized(&self) -> VmConfig {
        let mut config = self.clone();
        let mut order = 1u32;

        // Devices keep whatever relative order they were given: an explicit
        // boot index first, then position, so re-normalizing is idempotent.
        let mut disk_order: Vec<usize> = (0..config.disks.len()).collect();
        disk_order.sort_by_key(|&i| (config.disks[i].boot_index.unwrap_or(u32::MAX), i));
        let mut cdrom_order: Vec<usize> = (0..config.cdroms.len()).collect();
        cdrom_order.sort_by_key(|&i| (config.cdroms[i].boot_index.unwrap_or(u32::MAX), i));
        let mut nic_order: Vec<usize> = (0..config.nics.len()).collect();
        nic_order.sort_by_key(|&i| (config.nics[i].boot_index.unwrap_or(u32::MAX), i));

        let mut boot_order: Vec<BootDevice> = Vec::new();
        for disk in &mut config.disks {
            disk.boot_index = None;
        }
        for cdrom in &mut config.cdroms {
            cdrom.boot_index = None;
        }
        for nic in &mut config.nics {
            nic.boot_index = None;
        }
        for class in &self.boot_order {
            match class {
                BootDevice::Disk => {
                    if config.disks.is_empty() {
                        continue;
                    }
                    for &index in &disk_order {
                        config.disks[index].boot_index = Some(order);
                        order += 1;
                    }
                    boot_order.push(BootDevice::Disk);
                }
                BootDevice::Network => {
                    if config.nics.is_empty() {
                        continue;
                    }
                    for &index in &nic_order {
                        config.nics[index].boot_index = Some(order);
                        order += 1;
                    }
                    boot_order.push(BootDevice::Network);
                }
                BootDevice::Cdrom => {
                    // An entry for a device class the machine does not have is
                    // not a setting — it is a line with nothing behind it, and
                    // the document would not carry it either.
                    if config.cdroms.is_empty() {
                        continue;
                    }
                    for &index in &cdrom_order {
                        config.cdroms[index].boot_index = Some(order);
                        order += 1;
                    }
                    boot_order.push(BootDevice::Cdrom);
                }
            }
        }
        config.boot_order = boot_order;
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{generate_mac, VmDisk, VmNic};

    fn disk(id: &str, source: &str, bus: DiskBus) -> VmDisk {
        VmDisk {
            id: id.into(),
            bus,
            source: source.into(),
            size: 0,
            cache: CacheMode::None,
            discard: true,
            boot_index: None,
        }
    }

    fn cdrom(id: &str, source: Option<&str>) -> VmCdrom {
        VmCdrom {
            id: id.into(),
            source: source.map(String::from),
            boot_index: None,
        }
    }

    fn sample() -> VmConfig {
        VmConfig {
            vmid: 101,
            name: "web01".into(),
            description: Some("Public web server".into()),
            vcpus: 4,
            memory_mib: 8192,
            cpu_model: CpuModel::HostModel,
            topology: Some(CpuTopology {
                sockets: 1,
                cores: 4,
                threads: 1,
            }),
            machine: "q35".into(),
            firmware: Firmware::Uefi,
            video: VideoModel::Virtio,
            boot_order: vec![BootDevice::Disk, BootDevice::Network],
            start_on_boot: false,
            guest_agent: true,
            tags: vec!["production".into(), "web".into()],
            os_id: Some("http://almalinux.org/almalinux/10".into()),
            disks: vec![disk(
                "vda",
                "/dev/zvol/boot/lumen/vm-101-disk-0",
                DiskBus::VirtioBlk,
            )],
            cdroms: Vec::new(),
            nics: vec![VmNic {
                id: generate_mac(101, 0),
                model: NicModel::Virtio,
                bridge: "br0".into(),
                vlan_tag: None,
                boot_index: None,
            }],
        }
    }

    /// The test this whole module exists for: build a configuration, render
    /// it, read it back, and get the same configuration.
    #[test]
    fn a_configuration_survives_the_round_trip() {
        let config = sample();
        let parsed = parse(&render(&config)).expect("the document parses");
        assert_eq!(parsed.config, config.normalized());
        assert_eq!(parsed.started_at, None);
    }

    #[test]
    fn every_variation_of_every_field_survives_the_round_trip() {
        /// A named perturbation of the healthy sample. Same shape as the
        /// validator's table-driven cases.
        type Case = (&'static str, fn(&mut VmConfig));

        let cases: Vec<Case> = vec![
            ("legacy firmware", |c| c.firmware = Firmware::Bios),
            ("host passthrough", |c| {
                c.cpu_model = CpuModel::HostPassthrough
            }),
            ("a named processor", |c| {
                c.cpu_model = CpuModel::Named("Skylake-Server".into())
            }),
            ("no topology", |c| c.topology = None),
            ("no guest agent", |c| c.guest_agent = false),
            ("no description or tags", |c| {
                c.description = None;
                c.tags = Vec::new();
            }),
            ("several disks on several buses", |c| {
                c.disks.push(disk(
                    "sda",
                    "/dev/zvol/boot/lumen/vm-101-disk-1",
                    DiskBus::VirtioScsi,
                ));
                c.disks.push(disk(
                    "sdb",
                    "/dev/zvol/boot/lumen/vm-101-disk-2",
                    DiskBus::Sata,
                ));
            }),
            ("a cache mode that is not the default", |c| {
                c.disks[0].cache = CacheMode::Writeback;
                c.disks[0].discard = false;
            }),
            ("a tagged adapter", |c| c.nics[0].vlan_tag = Some(100)),
            ("an older adapter model", |c| {
                c.nics[0].model = NicModel::E1000e
            }),
            ("two adapters", |c| {
                c.nics.push(VmNic {
                    id: generate_mac(101, 1),
                    model: NicModel::Virtio,
                    bridge: "br1".into(),
                    vlan_tag: Some(20),
                    boot_index: None,
                })
            }),
            ("network first", |c| {
                c.boot_order = vec![BootDevice::Network, BootDevice::Disk]
            }),
            ("nothing bootable", |c| c.boot_order = Vec::new()),
            ("no disks at all", |c| {
                c.disks = Vec::new();
                c.boot_order = vec![BootDevice::Network];
            }),
            ("characters the document would otherwise eat", |c| {
                c.description = Some("web & \"app\" <primary>".into())
            }),
            ("no recorded guest", |c| c.os_id = None),
            ("an installation drive, booting first", |c| {
                c.cdroms
                    .push(cdrom("sda", Some("/var/lib/lumen/iso/boot/al10.iso")));
                c.boot_order = vec![BootDevice::Cdrom, BootDevice::Disk];
            }),
            ("an empty drive", |c| c.cdroms.push(cdrom("sda", None))),
            ("installation media and the driver disc", |c| {
                c.cdroms
                    .push(cdrom("sda", Some("/var/lib/lumen/iso/boot/win.iso")));
                c.cdroms
                    .push(cdrom("sdb", Some("/var/lib/lumen/iso/boot/virtio-win.iso")));
                c.boot_order = vec![BootDevice::Cdrom, BootDevice::Disk];
            }),
            ("a drive alongside disks that share its prefix", |c| {
                c.disks.push(disk(
                    "sda",
                    "/dev/zvol/boot/lumen/vm-101-disk-1",
                    DiskBus::Sata,
                ));
                c.cdroms
                    .push(cdrom("sdb", Some("/var/lib/lumen/iso/boot/al10.iso")));
            }),
            ("a drive named in the boot order that is not there", |c| {
                c.boot_order = vec![BootDevice::Cdrom, BootDevice::Disk]
            }),
        ];

        for (name, mutate) in cases {
            let mut config = sample();
            mutate(&mut config);
            let xml = render(&config);
            let parsed = parse(&xml).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(parsed.config, config.normalized(), "{name}\n{xml}");
        }
    }

    /// Normalizing twice must produce the same thing as normalizing once,
    /// otherwise a machine would drift a little every time it was saved.
    #[test]
    fn normalizing_is_idempotent() {
        let once = sample().normalized();
        assert_eq!(once.normalized(), once);
    }

    #[test]
    fn boot_numbers_come_out_of_the_device_class_order() {
        let mut config = sample();
        config.disks.push(disk(
            "vdb",
            "/dev/zvol/boot/lumen/vm-101-disk-1",
            DiskBus::VirtioBlk,
        ));
        let normalized = config.normalized();
        assert_eq!(normalized.disks[0].boot_index, Some(1));
        assert_eq!(normalized.disks[1].boot_index, Some(2));

        // Adapters first, and the disks move down behind them.
        config.boot_order = vec![BootDevice::Network, BootDevice::Disk];
        let normalized = config.normalized();
        assert_eq!(normalized.disks[0].boot_index, Some(2));
        assert_eq!(normalized.disks[1].boot_index, Some(3));
    }

    /// An entry for a device class the machine does not have is not a setting.
    #[test]
    fn a_boot_entry_with_nothing_behind_it_is_dropped() {
        let mut config = sample();
        config.boot_order = vec![BootDevice::Cdrom, BootDevice::Disk];
        assert_eq!(config.normalized().boot_order, vec![BootDevice::Disk]);

        config.nics = Vec::new();
        config.boot_order = vec![BootDevice::Disk, BootDevice::Network];
        assert_eq!(config.normalized().boot_order, vec![BootDevice::Disk]);
    }

    /// The hypervisor hands the document back with elements of its own and
    /// with memory in its own unit. None of that may disturb the reader.
    #[test]
    fn a_document_the_hypervisor_has_been_through_still_reads() {
        let xml = format!(
            r#"<domain type='kvm' id='3'>
  <name>web01</name>
  <uuid>0c8d4b1e-3a5f-4e8a-9d21-7f3c9b0a1122</uuid>
  <metadata>
    <ns0:vm xmlns:ns0='{LUMEN_NS}'>
      <ns0:vmid>101</ns0:vmid>
      <ns0:tags>production</ns0:tags>
      <ns0:started>1785000000</ns0:started>
    </ns0:vm>
  </metadata>
  <memory unit='KiB'>8388608</memory>
  <currentMemory unit='KiB'>8388608</currentMemory>
  <vcpu placement='static'>4</vcpu>
  <resource><partition>/machine</partition></resource>
  <os firmware='efi'>
    <type arch='x86_64' machine='pc-q35-rhel9.6.0'>hvm</type>
    <firmware><feature enabled='no' name='enrolled-keys'/></firmware>
    <loader readonly='yes' type='pflash'>/usr/share/edk2/ovmf/OVMF_CODE.fd</loader>
    <nvram template='/usr/share/edk2/ovmf/OVMF_VARS.fd'>/var/lib/libvirt/qemu/nvram/web01_VARS.fd</nvram>
  </os>
  <cpu mode='host-model' check='partial'/>
  <devices>
    <emulator>/usr/libexec/qemu-kvm</emulator>
    <disk type='block' device='disk'>
      <driver name='qemu' type='raw' cache='none' discard='unmap'/>
      <source dev='/dev/zvol/boot/lumen/vm-101-disk-0' index='1'/>
      <backingStore/>
      <target dev='vda' bus='virtio'/>
      <boot order='1'/>
      <alias name='virtio-disk0'/>
      <address type='pci' domain='0x0000' bus='0x04' slot='0x00' function='0x0'/>
    </disk>
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <target dev='sdz' bus='sata'/>
      <readonly/>
    </disk>
    <interface type='bridge'>
      <mac address='52:54:00:00:65:00'/>
      <source bridge='br0'/>
      <target dev='vnet3'/>
      <model type='virtio'/>
      <boot order='2'/>
      <alias name='net0'/>
    </interface>
    <channel type='unix'>
      <source mode='bind' path='/var/lib/libvirt/qemu/channel/target/domain-3-web01/org.qemu.guest_agent.0'/>
      <target type='virtio' name='org.qemu.guest_agent.0' state='connected'/>
    </channel>
    <graphics type='vnc' socket='/var/lib/libvirt/qemu/lumen-101-vnc.sock'>
      <listen type='socket' socket='/var/lib/libvirt/qemu/lumen-101-vnc.sock'/>
    </graphics>
    <memballoon model='virtio'><alias name='balloon0'/></memballoon>
  </devices>
  <seclabel type='dynamic' model='selinux' relabel='yes'/>
</domain>"#
        );

        let parsed = parse(&xml).expect("the document parses");
        assert_eq!(parsed.config.vmid, 101);
        assert_eq!(parsed.config.name, "web01");
        assert_eq!(parsed.config.vcpus, 4);
        // KiB on the way in, MiB in the model.
        assert_eq!(parsed.config.memory_mib, 8192);
        assert_eq!(parsed.config.firmware, Firmware::Uefi);
        assert_eq!(parsed.config.machine, "pc-q35-rhel9.6.0");
        assert_eq!(parsed.config.cpu_model, CpuModel::HostModel);
        assert_eq!(parsed.config.tags, vec!["production".to_string()]);
        assert!(parsed.config.guest_agent);
        assert_eq!(parsed.started_at, Some(1_785_000_000));
        assert_eq!(
            parsed.uuid.as_deref(),
            Some("0c8d4b1e-3a5f-4e8a-9d21-7f3c9b0a1122")
        );

        // The optical drive is a disk element too, and it is not one of ours.
        assert_eq!(parsed.config.disks.len(), 1);
        assert_eq!(parsed.config.disks[0].id, "vda");
        assert_eq!(
            parsed.config.disks[0].source,
            "/dev/zvol/boot/lumen/vm-101-disk-0"
        );
        assert!(parsed.config.disks[0].discard);

        assert_eq!(parsed.config.nics.len(), 1);
        assert_eq!(parsed.config.nics[0].bridge, "br0");
        assert_eq!(
            parsed.config.boot_order,
            vec![BootDevice::Disk, BootDevice::Network]
        );
    }

    /// A machine somebody else defined is visible but not ours to manage, and
    /// the refusal says which one it is.
    #[test]
    fn a_document_with_no_lumen_identifier_is_refused_by_name() {
        let xml = "<domain type='kvm'><name>somebody-elses-vm</name>\
                   <memory unit='KiB'>1048576</memory><vcpu>1</vcpu>\
                   <os><type arch='x86_64' machine='q35'>hvm</type></os>\
                   <devices/></domain>";
        let err = parse(xml).unwrap_err();
        assert!(matches!(err, VirtError::Conflict(_)), "{err:?}");
        assert!(err.to_string().contains("somebody-elses-vm"), "{err}");
    }

    #[test]
    fn a_console_socket_is_defined_from_the_very_first_machine() {
        let xml = render(&sample());
        assert!(
            xml.contains(&format!("socket='{}'", vnc_socket_path(101))),
            "{xml}"
        );
        assert_eq!(
            vnc_socket_path(101),
            "/var/lib/libvirt/qemu/lumen-101-vnc.sock"
        );
        // What was written is what comes back: the viewer connects to the
        // machine's own answer rather than to the path it would have guessed.
        assert_eq!(
            vnc_socket_of(&xml).as_deref(),
            Some(vnc_socket_path(101).as_str())
        );
    }

    /// `virDomainSetMetadata` attaches the namespace itself, from the prefix
    /// and URI it is given. An element that also declares it makes libxml2
    /// refuse the duplicate prefix, and libvirt turns that into "internal
    /// error: failed to create a new XML namespace" — so this is not a style
    /// rule, it is the difference between recording a start time and never
    /// recording one on any machine.
    #[test]
    fn live_metadata_leaves_the_namespace_to_the_hypervisor() {
        let metadata = live_metadata(&sample(), 1_700_000_000);
        assert!(
            !metadata.contains("xmlns"),
            "the element must not declare the namespace it is about to be given: {metadata}"
        );
        assert!(
            !metadata.contains("lumen:"),
            "nor carry the prefix: {metadata}"
        );

        // And it still reads back, which is the whole point of writing it.
        assert_eq!(started_from_metadata(&metadata), Some(1_700_000_000));
    }

    /// The hypervisor hands metadata back with its own prefix on it, and the
    /// reader matches local names precisely so that both shapes work.
    #[test]
    fn a_start_time_reads_back_however_the_hypervisor_prefixes_it() {
        assert_eq!(
            started_from_metadata("<vm><started>1700000000</started></vm>"),
            Some(1_700_000_000)
        );
        assert_eq!(
            started_from_metadata(&format!(
                "<lumen:vm xmlns:lumen='{LUMEN_NS}'><lumen:started>1700000000</lumen:started></lumen:vm>"
            )),
            Some(1_700_000_000)
        );
    }

    /// The console viewer must follow the document, not the naming rule — a
    /// machine defined elsewhere, or edited by hand, has its socket where it
    /// has it.
    #[test]
    fn the_console_socket_is_read_from_the_document_the_hypervisor_returns() {
        // The hypervisor's own answer, on the <listen> child, wins over the
        // attribute it was handed.
        let both = "<domain type='kvm'><name>web01</name><devices>\
                    <graphics type='vnc' socket='/stale.sock'>\
                    <listen type='socket' socket='/run/live.sock'/>\
                    </graphics></devices></domain>";
        assert_eq!(vnc_socket_of(both).as_deref(), Some("/run/live.sock"));

        // An element with no child still answers.
        let bare = "<domain type='kvm'><name>web01</name><devices>\
                    <graphics type='vnc' socket='/run/only.sock'/>\
                    </devices></domain>";
        assert_eq!(vnc_socket_of(bare).as_deref(), Some("/run/only.sock"));

        // A graphics device that is not VNC is not this viewer's, and a
        // port-listening one has no socket to speak of.
        let spice = "<domain type='kvm'><name>web01</name><devices>\
                     <graphics type='spice' socket='/run/spice.sock'/>\
                     </devices></domain>";
        assert_eq!(vnc_socket_of(spice), None);
        let on_a_port = "<domain type='kvm'><name>web01</name><devices>\
                         <graphics type='vnc' port='5900' listen='127.0.0.1'/>\
                         </devices></domain>";
        assert_eq!(vnc_socket_of(on_a_port), None);

        // A machine with no graphics at all has no console, and says so rather
        // than handing back a path nothing is listening on.
        let headless = "<domain type='kvm'><name>web01</name><devices/></domain>";
        assert_eq!(vnc_socket_of(headless), None);
    }

    /// Without this the guest never hears a shutdown request and an orderly
    /// stop silently does nothing.
    #[test]
    fn power_management_is_always_present() {
        let xml = render(&sample());
        assert!(xml.contains("<acpi/>"), "{xml}");
    }

    #[test]
    fn a_scsi_controller_appears_only_when_something_needs_it() {
        let mut config = sample();
        assert!(!render(&config).contains("virtio-scsi"));
        config.disks.push(disk(
            "sda",
            "/dev/zvol/boot/lumen/vm-101-disk-1",
            DiskBus::VirtioScsi,
        ));
        assert!(render(&config).contains("model='virtio-scsi'"));
    }

    /// The whole `<video>` element, so an assertion about the card cannot be
    /// satisfied by the adapter's `<model type='virtio'/>` somewhere else in
    /// the same document.
    fn video_block(model: VideoModel) -> String {
        format!(
            "<video>\n      <model type='{}'/>\n    </video>",
            model.as_str()
        )
    }

    /// The card is a choice now, so it has to survive the document in both
    /// directions — a machine that comes back as the wrong card would be
    /// silently reset to it by the next save.
    #[test]
    fn the_graphics_card_is_written_and_read_back() {
        let mut config = sample();
        // The default is what every machine defined so far already carries.
        assert!(render(&config).contains(&video_block(VideoModel::Virtio)));

        for model in [VideoModel::Vga, VideoModel::Bochs, VideoModel::Qxl] {
            config.video = model;
            let xml = render(&config);
            assert!(xml.contains(&video_block(model)), "{xml}");
            assert_eq!(parse(&xml).unwrap().config.video, model);
        }
    }

    /// A machine defined before this appliance put a screen on one carries no
    /// `<video>` at all. That is not an error and not an unknown card: it reads
    /// as the default, which is exactly what saving the machine then writes
    /// into its document — the remedy `VirtService::console` names.
    #[test]
    fn a_document_with_no_video_device_reads_as_the_default_card() {
        let mut config = sample();
        config.video = VideoModel::Vga;
        let without = render(&config).replace(&video_block(VideoModel::Vga), "");
        assert!(!without.contains("<video>"), "{without}");
        assert_eq!(parse(&without).unwrap().config.video, VideoModel::Virtio);
    }

    /// …and that default is precisely why the card cannot be asked whether the
    /// machine has a screen. Both documents below parse to `Virtio`; only one
    /// of them has anything for a viewer to connect to, and telling an
    /// operator the second one has a VirtIO GPU is how "save it and restart"
    /// reads as "the console is broken".
    #[test]
    fn the_card_cannot_answer_whether_there_is_a_screen_but_the_document_can() {
        let config = sample();
        let with = render(&config);
        assert!(has_screen(&with));

        // A machine from before consoles: no <graphics>, no <video>.
        let without = with
            .replace(
                &format!(
                    "    <graphics type='vnc' socket='{}'/>\n",
                    vnc_socket_path(config.vmid)
                ),
                "",
            )
            .replace(&format!("    {}\n", video_block(VideoModel::Virtio)), "");
        assert!(!without.contains("<graphics"), "{without}");
        assert!(!without.contains("<video>"), "{without}");

        // The card says the same thing for both. The document does not.
        assert_eq!(
            parse(&with).unwrap().config.video,
            parse(&without).unwrap().config.video
        );
        assert!(!has_screen(&without));
    }

    /// Operator text goes into the document as text, not as markup.
    #[test]
    fn a_name_full_of_markup_cannot_break_out_of_its_element() {
        let mut config = sample();
        config.description = Some("</lumen:description><evil/>".into());
        let xml = render(&config);
        assert!(!xml.contains("<evil/>"), "{xml}");
        let parsed = parse(&xml).unwrap();
        assert_eq!(
            parsed.config.description.as_deref(),
            Some("</lumen:description><evil/>")
        );
    }

    #[test]
    fn live_metadata_carries_the_start_time_and_reads_back_out_of_itself() {
        let fragment = live_metadata(&sample(), 1_785_000_000);
        // Unprefixed, because `virDomainSetMetadata` attaches the prefix
        // itself — see `live_metadata_leaves_the_namespace_to_the_hypervisor`,
        // which is the test that says why it must not be here.
        assert!(
            fragment.contains("<started>1785000000</started>"),
            "{fragment}"
        );
        assert!(fragment.contains("<vmid>101</vmid>"), "{fragment}");
        // The hypervisor hands this element back on its own, outside any
        // document, and the same reader has to cope with that.
        assert_eq!(started_from_metadata(&fragment), Some(1_785_000_000));
        // A machine somebody else started carries none, which is not an error.
        assert_eq!(
            started_from_metadata(
                "<lumen:vm xmlns:lumen='x'><lumen:vmid>101</lumen:vmid></lumen:vm>"
            ),
            None
        );
        assert_eq!(started_from_metadata("not a document at all"), None);
    }

    #[test]
    fn device_fragments_are_the_same_markup_the_document_uses() {
        let config = sample().normalized();
        let whole = render(&config);
        assert!(whole.contains(disk_fragment(&config.disks[0]).trim_end()));
        assert!(whole.contains(nic_fragment(&config.nics[0]).trim_end()));
    }

    #[test]
    fn memory_units_all_land_in_the_same_place() {
        assert_eq!(to_mib(8_388_608, "KiB"), 8192);
        assert_eq!(to_mib(8192, "MiB"), 8192);
        assert_eq!(to_mib(8, "GiB"), 8192);
        assert_eq!(to_mib(8_589_934_592, "bytes"), 8192);
        // An unfamiliar unit reads as the one the hypervisor actually emits.
        assert_eq!(to_mib(8_388_608, "wat"), 8192);
    }
}
