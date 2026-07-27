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
    /// The management NIC's hardware address, as read from
    /// /sys/class/net/<nic>/address — the same value lumen-nicnames pins names
    /// to. It is written into the management bridge so the bridge keeps this
    /// address instead of inheriting the lowest MAC among its ports; see
    /// docs/networking.md.
    pub nic_mac: String,
    pub network: NetworkConfig,
    /// Whole-disk target device paths, e.g. ["/dev/sda", "/dev/sdb"]. The
    /// first disk carries the EFI system partition and /boot; every disk
    /// contributes its third partition to the boot vdev.
    pub disks: Vec<String>,
    /// vdev layout of boot over `disks`.
    pub topology: PoolTopology,
}

/// ZFS vdev layout for the boot pool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolTopology {
    #[default]
    Single,
    Mirror,
    Raidz1,
    Raidz2,
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

    /// Whether this many drives can actually be built into this layout.
    ///
    /// Not simply `disks >= min_disks`: a single-disk pool is *exactly* one
    /// disk, and offering it for four would silently build a pool that used
    /// one of them. The Review button has always refused that; this is the
    /// same rule moved to where it stops the choice being made.
    pub fn fits(self, disks: usize) -> bool {
        match self {
            Self::Single => disks == 1,
            other => disks >= other.min_disks(),
        }
    }

    /// The layouts this many drives can be built into, in menu order.
    ///
    /// With nothing selected there is nothing to narrow by, so the whole menu
    /// shows — that is the operator reading what the installer offers before
    /// they have picked anything for it to be measured against.
    pub fn options_for(disks: usize) -> Vec<PoolTopology> {
        if disks == 0 {
            return Self::ALL.to_vec();
        }
        Self::ALL
            .iter()
            .copied()
            .filter(|t| t.fits(disks))
            .collect()
    }

    /// What to build this many drives into, unless the operator says
    /// otherwise.
    ///
    /// The long-standing ZFS shape: two drives mirror, and past that the
    /// parity grows with the width, because the wider the group the longer a
    /// rebuild takes and the more likely a second drive goes during it.
    ///
    /// Mirrored in `recommendedVdev` in the console
    /// (lumen-webui/lib/storageClient.ts), which offers the same arrangements
    /// under their ZFS names and must give the same answer. The console also
    /// offers RAIDZ3, which the installer does not: a boot pool that wide is
    /// not something to set up on the way past.
    pub fn recommended_for(disks: usize) -> PoolTopology {
        match disks {
            0 | 1 => Self::Single,
            2 => Self::Mirror,
            3..=5 => Self::Raidz1,
            _ => Self::Raidz2,
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

    /// The menu never offers a layout the ticked drives cannot be built into,
    /// and one drive is offered exactly one choice.
    #[test]
    fn the_menu_offers_only_what_the_drives_can_build() {
        assert_eq!(PoolTopology::options_for(1), vec![PoolTopology::Single]);
        assert_eq!(PoolTopology::options_for(2), vec![PoolTopology::Mirror]);
        assert_eq!(
            PoolTopology::options_for(3),
            vec![PoolTopology::Mirror, PoolTopology::Raidz1]
        );
        assert_eq!(
            PoolTopology::options_for(4),
            vec![
                PoolTopology::Mirror,
                PoolTopology::Raidz1,
                PoolTopology::Raidz2
            ]
        );
        // A single-disk pool is exactly one disk, so it drops off the moment
        // there is a second — unlike every other minimum, which is a floor.
        assert!(!PoolTopology::options_for(2).contains(&PoolTopology::Single));
        // Nothing ticked narrows nothing.
        assert_eq!(PoolTopology::options_for(0), PoolTopology::ALL.to_vec());
    }

    #[test]
    fn the_recommendation_is_always_one_of_the_options() {
        for count in 1..=12 {
            let recommended = PoolTopology::recommended_for(count);
            assert!(
                PoolTopology::options_for(count).contains(&recommended),
                "{count} drives recommends {recommended:?}, which is not on offer"
            );
        }
    }

    #[test]
    fn the_recommendation_follows_the_drive_count() {
        assert_eq!(PoolTopology::recommended_for(1), PoolTopology::Single);
        assert_eq!(PoolTopology::recommended_for(2), PoolTopology::Mirror);
        assert_eq!(PoolTopology::recommended_for(3), PoolTopology::Raidz1);
        assert_eq!(PoolTopology::recommended_for(5), PoolTopology::Raidz1);
        assert_eq!(PoolTopology::recommended_for(6), PoolTopology::Raidz2);
        assert_eq!(PoolTopology::recommended_for(12), PoolTopology::Raidz2);
    }

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
            nic_mac: "52:54:00:aa:bb:00".into(),
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
        assert_eq!(back.nic_mac, "52:54:00:aa:bb:00");
        match back.network {
            NetworkConfig::Static { ref cidr, .. } => assert_eq!(cidr, "192.168.10.5/24"),
            _ => panic!("expected static config"),
        }
    }

    /// The control plane rewrites the management settings this installer
    /// writes, through `lumen_net::model::IpConfig`. The two crates do not
    /// depend on each other — the networking crate pulls in a D-Bus stack the
    /// installer has no business linking (docs/networking.md) — so this test
    /// and its mirror image in lumen-net
    /// (`ip_config_matches_installer_network_config`) pin the wire format
    /// both sides must keep producing.
    #[test]
    fn network_config_wire_format_matches_lumen_net() {
        assert_eq!(
            serde_json::to_value(NetworkConfig::Dhcp).unwrap(),
            serde_json::json!({ "mode": "dhcp" })
        );
        assert_eq!(
            serde_json::to_value(NetworkConfig::Static {
                cidr: "192.168.10.5/24".into(),
                gateway: "192.168.10.1".into(),
                dns: vec!["9.9.9.9".into()],
            })
            .unwrap(),
            serde_json::json!({
                "mode": "static",
                "cidr": "192.168.10.5/24",
                "gateway": "192.168.10.1",
                "dns": ["9.9.9.9"],
            })
        );
    }
}
