//! Reading a VMware appliance archive — what machine an OVA describes.
//!
//! An OVA is a plain tar: one `.ovf` XML descriptor, the disks it names
//! (VMDK, usually stream-compressed), and a manifest of checksums. This
//! module answers one question about such an archive: *what machine is in
//! here, in Lumen's own vocabulary* — so the console can show an operator a
//! proposal they can correct, and the import job can know which bytes in the
//! archive are which disk.
//!
//! Nothing here converts anything. The descriptor is read, the hardware it
//! describes is mapped onto the machine model this crate already has —
//! deliberately easy, because the model grew VMware's dialect long before
//! this file existed: `pvscsi`, `vmxnet3`, and the EFI/BIOS choice are all
//! spellings [`crate::model`] knows — and everything the mapping cannot carry
//! is said out loud in [`OvaAppliance::warnings`] rather than dropped.
//!
//! ## Why a hand-rolled tar walk
//!
//! The archive is walked, never unpacked: the only member ever read into
//! memory is the descriptor, and a disk is named to the converter as an
//! offset and a length inside the archive it already lives in. The dozen
//! header fields that takes are less code than a dependency, and the one
//! subtlety — a member over 8 GiB carries its size in base-256 because the
//! octal field cannot hold it, and a VMDK is routinely that big — is handled
//! and tested here.
//!
//! The manifest's checksums are deliberately not verified: they could only be
//! checked by reading the whole archive a second time, minutes after the
//! upload that just wrote it, and TLS already carried the bytes here intact.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;
use serde::Serialize;

use crate::domain_xml::{attr, local_name};
use crate::error::{Result, VirtError};
use crate::model::{normalize_mac, DiskBus, Firmware, NicModel, ScsiController};

/// The descriptor is small XML measured in kilobytes. A member wearing the
/// `.ovf` name at gigabyte size is not a descriptor, and must not be read
/// into memory to find that out.
const MAX_DESCRIPTOR_BYTES: u64 = 10 * 1024 * 1024;

/// The machine one archive describes, in Lumen's vocabulary — the proposal
/// the console shows, corrected by the operator, and the facts the import
/// job needs about where each disk's bytes are.
#[derive(Debug, Clone, Serialize)]
pub struct OvaAppliance {
    /// What the appliance calls itself. Shown as the suggested machine name;
    /// it has not been checked against Lumen's name rules and the operator
    /// edits it anyway.
    pub name: String,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// What the source machine booted with. BIOS when the descriptor does
    /// not say — that is VMware's own default — and worth surfacing, because
    /// Lumen's default is UEFI and a machine imported under the wrong one
    /// does not boot.
    pub firmware: Firmware,
    /// The SCSI controller the source disks sat behind, mapped to the
    /// nearest model this appliance emulates. `None` when no disk is SCSI.
    pub scsi_controller: Option<ScsiController>,
    /// What the descriptor says the machine runs, in VMware's words.
    /// Display only; the operator picks the Lumen guest entry themselves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// In the order the descriptor lists them, which is the order the disks
    /// will be created and filled in.
    pub disks: Vec<OvaDisk>,
    pub nics: Vec<OvaNic>,
    /// Hardware that was read but could not be carried, one sentence each.
    /// Shown with the proposal: an import that silently dropped a device
    /// would be read as a broken machine, not as a choice.
    pub warnings: Vec<String>,
}

/// One disk: what the guest sees, and where its bytes are in the archive.
#[derive(Debug, Clone, Serialize)]
pub struct OvaDisk {
    /// The member name inside the archive — how the console and the progress
    /// feed refer to it.
    pub file: String,
    /// Bytes the guest sees: the size the volume must be created at.
    pub capacity: u64,
    /// Bytes in the archive — the compressed stream, so usually far smaller
    /// than the capacity.
    pub size: u64,
    /// Where the member's data starts in the archive. The converter reads
    /// the disk in place through this; nothing outside the appliance needs
    /// it.
    #[serde(skip)]
    pub offset: u64,
    /// The bus the source machine had it on, in Lumen's spelling: SCSI stays
    /// SCSI (behind [`OvaAppliance::scsi_controller`]), IDE and SATA both
    /// land on SATA.
    pub bus: DiskBus,
}

