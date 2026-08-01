//! Which adapter answers to which name — and what to do when the hardware
//! behind a name is replaced.
//!
//! Lumen names every adapter `nic<N>` and pins the name to the card's
//! permanent MAC in `/etc/systemd/network/70-lumen-nic<N>.link`. That pin is
//! what makes a profile durable: `bond0` is a bond over `nic2` and `nic3`,
//! and it keeps meaning that across reboots, driver changes and PCI
//! renumbering.
//!
//! It also means a **replaced card orphans everything built on its name**.
//! The new card has different MACs, so no pin matches it; the old pin names
//! hardware that is no longer in the machine; and every profile above —
//! a bond, the Core network, the cluster's ring 0 — is bound to a ghost.
//! NetworkManager reports it as "no suitable device found", which is true
//! and unhelpful.
//!
//! ## Why adoption is an operator's decision
//!
//! The obvious fix is to let the namer notice a vanished card and give its
//! slot to a new one. It must not. A name is a promise about *which cable*,
//! and the appliance cannot know which port of a new card carries the
//! network the old one did — guessing puts storage replication on whatever
//! enumerated first, silently, and the operator finds out when the pool
//! degrades. So this module **reports** the orphaned slot and the adapters
//! nothing claims, and adopts one into the other only when told.
//!
//! Everything here is small file work under a root that tests override, in
//! the same shape `lumen_zfs::devices` reads disks — no daemon, no D-Bus.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{NetError, Result};

/// Where the node keeps its pins and describes its adapters.
#[derive(Debug, Clone)]
pub struct PinRoots {
    pub link_dir: PathBuf,
    pub sys_class_net: PathBuf,
}

impl Default for PinRoots {
    fn default() -> Self {
        Self {
            link_dir: "/etc/systemd/network".into(),
            sys_class_net: "/sys/class/net".into(),
        }
    }
}

impl PinRoots {
    pub fn under(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            link_dir: root.join("etc/systemd/network"),
            sys_class_net: root.join("sys/class/net"),
        }
    }

    fn pin_path(&self, slot: u32) -> PathBuf {
        self.link_dir.join(format!("70-lumen-nic{slot}.link"))
    }
}

/// One name pin as the file records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pin {
    /// The `N` of `nic<N>`.
    pub slot: u32,
    /// Lowercase, the way every comparison here wants it.
    pub mac: String,
    /// The kernel's own name for the card, recorded when it was renamed —
    /// what ties a row in the console to a label on the chassis.
    pub altname: Option<String>,
    /// An adapter with this MAC is in the machine right now.
    pub present: bool,
}

/// An adapter the node has that no pin claims — a card added or swapped in
/// since the names were last written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unclaimed {
    /// The name the kernel gave it, which is what it still answers to.
    pub device: String,
    pub mac: String,
    /// Enough to tell a 10G uplink from an unplugged management port
    /// without leaving the page.
    pub carrier: bool,
    pub speed_mbps: Option<u32>,
    pub driver: Option<String>,
}

/// What the console needs to explain a broken name and offer the fix.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PinReport {
    /// Slots whose card is not in the machine. Every profile naming one of
    /// these is currently bound to nothing.
    pub orphaned: Vec<Pin>,
    /// Adapters present that no slot names.
    pub unclaimed: Vec<Unclaimed>,
}

fn lower(mac: &str) -> String {
    mac.trim().to_ascii_lowercase()
}

/// Read one file's `Key=value` lines. The format is systemd's; only three
/// keys matter and anything else is somebody's comment.
fn field(text: &str, key: &str) -> Option<String> {
    text.lines().map(str::trim).find_map(|line| {
        line.strip_prefix(key)?
            .strip_prefix('=')
            .map(str::to_string)
    })
}

/// Every pin the node has recorded, lowest slot first.
pub fn pins(roots: &PinRoots) -> Vec<Pin> {
    let present = present_macs(roots);
    let Ok(entries) = std::fs::read_dir(&roots.link_dir) else {
        return Vec::new();
    };
    let mut out: Vec<Pin> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let slot: u32 = name
                .strip_prefix("70-lumen-nic")?
                .strip_suffix(".link")?
                .parse()
                .ok()?;
            let text = std::fs::read_to_string(entry.path()).ok()?;
            let mac = lower(&field(&text, "PermanentMACAddress")?);
            Some(Pin {
                slot,
                present: present.contains_key(&mac),
                mac,
                altname: field(&text, "AlternativeName"),
            })
        })
        .collect();
    out.sort_by_key(|pin| pin.slot);
    out
}

