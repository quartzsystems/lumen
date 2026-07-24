//! Read-only system probing for the UI: NICs, disks, time zones, firmware
//! state. GTK-free and side-effect-free (except subprocess reads).

use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Nic {
    pub name: String,
    pub mac: String,
    pub link_up: bool,
    /// Mb/s when the link is up and the driver reports it.
    pub speed_mbps: Option<u32>,
    /// Kernel driver name (e.g. "e1000e", "vmxnet3").
    pub driver: String,
}

#[derive(Debug, Clone)]
pub struct Country {
    pub name: String,
    pub zones: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Disk {
    pub path: String,
    pub model: String,
    pub serial: String,
    pub size_bytes: u64,
    pub transport: String,
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Physical ethernet NICs, sorted by name (nic0..nicN after lumen-nicnames
/// has run in the live environment).
pub fn nics() -> Vec<Nic> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/net") else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !path.join("device").exists() {
            continue; // physical only
        }
        if read_trim(&path.join("type")).as_deref() != Some("1") {
            continue; // ethernet only
        }
        let Some(mac) = read_trim(&path.join("address")) else {
            continue;
        };
        // carrier reads EINVAL while the interface is down; treat as down.
        let link_up = read_trim(&path.join("carrier")).as_deref() == Some("1");
        let speed_mbps = if link_up {
            read_trim(&path.join("speed"))
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|s| u32::try_from(s).ok())
        } else {
            None
        };
        let driver = fs::read_link(path.join("device/driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();
        out.push(Nic {
            name,
            mac,
            link_up,
            speed_mbps,
            driver,
        });
    }
    out.sort_by(|a, b| natural_key(&a.name).cmp(&natural_key(&b.name)));
    out
}

/// Installable whole disks: fixed, writable, and not the live medium.
pub fn disks() -> Vec<Disk> {
    let Ok(output) = Command::new("lsblk")
        .args([
            "-J",
            "-b",
            "-d",
            "-o",
            "PATH,SIZE,MODEL,SERIAL,TYPE,RM,RO,TRAN",
        ])
        .output()
    else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };

    let live_source = live_medium_source();
    let mut out = Vec::new();
    for dev in json["blockdevices"].as_array().cloned().unwrap_or_default() {
        if dev["type"].as_str() != Some("disk") {
            continue;
        }
        if truthy(&dev["rm"]) || truthy(&dev["ro"]) {
            continue;
        }
        let Some(path) = dev["path"].as_str() else {
            continue;
        };
        if path.starts_with("/dev/zram") || path.starts_with("/dev/loop") {
            continue;
        }
        // Never offer the disk the live medium itself lives on.
        if live_source
            .as_deref()
            .is_some_and(|src| src.starts_with(path))
        {
            continue;
        }
        out.push(Disk {
            path: path.to_string(),
            model: dev["model"].as_str().unwrap_or("").trim().to_string(),
            serial: dev["serial"].as_str().unwrap_or("").trim().to_string(),
            size_bytes: dev["size"].as_u64().unwrap_or(0),
            transport: dev["tran"].as_str().unwrap_or("").to_string(),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Countries with their time zones (tzdata's iso3166.tab + zone1970.tab),
/// sorted by name, with "Other (UTC)" first as the neutral default.
pub fn countries() -> Vec<Country> {
    let iso = fs::read_to_string("/usr/share/zoneinfo/iso3166.tab").unwrap_or_default();
    let zone_tab = fs::read_to_string("/usr/share/zoneinfo/zone1970.tab").unwrap_or_default();
    parse_countries(&iso, &zone_tab)
}

pub fn parse_countries(iso: &str, zone_tab: &str) -> Vec<Country> {
    let mut code_to_name = std::collections::HashMap::new();
    for line in iso.lines().filter(|l| !l.trim_start().starts_with('#')) {
        if let Some((code, name)) = line.split_once('\t') {
            code_to_name.insert(code.trim(), name.trim());
        }
    }
    let mut by_name: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for line in zone_tab
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
    {
        let mut fields = line.split('\t');
        let (Some(codes), Some(_coords), Some(tz)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        for code in codes.split(',') {
            if let Some(name) = code_to_name.get(code.trim()) {
                by_name
                    .entry((*name).to_string())
                    .or_default()
                    .push(tz.to_string());
            }
        }
    }
    let mut out = vec![Country {
        name: "Other (UTC)".into(),
        zones: vec!["UTC".into()],
    }];
    out.extend(by_name.into_iter().map(|(name, mut zones)| {
        zones.sort();
        zones.dedup();
        Country { name, zones }
    }));
    out
}

/// Console keymaps offered by the installer (display label, kbd name).
pub fn keymaps() -> &'static [(&'static str, &'static str)] {
    &[
        ("English (US)", "us"),
        ("English (UK)", "uk"),
        ("Czech", "cz"),
        ("Danish", "dk"),
        ("Dutch", "nl"),
        ("Finnish", "fi"),
        ("French", "fr"),
        ("German", "de"),
        ("Italian", "it"),
        ("Japanese", "jp106"),
        ("Norwegian", "no"),
        ("Polish", "pl2"),
        ("Portuguese", "pt-latin1"),
        ("Portuguese (Brazil)", "br-abnt2"),
        ("Spanish", "es"),
        ("Swedish", "sv-latin1"),
    ]
}

pub fn is_uefi() -> bool {
    Path::new("/sys/firmware/efi").exists()
}

/// SecureBoot efivar: 4-byte attr header + 1 data byte (1 = enabled).
pub fn secure_boot_enabled() -> bool {
    let var = "/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c";
    matches!(fs::read(var), Ok(bytes) if bytes.last() == Some(&1))
}

/// crypt(3) SHA-512 hash via openssl (present in the live environment).
pub fn hash_password(password: &str) -> Result<String, String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("openssl")
        .args(["passwd", "-6", "-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("openssl: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("openssl: no stdin")?
        .write_all(password.as_bytes())
        .map_err(|e| format!("openssl: {e}"))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("openssl: {e}"))?;
    if !output.status.success() {
        return Err("openssl passwd failed".into());
    }
    let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if hash.starts_with("$6$") {
        Ok(hash)
    } else {
        Err("unexpected hash format from openssl".into())
    }
}

pub fn lumen_version() -> String {
    fs::read_to_string("/etc/lumen-release")
        .ok()
        .and_then(|s| s.lines().next().map(str::to_string))
        .unwrap_or_else(|| "Quartz Systems Lumen".to_string())
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn truthy(value: &serde_json::Value) -> bool {
    value.as_bool().unwrap_or(false)
        || value.as_str().is_some_and(|s| s == "1" || s == "true")
        || value.as_u64().is_some_and(|n| n == 1)
}

fn live_medium_source() -> Option<String> {
    let output = Command::new("findmnt")
        .args(["-n", "-o", "SOURCE", "/run/initramfs/live"])
        .output()
        .ok()?;
    let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!source.is_empty()).then_some(source)
}

/// "nic10" sorts after "nic2".
fn natural_key(name: &str) -> (String, u64) {
    let digits: String = name
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let digits: String = digits.chars().rev().collect();
    let prefix = name[..name.len() - digits.len()].to_string();
    (prefix, digits.parse().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_parsing() {
        let iso = "# comment\nUS\tUnited States\nFR\tFrance\nAU\tAustralia\nNZ\tNew Zealand\n";
        let tab = "# comment\n\
                   US\t+404251-0740023\tAmerica/New_York\tEastern\n\
                   US\t+340308-1181434\tAmerica/Los_Angeles\tPacific\n\
                   FR\t+4852+00220\tEurope/Paris\n\
                   AU,NZ\t-3352+15113\tAustralia/Sydney\tNSW\n";
        let countries = parse_countries(iso, tab);
        assert_eq!(countries[0].name, "Other (UTC)");
        assert_eq!(countries[0].zones, vec!["UTC".to_string()]);
        let us = countries
            .iter()
            .find(|c| c.name == "United States")
            .unwrap();
        assert_eq!(us.zones.len(), 2);
        assert!(us.zones.contains(&"America/New_York".to_string()));
        let nz = countries.iter().find(|c| c.name == "New Zealand").unwrap();
        assert_eq!(nz.zones, vec!["Australia/Sydney".to_string()]);
        // Sorted by name after the UTC entry.
        assert!(countries[1].name < countries[2].name);
    }

    #[test]
    fn nic_natural_sort() {
        let mut names = vec!["nic10", "nic2", "nic0", "nic1"];
        names.sort_by(|a, b| natural_key(a).cmp(&natural_key(b)));
        assert_eq!(names, vec!["nic0", "nic1", "nic2", "nic10"]);
    }

    #[test]
    fn sizes_are_humanized() {
        assert_eq!(human_size(512), "512.0 B");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
        assert_eq!(human_size(2_000_398_934_016), "1.8 TiB");
    }
}