/// One network adapter, still naming the source's network — which bridge
/// that maps to is exactly the question the operator answers.
#[derive(Debug, Clone, Serialize)]
pub struct OvaNic {
    /// The OVF network the adapter was connected to.
    pub network: String,
    /// The nearest model this appliance emulates. VMware's own adapters map
    /// exactly; anything unrecognized becomes e1000e with a warning.
    pub model: NicModel,
    /// The source machine's hardware address, kept so the guest's NIC naming
    /// and any DHCP reservations survive the move.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

/// Read the machine out of an archive on disk.
///
/// Synchronous on purpose — it seeks through a file that may be tens of
/// gigabytes reading only headers and one small member. Callers on the
/// runtime wrap it in `spawn_blocking`.
pub fn read_appliance(path: &Path) -> Result<OvaAppliance> {
    let mut file = File::open(path)
        .map_err(|err| VirtError::Backend(anyhow::anyhow!("opening {}: {err}", path.display())))?;
    let members = tar_members(&mut file)?;

    let descriptor = members
        .iter()
        .find(|member| member.name.to_lowercase().ends_with(".ovf"))
        .ok_or_else(|| {
            VirtError::Conflict(
                "The archive has no .ovf descriptor in it, so it is not an OVA export.".into(),
            )
        })?;
    if descriptor.size > MAX_DESCRIPTOR_BYTES {
        return Err(VirtError::Conflict(format!(
            "\"{}\" is {} bytes, which is far too large to be an appliance descriptor.",
            descriptor.name, descriptor.size
        )));
    }
    file.seek(SeekFrom::Start(descriptor.offset))
        .map_err(VirtError::from)?;
    let mut xml = vec![0u8; descriptor.size as usize];
    file.read_exact(&mut xml).map_err(VirtError::from)?;
    let xml = String::from_utf8(xml).map_err(|_| {
        VirtError::Conflict(format!("\"{}\" is not readable XML.", descriptor.name))
    })?;

    let parsed = parse_descriptor(&xml)?;

    // Every disk the descriptor names has to be in the archive, or the
    // machine cannot be filled in. Checked here, before anything is created
    // anywhere, and refused with the missing name in the sentence.
    let by_name: HashMap<&str, &TarMember> = members
        .iter()
        .map(|member| (member.name.trim_start_matches("./"), member))
        .collect();
    let mut disks = Vec::new();
    for disk in parsed.disks {
        let member = by_name.get(disk.file.trim_start_matches("./")).ok_or_else(|| {
            VirtError::Conflict(format!(
                "The descriptor names \"{}\" as a disk, but the archive does not contain it.",
                disk.file
            ))
        })?;
        disks.push(OvaDisk {
            file: disk.file,
            capacity: disk.capacity,
            size: member.size,
            offset: member.offset,
            bus: disk.bus,
        });
    }

    Ok(OvaAppliance {
        name: parsed.name,
        vcpus: parsed.vcpus,
        memory_mib: parsed.memory_mib,
        firmware: parsed.firmware,
        scsi_controller: parsed.scsi_controller,
        os: parsed.os,
        disks,
        nics: parsed.nics,
        warnings: parsed.warnings,
    })
}

// --- the tar walk ------------------------------------------------------------

/// One member: its name and where its data is. Offsets are to the data, not
/// the header.
struct TarMember {
    name: String,
    offset: u64,
    size: u64,
}

/// Walk the archive's headers without reading any member's data.
fn tar_members(file: &mut File) -> Result<Vec<TarMember>> {
    let total = file.seek(SeekFrom::End(0)).map_err(VirtError::from)?;
    file.seek(SeekFrom::Start(0)).map_err(VirtError::from)?;

    let mut members = Vec::new();
    let mut offset = 0u64;
    // A GNU long-name entry names the member that follows it.
    let mut long_name: Option<String> = None;
    let mut header = [0u8; 512];

    while offset + 512 <= total {
        file.seek(SeekFrom::Start(offset)).map_err(VirtError::from)?;
        file.read_exact(&mut header).map_err(VirtError::from)?;
        if header.iter().all(|byte| *byte == 0) {
            break; // The end-of-archive marker.
        }
        if &header[257..262] != b"ustar" {
            return Err(VirtError::Conflict(
                "The file is not a tar archive, so it is not an OVA export.".into(),
            ));
        }
        let size = field_number(&header[124..136]).ok_or_else(|| {
            VirtError::Conflict("The archive has an unreadable member size in it.".into())
        })?;
        let data = offset + 512;
        let padded = size.div_ceil(512) * 512;

        match header[156] {
            // A regular file, under either spelling.
            0 | b'0' => {
                let name = long_name.take().unwrap_or_else(|| {
                    let stem = text_field(&header[0..100]);
                    let prefix = text_field(&header[345..500]);
                    if prefix.is_empty() {
                        stem
                    } else {
                        format!("{prefix}/{stem}")
                    }
                });
                members.push(TarMember {
                    name,
                    offset: data,
                    size,
                });
            }
            // GNU long name: the data is the real name of the next member.
            b'L' => {
                if size > 4096 {
                    return Err(VirtError::Conflict(
                        "The archive has an implausibly long member name in it.".into(),
                    ));
                }
                let mut bytes = vec![0u8; size as usize];
                file.read_exact(&mut bytes).map_err(VirtError::from)?;
                long_name = Some(text_field(&bytes));
            }
            // Anything else — pax headers, directories, links — is not a
            // member an OVA needs, and is stepped over.
            _ => {
                long_name = None;
            }
        }
        offset = data + padded;
    }
    if members.is_empty() {
        return Err(VirtError::Conflict(
            "The archive is empty, so it is not an OVA export.".into(),
        ));
    }
    Ok(members)
}

/// A NUL-terminated text field.
fn text_field(field: &[u8]) -> String {
    let end = field.iter().position(|byte| *byte == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).trim().to_string()
}

/// A tar number: octal in ASCII, or base-256 when the value cannot fit —
/// which a disk image routinely cannot, octal's ceiling in a 12-byte field
/// being 8 GiB.
fn field_number(field: &[u8]) -> Option<u64> {
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        let mut value: u64 = (field[0] & 0x7f) as u64;
        for byte in &field[1..] {
            value = value.checked_mul(256)?.checked_add(*byte as u64)?;
        }
        return Some(value);
    }
    let text = String::from_utf8_lossy(field);
    let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}