/// Every physical adapter the node has, by MAC. Virtual links have no
/// `device` symlink and are not adapters anybody pins.
fn present_macs(roots: &PinRoots) -> HashMap<String, String> {
    let mut found = HashMap::new();
    let Ok(entries) = std::fs::read_dir(&roots.sys_class_net) else {
        return found;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.join("device").exists() {
            continue;
        }
        // Ethernet only, the same test the namer applies.
        if read_trimmed(&dir.join("type")).as_deref() != Some("1") {
            continue;
        }
        // And the namer's driver exclusions, for the same reason: a BMC's
        // USB gadget looks like an ethernet NIC and leads to the service
        // processor. Offering it as a replacement for a real adapter would
        // pin a cluster network to a dead end.
        if let Ok(driver) = std::fs::read_link(dir.join("device/driver")) {
            if let Some(name) = driver.file_name().and_then(|n| n.to_str()) {
                if matches!(name, "rndis_host" | "cdc_ether" | "cdc_ncm" | "cdc_eem") {
                    continue;
                }
            }
        }
        let Some(mac) = permanent_mac(&dir) else {
            continue;
        };
        found.insert(mac, entry.file_name().to_string_lossy().into_owned());
    }
    found
}

/// The address a pin is written against: the card's **permanent** one.
///
/// `address` is the address the link is *using*, and an enslaved bond port
/// borrows its bond's — so comparing a pin against it reports every
/// bonded adapter as missing hardware. That is not hypothetical: it fired
/// on a live node, declaring the second port of a healthy Core bond an
/// orphan and offering to replace a card that was sitting right there.
/// The kernel keeps the real one for exactly this case.
fn permanent_mac(dir: &Path) -> Option<String> {
    read_trimmed(&dir.join("bonding_slave/perm_hwaddr"))
        .or_else(|| read_trimmed(&dir.join("address")))
        .map(|mac| lower(&mac))
        .filter(|mac| !mac.is_empty() && mac != "00:00:00:00:00:00")
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The orphaned slots and the adapters that could fill them.
pub fn report(roots: &PinRoots) -> PinReport {
    let pins = pins(roots);
    let pinned: Vec<String> = pins.iter().map(|pin| pin.mac.clone()).collect();
    let mut unclaimed: Vec<Unclaimed> = present_macs(roots)
        .into_iter()
        .filter(|(mac, _)| !pinned.contains(mac))
        .map(|(mac, device)| {
            let dir = roots.sys_class_net.join(&device);
            Unclaimed {
                carrier: read_trimmed(&dir.join("carrier")).as_deref() == Some("1"),
                speed_mbps: read_trimmed(&dir.join("speed"))
                    .and_then(|s| s.parse::<i64>().ok())
                    .filter(|speed| *speed > 0)
                    .map(|speed| speed as u32),
                driver: std::fs::read_link(dir.join("device/driver"))
                    .ok()
                    .and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned())),
                device,
                mac,
            }
        })
        .collect();
    unclaimed.sort_by(|a, b| a.device.cmp(&b.device));
    PinReport {
        orphaned: pins.into_iter().filter(|pin| !pin.present).collect(),
        unclaimed,
    }
}

/// Give an orphaned slot to an adapter that is actually in the machine.
///
/// Refused unless both halves are true — the slot's card is really gone,
/// and the MAC really is an unclaimed adapter here. Adopting into a live
/// slot would rename an adapter the appliance is configured against; naming
/// an absent MAC would write a second ghost.
///
/// Returns the kernel name the adapter still answers to, so the caller can
/// rename it in place; the pin alone only takes effect at the next boot.
pub fn adopt(roots: &PinRoots, slot: u32, mac: &str) -> Result<String> {
    let mac = lower(mac);
    let report = report(roots);

    let orphan = report
        .orphaned
        .iter()
        .find(|pin| pin.slot == slot)
        .ok_or_else(|| {
            NetError::Conflict(format!(
                "nic{slot} is not an orphaned name. A name is only adopted when the card behind \
                 it has been removed — this one still has its adapter, and renaming it would \
                 take every profile built on it with it."
            ))
        })?;

    let adapter = report
        .unclaimed
        .iter()
        .find(|candidate| candidate.mac == mac)
        .ok_or_else(|| {
            NetError::Conflict(format!(
                "no unclaimed adapter on this node has the address {mac}. Only an adapter that \
                 is present and that no other name already claims can be adopted."
            ))
        })?;

    let body = format!(
        "# Written by lumen-nicnames. MAC pin is authoritative; do not renumber.\n\
         # Adopted into nic{slot} after the card pinned as {} was replaced.\n\
         [Match]\n\
         PermanentMACAddress={mac}\n\
         \n\
         [Link]\n\
         Name=nic{slot}\n\
         AlternativeName={}\n",
        orphan.mac, adapter.device
    );
    std::fs::write(roots.pin_path(slot), body).map_err(|err| {
        NetError::backend(anyhow::anyhow!(
            "the name pin for nic{slot} could not be written: {err}"
        ))
    })?;
    Ok(adapter.device.clone())
}

