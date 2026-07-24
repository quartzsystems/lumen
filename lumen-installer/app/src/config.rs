//! Install configuration: the four operator decisions plus build pins.

use serde::{Deserialize, Serialize};

/// Everything the engine needs to perform an install.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallConfig {
    /// crypt(3) SHA-512 hash (`$6$...`), applied with `chpasswd -e`.
    pub root_password_hash: String,
    /// IANA zone name, e.g. "America/New_York" or "UTC".
    pub timezone: String,
    /// Console keymap for the installed system (/etc/vconsole.conf).
    pub keymap: String,
    /// Hostname or FQDN, e.g. "lumen01.example.lan".
    pub hostname: String,
    /// Management NIC name as shown in the live env (nic0..nicN).
    pub nic: String,
    pub network: NetworkConfig,
    /// Whole-disk target device paths, e.g. ["/dev/sda", "/dev/sdb"]. The
    /// first disk carries the EFI system partition and /boot; every disk
    /// contributes its third partition to the rpool vdev.
    pub disks: Vec<String>,
    /// vdev layout of rpool over `disks`.
    pub topology: PoolTopology,
}

/// ZFS vdev layout for the boot pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolTopology {
    Single,
    Mirror,
    Raidz1,
    Raidz2,
}

impl Default for PoolTopology {
    fn default() -> Self {
        Self::Single
    }
}

impl PoolTopology {
    /// Menu order in the UI.
    pub const ALL: [PoolTopology; 4] = [Self::Single, Self::Mirror, Self::Raidz1, Self::Raidz2];

    pub fn label(self) -> &'static str {
        match self {
            Self::Single => "Single disk",
            Self::Mirror => "Mirror (RAID1)",
            Self::Raidz1 => "RAIDZ1",
            Self::Raidz2 => "RAIDZ2",
        }
    }

    /// `zpool create` vdev keyword; a single disk is a plain top-level vdev.
    pub fn vdev_keyword(self) -> Option<&'static str> {
        match self {
            Self::Single => None,
            Self::Mirror => Some("mirror"),
            Self::Raidz1 => Some("raidz1"),
            Self::Raidz2 => Some("raidz2"),
        }
    }

    /// Practical minimums (raidz accepts fewer, but below these the layout
    /// is pointless), used for UI validation.
    pub fn min_disks(self) -> usize {
        match self {
            Self::Single => 1,
            Self::Mirror => 2,
            Self::Raidz1 => 3,
            Self::Raidz2 => 4,
        }
    }
}

/// Accepts a dotted-quad netmask ("255.255.240.0") or a bare prefix
/// length ("20") and returns the prefix, rejecting non-contiguous masks.
pub fn parse_netmask(text: &str) -> Option<u8> {
    let text = text.trim();
    if let Ok(prefix) = text.parse::<u8>() {
        return (1..=32).contains(&prefix).then_some(prefix);
    }
    let mask: std::net::Ipv4Addr = text.parse().ok()?;
    let bits = u32::from(mask);
    let prefix = bits.count_ones() as u8;
    if prefix == 0 || prefix > 32 {
        return None;
    }
    let expected = u32::MAX << (32 - prefix);
    (bits == expected).then_some(prefix)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum NetworkConfig {
    Dhcp,
    Static {
        /// Address in CIDR form, e.g. "192.168.10.5/24".
        cidr: String,
        gateway: String,
        dns: Vec<String>,
    },
}

/// Pins stamped into the live image by iso/build-live-iso.sh
/// (/etc/lumen-build.env). The kernel NEVR keeps the installed target on
/// exactly the kernel the on-media kmod-zfs was verified against.
#[derive(Debug, Clone, Default)]
pub struct BuildPins {
    pub kernel_nevr: Option<String>,
    pub lumen_version: String,
}

impl BuildPins {
    pub fn load() -> Self {
        Self::parse(&std::fs::read_to_string("/etc/lumen-build.env").unwrap_or_default())
    }

    pub fn parse(text: &str) -> Self {
        let mut pins = Self {
            lumen_version: "dev".into(),
            ..Self::default()
        };
        for line in text.lines() {
            let line = line.trim();
            if let Some((key, value)) = line.split_once('=') {
                let value = value.trim().trim_matches('"').to_string();
                match key.trim() {
                    "KERNEL_NEVR" if !value.is_empty() => pins.kernel_nevr = Some(value),
                    "LUMEN_VERSION" if !value.is_empty() => pins.lumen_version = value,
                    _ => {}
                }
            }
        }
        pins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_build_pins() {
        let pins = BuildPins::parse(
            "# comment\nKERNEL_NEVR=\"kernel-6.12.0-211.7.3.el10_2\"\nLUMEN_VERSION=0.1.0\n",
        );
        assert_eq!(
            pins.kernel_nevr.as_deref(),
            Some("kernel-6.12.0-211.7.3.el10_2")
        );
        assert_eq!(pins.lumen_version, "0.1.0");
    }

    #[test]
    fn netmask_parsing() {
        assert_eq!(parse_netmask("255.255.255.0"), Some(24));
        assert_eq!(parse_netmask("255.255.240.0"), Some(20));
        assert_eq!(parse_netmask("255.255.255.255"), Some(32));
        assert_eq!(parse_netmask("24"), Some(24));
        assert_eq!(parse_netmask("255.0.255.0"), None); // non-contiguous
        assert_eq!(parse_netmask("0.0.0.0"), None);
        assert_eq!(parse_netmask("0"), None);
        assert_eq!(parse_netmask("33"), None);
        assert_eq!(parse_netmask("garbage"), None);
    }

    #[test]
    fn config_json_roundtrip() {
        let cfg = InstallConfig {
            root_password_hash: "$6$salt$hash".into(),
            timezone: "UTC".into(),
            keymap: "us".into(),
            hostname: "lumen01.example.lan".into(),
            nic: "nic0".into(),
            network: NetworkConfig::Static {
                cidr: "192.168.10.5/24".into(),
                gateway: "192.168.10.1".into(),
                dns: vec!["9.9.9.9".into()],
            },
            disks: vec!["/dev/nvme0n1".into(), "/dev/nvme1n1".into()],
            topology: PoolTopology::Mirror,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: InstallConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.disks, vec!["/dev/nvme0n1", "/dev/nvme1n1"]);
        assert_eq!(back.topology, PoolTopology::Mirror);
        match back.network {
            NetworkConfig::Static { ref cidr, .. } => assert_eq!(cidr, "192.168.10.5/24"),
            _ => panic!("expected static config"),
        }
    }
}