// --- the descriptor ----------------------------------------------------------

/// What the XML said, before the archive's members are matched against it.
struct ParsedDescriptor {
    name: String,
    vcpus: u32,
    memory_mib: u64,
    firmware: Firmware,
    scsi_controller: Option<ScsiController>,
    os: Option<String>,
    disks: Vec<ParsedDisk>,
    nics: Vec<OvaNic>,
    warnings: Vec<String>,
}

struct ParsedDisk {
    file: String,
    capacity: u64,
    bus: DiskBus,
}

/// What kind of controller an Item declared, by its instance identifier.
#[derive(Clone, Copy, PartialEq)]
enum ControllerKind {
    Scsi,
    Ide,
    Sata,
}

/// One `<Item>` while its children are being read. The rasd fields are all
/// text elements, collected by name and dispatched when the item closes.
#[derive(Default)]
struct ItemAcc {
    resource_type: Option<u32>,
    sub_type: String,
    quantity: Option<u64>,
    units: String,
    address: String,
    parent: String,
    instance: String,
    host_resource: String,
    connection: String,
}

/// A disk item, held until the sections it points into have all been read —
/// the descriptor may put DiskSection after the hardware.
struct DiskItem {
    disk_id: String,
    parent: String,
}

fn parse_descriptor(xml: &str) -> Result<ParsedDescriptor> {
    let mut reader = XmlReader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut path: Vec<String> = Vec::new();

    let mut systems = 0u32;
    let mut system_id = String::new();
    let mut system_name = String::new();
    let mut os: Option<String> = None;
    let mut vcpus: Option<u32> = None;
    let mut memory_mib: Option<u64> = None;
    let mut firmware = Firmware::Bios;
    let mut warnings: Vec<String> = Vec::new();

    // References: file id → (href, declared size). DiskSection: disk id →
    // (file id, capacity, units).
    let mut files: HashMap<String, String> = HashMap::new();
    let mut disk_section: HashMap<String, (String, u64, String)> = HashMap::new();
    let mut disk_order: Vec<String> = Vec::new();

    let mut controllers: HashMap<String, ControllerKind> = HashMap::new();
    let mut scsi_sub_type: Option<String> = None;
    let mut disk_items: Vec<DiskItem> = Vec::new();
    let mut nics: Vec<OvaNic> = Vec::new();

    let mut item: Option<ItemAcc> = None;

    loop {
        let event = reader.read_event().map_err(|err| {
            VirtError::Conflict(format!("The appliance descriptor is not readable XML: {err}"))
        })?;
        let empty = matches!(event, Event::Empty(_));

        match event {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => {
                let name = local_name(e.name().as_ref());
                path.push(name.clone());
                let here = path.join("/");

                match here.as_str() {
                    "Envelope/References/File" => {
                        if let (Some(id), Some(href)) = (attr(e, "id"), attr(e, "href")) {
                            files.insert(id, href);
                        }
                    }
                    "Envelope/DiskSection/Disk" => {
                        let id = attr(e, "diskId").unwrap_or_default();
                        let capacity = attr(e, "capacity")
                            .and_then(|text| text.parse::<u64>().ok())
                            .unwrap_or(0);
                        let units = attr(e, "capacityAllocationUnits").unwrap_or_default();
                        let file_ref = attr(e, "fileRef").unwrap_or_default();
                        disk_order.push(id.clone());
                        disk_section.insert(id, (file_ref, capacity, units));
                    }
                    "Envelope/VirtualSystem" => {
                        systems += 1;
                        system_id = attr(e, "id").unwrap_or_default();
                    }
                    "Envelope/VirtualSystem/OperatingSystemSection" => {
                        if let Some(kind) = attr(e, "osType") {
                            os = Some(kind);
                        }
                    }
                    "Envelope/VirtualSystem/VirtualHardwareSection/Item" => {
                        item = Some(ItemAcc::default());
                    }
                    // VMware's own extension elements: the firmware choice
                    // rides one of these, and it is the one fact here that
                    // decides whether the imported machine boots at all.
                    "Envelope/VirtualSystem/VirtualHardwareSection/Config" => {
                        if attr(e, "key").as_deref() == Some("firmware") {
                            firmware = match attr(e, "value").as_deref() {
                                Some("efi") => Firmware::Uefi,
                                _ => Firmware::Bios,
                            };
                        }
                    }
                    _ => {}
                }
                if empty {
                    close_element(&mut path, &mut item, &mut warnings, &mut vcpus, &mut memory_mib,
                        &mut controllers, &mut scsi_sub_type, &mut disk_items, &mut nics);
                }
            }
            Event::Text(ref text) => {
                let value = text
                    .unescape()
                    .map(|value| value.into_owned())
                    .unwrap_or_default();
                let here = path.join("/");
                if let Some(acc) = item.as_mut() {
                    if let Some(field) = path.last() {
                        collect_item_field(acc, field, &value);
                    }
                } else if here == "Envelope/VirtualSystem/Name" {
                    system_name = value;
                } else if here == "Envelope/VirtualSystem/OperatingSystemSection/Description" {
                    // The attribute's identifier is for machines; the
                    // description is for people, so it wins when both exist.
                    os = Some(value);
                }
            }
            Event::End(_) => {
                close_element(&mut path, &mut item, &mut warnings, &mut vcpus, &mut memory_mib,
                    &mut controllers, &mut scsi_sub_type, &mut disk_items, &mut nics);
            }
            _ => {}
        }
    }

    if systems == 0 {
        return Err(VirtError::Conflict(
            "The descriptor contains no machine at all.".into(),
        ));
    }
    if systems > 1 {
        return Err(VirtError::Conflict(format!(
            "The archive contains {systems} machines. Import takes one machine at a time — \
             export each from vCenter on its own."
        )));
    }

    if vcpus.is_none() {
        warnings.push("The descriptor names no processor count; one is proposed.".into());
    }
    if memory_mib.is_none() {
        warnings.push("The descriptor names no memory size; 2048 MiB is proposed.".into());
    }

    // The controller the SCSI disks sat behind, mapped to the nearest model
    // this appliance emulates — so the guest's existing driver still finds
    // its controller on first boot.
    let scsi_controller = scsi_sub_type.as_deref().map(|sub| {
        let lowered = sub.to_lowercase();
        if lowered.contains("virtualscsi") || lowered.contains("paravirtual") {
            ScsiController::Pvscsi
        } else if lowered.contains("lsilogic") {
            // Plain and SAS both: the 53C895A is the nearest thing this
            // appliance emulates, and every guest with an LSI driver has
            // this one's.
            if lowered.contains("sas") {
                warnings.push(format!(
                    "The source's \"{sub}\" controller is emulated as LSI 53C895A."
                ));
            }
            ScsiController::Lsi
        } else {
            warnings.push(format!(
                "\"{sub}\" is not a controller this appliance emulates; LSI 53C895A is \
                 proposed instead."
            ));
            ScsiController::Lsi
        }
    });

    // Disk items resolved through DiskSection and References, in the order
    // the descriptor listed the disks — which is the order the source
    // machine counted them in.
    let mut disks = Vec::new();
    for id in &disk_order {
        let Some((file_ref, capacity, units)) = disk_section.get(id) else {
            continue;
        };
        let Some(claimed) = disk_items.iter().find(|item| &item.disk_id == id) else {
            // A disk in the section no hardware item claims is not attached
            // to the machine.
            warnings.push(format!("Disk \"{id}\" is in the archive but attached to nothing."));
            continue;
        };
        let Some(href) = files.get(file_ref) else {
            return Err(VirtError::Conflict(format!(
                "The descriptor's disk \"{id}\" points at file reference \"{file_ref}\", \
                 which is not in its file list."
            )));
        };
        let capacity = allocation_bytes(*capacity, units).ok_or_else(|| {
            VirtError::Conflict(format!(
                "Disk \"{id}\" declares its capacity in \"{units}\", which is not a unit \
                 this import understands."
            ))
        })?;
        let bus = match controllers.get(&claimed.parent) {
            Some(ControllerKind::Scsi) => DiskBus::VirtioScsi,
            Some(ControllerKind::Ide) | Some(ControllerKind::Sata) => DiskBus::Sata,
            None => {
                warnings.push(format!(
                    "Disk \"{href}\" sits on a controller the descriptor does not describe; \
                     SATA is proposed."
                ));
                DiskBus::Sata
            }
        };
        disks.push(ParsedDisk {
            file: href.clone(),
            capacity,
            bus,
        });
    }

    let name = if system_name.is_empty() { system_id } else { system_name };
    Ok(ParsedDescriptor {
        name,
        vcpus: vcpus.unwrap_or(1),
        memory_mib: memory_mib.unwrap_or(2048),
        firmware,
        scsi_controller,
        os,
        disks,
        nics,
        warnings,
    })
}