/// Rename a live adapter, so an adoption does not wait for a reboot.
///
/// Safe exactly while the adapter is unconfigured, which is what being
/// unclaimed means: nothing has a profile bound to a name it does not yet
/// have. The link is brought down first because the kernel refuses to
/// rename a running interface.
pub fn rename_now(device: &str, slot: u32) -> Result<()> {
    let target = format!("nic{slot}");
    if device == target {
        return Ok(());
    }
    let run = |args: &[&str]| -> Result<()> {
        let output = std::process::Command::new("ip")
            .args(args)
            .output()
            .map_err(|err| NetError::backend(anyhow::anyhow!("could not run ip: {err}")))?;
        if !output.status.success() {
            return Err(NetError::backend(anyhow::anyhow!(
                "ip {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    };
    run(&["link", "set", device, "down"])?;
    run(&["link", "set", device, "name", &target])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A node with two pinned adapters, one of which has been replaced by a
    /// card the pins know nothing about.
    fn node(tag: &str) -> PinRoots {
        let root = std::env::temp_dir().join(format!(
            "lumen-net-pins-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        let roots = PinRoots::under(&root);
        fs::create_dir_all(&roots.link_dir).unwrap();

        let pin = |slot: u32, mac: &str, alt: &str| {
            fs::write(
                roots.link_dir.join(format!("70-lumen-nic{slot}.link")),
                format!(
                    "# Written by lumen-nicnames.\n[Match]\nPermanentMACAddress={mac}\n\n\
                     [Link]\nName=nic{slot}\nAlternativeName={alt}\n"
                ),
            )
            .unwrap();
        };
        // nic0's card is here; nic2's was pulled.
        pin(0, "aa:bb:cc:00:00:01", "eno1");
        pin(2, "aa:bb:cc:00:00:99", "enp129s0f0");

        let adapter = |name: &str, mac: &str, carrier: &str, speed: &str| {
            let dir = roots.sys_class_net.join(name);
            fs::create_dir_all(dir.join("device")).unwrap();
            fs::write(dir.join("address"), format!("{mac}\n")).unwrap();
            fs::write(dir.join("type"), "1\n").unwrap();
            fs::write(dir.join("carrier"), format!("{carrier}\n")).unwrap();
            fs::write(dir.join("speed"), format!("{speed}\n")).unwrap();
        };
        adapter("nic0", "aa:bb:cc:00:00:01", "1", "1000");
        // The replacement, still under its kernel name.
        adapter("enp1s0f0", "aa:bb:cc:00:00:aa", "1", "10000");
        adapter("enp1s0f1", "aa:bb:cc:00:00:ab", "0", "-1");
        // The BMC's USB gadget: passes the ethernet test, leads to the
        // service processor. Must never be offered.
        adapter("enp5s0f3u2u1c2", "be:3a:f2:00:00:99", "1", "-1");
        let gadget = roots.sys_class_net.join("enp5s0f3u2u1c2/device");
        let driver_dir = roots.sys_class_net.join("drivers/rndis_host");
        fs::create_dir_all(&driver_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&driver_dir, gadget.join("driver")).unwrap();
        // A virtual link: no device directory, so not an adapter.
        let bond = roots.sys_class_net.join("bond0");
        fs::create_dir_all(&bond).unwrap();
        fs::write(bond.join("address"), "aa:bb:cc:00:00:aa\n").unwrap();
        fs::write(bond.join("type"), "1\n").unwrap();

        roots
    }

    /// A bonded port answers to its bond's address while enslaved. Its pin
    /// is written against its own, so the pin must be read against the
    /// permanent address the kernel keeps — the live failure this guards:
    /// a healthy Core bond's second port reported as a missing card.
    #[test]
    fn an_enslaved_port_is_not_a_missing_card() {
        let roots = node("enslaved");
        // nic0 joins a bond and takes the bond's address, as the kernel
        // does; its permanent one stays where the pin can find it.
        let dir = roots.sys_class_net.join("nic0");
        fs::write(dir.join("address"), "aa:bb:cc:00:00:f0\n").unwrap();
        fs::create_dir_all(dir.join("bonding_slave")).unwrap();
        fs::write(dir.join("bonding_slave/perm_hwaddr"), "aa:bb:cc:00:00:01\n").unwrap();

        let report = report(&roots);
        assert!(
            report.orphaned.iter().all(|pin| pin.slot != 0),
            "an enslaved port was called a missing card: {report:?}"
        );
        assert!(
            !report
                .unclaimed
                .iter()
                .any(|adapter| adapter.device == "nic0"),
            "and it must not be offered as an unclaimed adapter either"
        );
    }

    #[test]
    fn a_replaced_card_leaves_its_name_orphaned_and_the_newcomer_unclaimed() {
        let roots = node("report");
        let report = report(&roots);

        assert_eq!(report.orphaned.len(), 1, "{report:?}");
        assert_eq!(report.orphaned[0].slot, 2);
        assert_eq!(report.orphaned[0].mac, "aa:bb:cc:00:00:99");

        // Both new ports, and neither the pinned nic0 nor the bond.
        let devices: Vec<&str> = report.unclaimed.iter().map(|a| a.device.as_str()).collect();
        assert_eq!(devices, vec!["enp1s0f0", "enp1s0f1"]);
        // The facts an operator picks with: which one has a cable in it.
        assert!(report.unclaimed[0].carrier);
        assert_eq!(report.unclaimed[0].speed_mbps, Some(10000));
        assert!(!report.unclaimed[1].carrier);
        assert_eq!(report.unclaimed[1].speed_mbps, None);
    }

    #[test]
    fn adopting_rewrites_the_pin_and_the_slot_stops_being_orphaned() {
        let roots = node("adopt");
        let device = adopt(&roots, 2, "AA:BB:CC:00:00:AA").unwrap();
        assert_eq!(device, "enp1s0f0", "the caller renames what it is told");

        let pins = pins(&roots);
        let adopted = pins.iter().find(|pin| pin.slot == 2).unwrap();
        assert_eq!(adopted.mac, "aa:bb:cc:00:00:aa");
        assert!(adopted.present, "the slot now names hardware that is here");
        assert_eq!(adopted.altname.as_deref(), Some("enp1s0f0"));

        // And nothing is orphaned any more, while the other newcomer stays
        // unclaimed rather than being swept along.
        let report = report(&roots);
        assert!(report.orphaned.is_empty());
        assert_eq!(report.unclaimed.len(), 1);
        assert_eq!(report.unclaimed[0].device, "enp1s0f1");
    }

    /// The two refusals that keep an adoption from being a rename of live
    /// hardware or a second ghost.
    #[test]
    fn a_live_slot_and_an_absent_adapter_are_both_refused() {
        let roots = node("refusals");
        // nic0's card is present: adopting into it would rename an adapter
        // the appliance is already configured against.
        assert!(adopt(&roots, 0, "aa:bb:cc:00:00:aa").is_err());
        // A MAC no adapter here has.
        assert!(adopt(&roots, 2, "de:ad:be:ef:00:01").is_err());
        // An adapter that another pin already claims.
        assert!(adopt(&roots, 2, "aa:bb:cc:00:00:01").is_err());
        // Nothing was written by any of them.
        assert_eq!(pins(&roots).len(), 2);
        assert_eq!(report(&roots).orphaned.len(), 1);
    }

    #[test]
    fn a_node_with_no_pins_at_all_reports_nothing_rather_than_failing() {
        let roots = PinRoots::under(std::env::temp_dir().join("lumen-net-pins-nowhere"));
        assert!(pins(&roots).is_empty());
        assert_eq!(report(&roots), PinReport::default());
    }
}
