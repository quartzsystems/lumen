//! Install configuration: the four operator decisions plus build pins.

use serde::{Deserialize, Serialize};

/// Everything the engine needs to perform an install.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallConfig {
    /// crypt(3) SHA-512 hash (`$6$...`), applied with `chpasswd -e`.
    pub root_password_hash: String,
    /// IANA zone name, e.g. "America/New_York" or "UTC".
    pub timezone: String,
    /// Management NIC name as shown in the live env (nic0..nicN).
    pub nic: String,
    pub network: NetworkConfig,
    /// Whole-disk target device path, e.g. "/dev/sda" or "/dev/nvme0n1".
    pub disk: String,
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
    fn config_json_roundtrip() {
        let cfg = InstallConfig {
            root_password_hash: "$6$salt$hash".into(),
            timezone: "UTC".into(),
            nic: "nic0".into(),
            network: NetworkConfig::Static {
                cidr: "192.168.10.5/24".into(),
                gateway: "192.168.10.1".into(),
                dns: vec!["9.9.9.9".into()],
            },
            disk: "/dev/nvme0n1".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: InstallConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.disk, "/dev/nvme0n1");
        match back.network {
            NetworkConfig::Static { ref cidr, .. } => assert_eq!(cidr, "192.168.10.5/24"),
            _ => panic!("expected static config"),
        }
    }
}