/// Pop one element off the path, dispatching a finished `<Item>` when that
/// is what closed. One function because `Empty` events close in the same
/// breath they open, and the two paths must not drift.
#[allow(clippy::too_many_arguments)]
fn close_element(
    path: &mut Vec<String>,
    item: &mut Option<ItemAcc>,
    warnings: &mut Vec<String>,
    vcpus: &mut Option<u32>,
    memory_mib: &mut Option<u64>,
    controllers: &mut HashMap<String, ControllerKind>,
    scsi_sub_type: &mut Option<String>,
    disk_items: &mut Vec<DiskItem>,
    nics: &mut Vec<OvaNic>,
) {
    let closed = path.join("/");
    path.pop();
    if closed != "Envelope/VirtualSystem/VirtualHardwareSection/Item" {
        return;
    }
    let Some(acc) = item.take() else { return };

    // CIM resource types, as VMware writes them. Anything unlisted — USB
    // controllers, sound cards, the video card the source had — is dropped
    // without a word: none of them is data, and Lumen provides its own.
    match acc.resource_type {
        Some(3) => *vcpus = acc.quantity.map(|q| q as u32),
        Some(4) => {
            *memory_mib = acc
                .quantity
                .and_then(|q| allocation_bytes(q, &acc.units))
                .map(|bytes| bytes.div_ceil(1024 * 1024));
        }
        Some(5) => {
            controllers.insert(acc.instance, ControllerKind::Ide);
        }
        Some(6) => {
            controllers.insert(acc.instance, ControllerKind::Scsi);
            if scsi_sub_type.is_none() {
                *scsi_sub_type = Some(acc.sub_type);
            }
        }
        // "Other storage device" is how VMware spells its SATA controller.
        Some(20) => {
            if acc.sub_type.to_lowercase().contains("ahci")
                || acc.sub_type.to_lowercase().contains("sata")
            {
                controllers.insert(acc.instance, ControllerKind::Sata);
            }
        }
        Some(10) => {
            let model = match acc.sub_type.to_lowercase().as_str() {
                "vmxnet3" => NicModel::Vmxnet3,
                "e1000" => NicModel::E1000,
                "e1000e" => NicModel::E1000e,
                other => {
                    warnings.push(format!(
                        "\"{other}\" is not an adapter this appliance emulates; e1000e is \
                         proposed instead."
                    ));
                    NicModel::E1000e
                }
            };
            // The address survives the move so reservations keep working —
            // unless it is not a usable one, in which case a derived address
            // is better than a refused machine.
            let mac = match acc.address.trim() {
                "" => None,
                given => match normalize_mac(given) {
                    Some(mac) => Some(mac),
                    None => {
                        warnings.push(format!(
                            "\"{given}\" is not a usable hardware address; a new one will be \
                             derived."
                        ));
                        None
                    }
                },
            };
            nics.push(OvaNic {
                network: acc.connection,
                model,
                mac,
            });
        }
        Some(14) => warnings.push("The source machine's floppy drive is left behind.".into()),
        Some(15) | Some(16) => {
            if !acc.host_resource.is_empty() {
                warnings.push(
                    "The source machine's optical drive and its image are left behind.".into(),
                );
            }
        }
        Some(17) => {
            // "ovf:/disk/vmdisk1" → the DiskSection identifier at the end.
            let disk_id = acc
                .host_resource
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            if !disk_id.is_empty() {
                disk_items.push(DiskItem {
                    disk_id,
                    parent: acc.parent,
                });
            }
        }
        _ => {}
    }
}

/// One rasd child's text, into the accumulator field it names.
fn collect_item_field(acc: &mut ItemAcc, field: &str, value: &str) {
    match field {
        "ResourceType" => acc.resource_type = value.trim().parse().ok(),
        "ResourceSubType" => acc.sub_type = value.trim().to_string(),
        "VirtualQuantity" => acc.quantity = value.trim().parse().ok(),
        "AllocationUnits" => acc.units = value.trim().to_string(),
        "Address" => acc.address = value.trim().to_string(),
        "Parent" => acc.parent = value.trim().to_string(),
        "InstanceID" => acc.instance = value.trim().to_string(),
        "HostResource" => acc.host_resource = value.trim().to_string(),
        "Connection" => acc.connection = value.trim().to_string(),
        _ => {}
    }
}

/// A quantity in the descriptor's units, as bytes. The programmatic form
/// (`byte * 2^20`) is what VMware writes; the word forms are what everything
/// else writes.
fn allocation_bytes(quantity: u64, units: &str) -> Option<u64> {
    let trimmed = units.trim().to_lowercase();
    if trimmed.is_empty() || trimmed == "byte" || trimmed == "bytes" {
        return Some(quantity);
    }
    if let Some(rest) = trimmed.strip_prefix("byte") {
        let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '*');
        if let Some(power) = rest.trim().strip_prefix("2^") {
            let power: u32 = power.trim().parse().ok()?;
            // checked_mul, not checked_shl: a shift only refuses when the
            // *amount* is out of range, and would happily carry a capacity
            // off the top of the integer.
            return quantity.checked_mul(1u64.checked_shl(power)?);
        }
        return None;
    }
    let shift = match trimmed.as_str() {
        "kb" | "kib" | "kilobyte" | "kilobytes" => 10,
        "mb" | "mib" | "megabyte" | "megabytes" => 20,
        "gb" | "gib" | "gigabyte" | "gigabytes" => 30,
        "tb" | "tib" | "terabyte" | "terabytes" => 40,
        _ => return None,
    };
    quantity.checked_mul(1 << shift)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A descriptor the shape vCenter exports: two disks behind a
    /// paravirtual SCSI controller, a vmxnet3 adapter with a reserved
    /// address, EFI firmware, and the optical drive every export drags
    /// along.
    const DESCRIPTOR: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Envelope xmlns="http://schemas.dmtf.org/ovf/envelope/1"
          xmlns:ovf="http://schemas.dmtf.org/ovf/envelope/1"
          xmlns:rasd="http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_ResourceAllocationSettingData"
          xmlns:vmw="http://www.vmware.com/schema/ovf">
  <References>
    <File ovf:id="file1" ovf:href="web-disk1.vmdk" ovf:size="8192"/>
    <File ovf:id="file2" ovf:href="web-disk2.vmdk" ovf:size="4096"/>
  </References>
  <DiskSection>
    <Info>Virtual disk information</Info>
    <Disk ovf:capacity="40" ovf:capacityAllocationUnits="byte * 2^30" ovf:diskId="vmdisk1" ovf:fileRef="file1"/>
    <Disk ovf:capacity="16" ovf:capacityAllocationUnits="byte * 2^30" ovf:diskId="vmdisk2" ovf:fileRef="file2"/>
  </DiskSection>
  <NetworkSection>
    <Info>The list of logical networks</Info>
    <Network ovf:name="VM Network">
      <Description>The production network</Description>
    </Network>
  </NetworkSection>
  <VirtualSystem ovf:id="web-frontend">
    <Info>A virtual machine</Info>
    <Name>web-frontend</Name>
    <OperatingSystemSection ovf:id="101" vmw:osType="rhel9_64Guest">
      <Info>The operating system installed</Info>
      <Description>Red Hat Enterprise Linux 9 (64-bit)</Description>
    </OperatingSystemSection>
    <VirtualHardwareSection>
      <Info>Virtual hardware requirements</Info>
      <Item>
        <rasd:AllocationUnits>hertz * 10^6</rasd:AllocationUnits>
        <rasd:ElementName>2 virtual CPU(s)</rasd:ElementName>
        <rasd:InstanceID>1</rasd:InstanceID>
        <rasd:ResourceType>3</rasd:ResourceType>
        <rasd:VirtualQuantity>2</rasd:VirtualQuantity>
      </Item>
      <Item>
        <rasd:AllocationUnits>byte * 2^20</rasd:AllocationUnits>
        <rasd:ElementName>4096MB of memory</rasd:ElementName>
        <rasd:InstanceID>2</rasd:InstanceID>
        <rasd:ResourceType>4</rasd:ResourceType>
        <rasd:VirtualQuantity>4096</rasd:VirtualQuantity>
      </Item>
      <Item>
        <rasd:ElementName>SCSI Controller 0</rasd:ElementName>
        <rasd:InstanceID>3</rasd:InstanceID>
        <rasd:ResourceSubType>VirtualSCSI</rasd:ResourceSubType>
        <rasd:ResourceType>6</rasd:ResourceType>
      </Item>
      <Item>
        <rasd:ElementName>SATA Controller 0</rasd:ElementName>
        <rasd:InstanceID>4</rasd:InstanceID>
        <rasd:ResourceSubType>vmware.sata.ahci</rasd:ResourceSubType>
        <rasd:ResourceType>20</rasd:ResourceType>
      </Item>
      <Item>
        <rasd:AddressOnParent>0</rasd:AddressOnParent>
        <rasd:ElementName>Hard disk 1</rasd:ElementName>
        <rasd:HostResource>ovf:/disk/vmdisk1</rasd:HostResource>
        <rasd:InstanceID>5</rasd:InstanceID>
        <rasd:Parent>3</rasd:Parent>
        <rasd:ResourceType>17</rasd:ResourceType>
      </Item>
      <Item>
        <rasd:AddressOnParent>1</rasd:AddressOnParent>
        <rasd:ElementName>Hard disk 2</rasd:ElementName>
        <rasd:HostResource>ovf:/disk/vmdisk2</rasd:HostResource>
        <rasd:InstanceID>6</rasd:InstanceID>
        <rasd:Parent>3</rasd:Parent>
        <rasd:ResourceType>17</rasd:ResourceType>
      </Item>
      <Item>
        <rasd:AddressOnParent>0</rasd:AddressOnParent>
        <rasd:ElementName>CD/DVD drive 1</rasd:ElementName>
        <rasd:HostResource>ovf:/file/file3</rasd:HostResource>
        <rasd:InstanceID>7</rasd:InstanceID>
        <rasd:Parent>4</rasd:Parent>
        <rasd:ResourceType>15</rasd:ResourceType>
      </Item>
      <Item>
        <rasd:Address>00:50:56:9a:12:34</rasd:Address>
        <rasd:Connection>VM Network</rasd:Connection>
        <rasd:ElementName>Network adapter 1</rasd:ElementName>
        <rasd:InstanceID>8</rasd:InstanceID>
        <rasd:ResourceSubType>VMXNET3</rasd:ResourceSubType>
        <rasd:ResourceType>10</rasd:ResourceType>
      </Item>
      <vmw:Config ovf:required="false" vmw:key="firmware" vmw:value="efi"/>
    </VirtualHardwareSection>
  </VirtualSystem>
</Envelope>"#;

    #[test]
    fn a_vcenter_descriptor_reads_as_the_machine_it_describes() {
        let parsed = parse_descriptor(DESCRIPTOR).unwrap();
        assert_eq!(parsed.name, "web-frontend");
        assert_eq!(parsed.vcpus, 2);
        assert_eq!(parsed.memory_mib, 4096);
        assert_eq!(parsed.firmware, Firmware::Uefi);
        assert_eq!(parsed.scsi_controller, Some(ScsiController::Pvscsi));
        assert_eq!(parsed.os.as_deref(), Some("Red Hat Enterprise Linux 9 (64-bit)"));

        assert_eq!(parsed.disks.len(), 2);
        assert_eq!(parsed.disks[0].file, "web-disk1.vmdk");
        assert_eq!(parsed.disks[0].capacity, 40 * 1024 * 1024 * 1024);
        assert_eq!(parsed.disks[0].bus, DiskBus::VirtioScsi);
        assert_eq!(parsed.disks[1].file, "web-disk2.vmdk");
        assert_eq!(parsed.disks[1].capacity, 16 * 1024 * 1024 * 1024);

        assert_eq!(parsed.nics.len(), 1);
        assert_eq!(parsed.nics[0].network, "VM Network");
        assert_eq!(parsed.nics[0].model, NicModel::Vmxnet3);
        assert_eq!(parsed.nics[0].mac.as_deref(), Some("00:50:56:9a:12:34"));

        // The optical drive was seen and said out loud, not dropped quietly.
        assert!(
            parsed.warnings.iter().any(|w| w.contains("optical drive")),
            "{:?}",
            parsed.warnings
        );
    }

    #[test]
    fn firmware_defaults_to_bios_when_the_descriptor_is_silent() {
        let stripped = DESCRIPTOR.replace(
            r#"<vmw:Config ovf:required="false" vmw:key="firmware" vmw:value="efi"/>"#,
            "",
        );
        let parsed = parse_descriptor(&stripped).unwrap();
        assert_eq!(parsed.firmware, Firmware::Bios);
    }

    #[test]
    fn allocation_units_cover_the_forms_in_the_wild() {
        assert_eq!(allocation_bytes(512, ""), Some(512));
        assert_eq!(allocation_bytes(512, "byte"), Some(512));
        assert_eq!(allocation_bytes(4096, "byte * 2^20"), Some(4096 << 20));
        assert_eq!(allocation_bytes(40, "byte * 2^30"), Some(40 << 30));
        assert_eq!(allocation_bytes(40, "byte*2^30"), Some(40 << 30));
        assert_eq!(allocation_bytes(2, "MegaBytes"), Some(2 << 20));
        assert_eq!(allocation_bytes(1, "GB"), Some(1 << 30));
        assert_eq!(allocation_bytes(1, "furlongs"), None);
        // A power that would overflow is a refusal, not a wrapped size.
        assert_eq!(allocation_bytes(u64::MAX, "byte * 2^30"), None);
    }

    #[test]
    fn member_sizes_beyond_octal_read_as_base256() {
        // 12 GiB cannot be written in the 12-byte octal field; tar writes a
        // base-256 number with the high bit up instead.
        let twelve_gib: u64 = 12 * 1024 * 1024 * 1024;
        let mut field = [0u8; 12];
        field[0] = 0x80;
        for i in 0..8 {
            field[11 - i] = ((twelve_gib >> (8 * i)) & 0xff) as u8;
        }
        assert_eq!(field_number(&field), Some(twelve_gib));
        assert_eq!(field_number(b"00000020000\0"), Some(8192));
        assert_eq!(field_number(b"\0\0\0\0\0\0\0\0\0\0\0\0"), Some(0));
    }

    // --- whole archives ---------------------------------------------------

    /// A ustar archive with the given members, byte for byte.
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
            // The checksum is computed with its own field as spaces.
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

    fn write_archive(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lumen-ova-{tag}-{}-{:?}.ova",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        path
    }

    #[test]
    fn an_archive_resolves_each_disk_to_its_bytes() {
        let disk1 = vec![0xAAu8; 700]; // Deliberately not block-aligned.
        let disk2 = vec![0xBBu8; 512];
        let bytes = tar(&[
            ("web.ovf", DESCRIPTOR.as_bytes()),
            ("web-disk1.vmdk", &disk1),
            ("web-disk2.vmdk", &disk2),
        ]);
        let path = write_archive("resolve", &bytes);

        let appliance = read_appliance(&path).unwrap();
        assert_eq!(appliance.name, "web-frontend");
        assert_eq!(appliance.disks.len(), 2);
        assert_eq!(appliance.disks[0].size, 700);
        assert_eq!(appliance.disks[1].size, 512);
        // The offsets point at the members' data: reading there finds the
        // disks' own bytes, which is what the converter will do.
        let mut file = File::open(&path).unwrap();
        let mut byte = [0u8; 1];
        file.seek(SeekFrom::Start(appliance.disks[0].offset)).unwrap();
        file.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], 0xAA);
        file.seek(SeekFrom::Start(appliance.disks[1].offset)).unwrap();
        file.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], 0xBB);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_disk_the_archive_does_not_contain_is_refused_by_name() {
        let bytes = tar(&[
            ("web.ovf", DESCRIPTOR.as_bytes()),
            ("web-disk1.vmdk", b"x"),
        ]);
        let path = write_archive("missing", &bytes);
        let err = read_appliance(&path).unwrap_err();
        assert!(err.to_string().contains("web-disk2.vmdk"), "{err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_file_that_is_not_a_tar_is_refused_as_not_an_ova() {
        let path = write_archive("nottar", &vec![0x51u8; 2048]);
        let err = read_appliance(&path).unwrap_err();
        assert!(err.to_string().contains("not a tar archive"), "{err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_archive_with_two_machines_is_refused() {
        let doubled = DESCRIPTOR.replace(
            "</VirtualSystem>",
            r#"</VirtualSystem><VirtualSystem ovf:id="second"><Info>x</Info></VirtualSystem>"#,
        );
        let bytes = tar(&[
            ("pair.ovf", doubled.as_bytes()),
            ("web-disk1.vmdk", b"x"),
            ("web-disk2.vmdk", b"x"),
        ]);
        let path = write_archive("pair", &bytes);
        let err = read_appliance(&path).unwrap_err();
        assert!(err.to_string().contains("one machine at a time"), "{err}");
        std::fs::remove_file(&path).ok();
    }
}
