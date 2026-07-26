//! The NetworkManager backend: Lumen's plans, applied over the system bus.
//!
//! Why NetworkManager and not netlink or a privileged helper: NM already owns
//! the management connection the installer wrote, it performs the privileged
//! filesystem and sysctl work in *its own* process, and it implements the
//! checkpoint/auto-rollback mechanism this whole design rests on. That keeps
//! `lumen-controlplane.service`'s `ProtectSystem=strict` and
//! `ProtectKernelTunables=yes` intact — talking to
//! /run/dbus/system_bus_socket needs neither relaxed. See docs/networking.md.

pub mod proxy;
pub mod settings;

use std::collections::HashMap;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::Connection as Bus;

use crate::backend::{CheckpointId, NetworkBackend};
use crate::error::{NetError, Result};
use crate::model::{BondMode, Duplex, LinkKind};
use crate::plan::Op;
use crate::state::{LinkState, ObservedLink, ObservedState};

use proxy::{
    ActiveConnectionProxy, ConnectionProxy, DeviceProxy, DeviceWiredProxy, Ip4ConfigProxy,
    NetworkManagerProxy, SettingsMap, SettingsProxy,
};

/// NM_DEVICE_TYPE_*, the values Lumen cares about.
const DEVICE_TYPE_ETHERNET: u32 = 1;
const DEVICE_TYPE_BOND: u32 = 10;
const DEVICE_TYPE_VLAN: u32 = 11;
const DEVICE_TYPE_BRIDGE: u32 = 13;

/// Drivers whose devices are not front-panel adapters and must not appear in
/// observed state at all. A BMC presents its shared-with-host side as a USB
/// gadget, and NM sees an ordinary ethernet device with a MAC and a carrier.
/// Showing it in the console's Interfaces table is the visible problem; the
/// quieter one is `state::derive_desired`, which seeds `desired.json` from
/// observed state on first boot and would write it in as a `nic` the operator
/// never has any reason to configure — and, on a box where something has
/// addressed it, could pick it as `management.link`.
///
/// Kept in step with the same list in `lumen-nicnames`, which skips these when
/// assigning `nic<N>` names, and in the installer's `sysinfo::nics`.
const EXCLUDED_DRIVERS: [&str; 4] = ["rndis_host", "cdc_ether", "cdc_ncm", "cdc_eem"];

/// NM_DEVICE_STATE_*.
const STATE_UNMANAGED: u32 = 10;
const STATE_UNAVAILABLE: u32 = 20;
const STATE_DISCONNECTED: u32 = 30;
const STATE_ACTIVATED: u32 = 100;
const STATE_DEACTIVATING: u32 = 110;

/// Checkpoint flags, chosen deliberately (docs/networking.md):
///
///   DELETE_NEW_CONNECTIONS (0x02) — a rollback must remove the profiles the
///     apply created, otherwise a half-built bridge autoconnects after the
///     revert and the box comes back in the broken state we just undid.
///   DISCONNECT_NEW_DEVICES (0x04) — likewise for the devices those profiles
///     brought into existence.
///
/// Deliberately NOT set:
///   DESTROY_ALL (0x01) — would tear down every other checkpoint on the box.
///   ALLOW_OVERLAPPING (0x08) — a second overlapping checkpoint is exactly
///     the concurrent-apply case the service already refuses; letting NM
///     accept it would hide the conflict.
const CHECKPOINT_FLAGS: u32 = 0x02 | 0x04;

/// The first NetworkManager release that understands `connection.controller`
/// and `connection.port-type`. Older daemons silently ignore them, which
/// would produce a bridge with no ports — hence a hard startup check rather
/// than a runtime surprise.
const MIN_NM_VERSION: (u32, u32) = (1, 46);

pub struct NmBackend {
    bus: Bus,
}

impl NmBackend {
    /// Connect to the system bus and confirm the daemon is new enough.
    pub async fn connect() -> Result<Self> {
        let bus = Bus::system()
            .await
            .context("connecting to the system bus (is dbus-daemon running?)")?;
        let backend = Self { bus };
        backend.check_version().await?;
        Ok(backend)
    }

    async fn check_version(&self) -> Result<()> {
        let nm = self.nm().await?;
        let version = nm
            .version()
            .await
            .context("reading the NetworkManager version")?;
        let parsed: Vec<u32> = version
            .split('.')
            .take(2)
            .map(|part| part.parse().unwrap_or(0))
            .collect();
        let found = (
            parsed.first().copied().unwrap_or(0),
            parsed.get(1).copied().unwrap_or(0),
        );
        if found < MIN_NM_VERSION {
            return Err(NetError::Backend(anyhow!(
                "NetworkManager {version} is too old — Lumen needs {}.{} or newer for \
                 controller/port-type connection settings",
                MIN_NM_VERSION.0,
                MIN_NM_VERSION.1
            )));
        }
        tracing::info!(version = %version, "NetworkManager backend ready");
        Ok(())
    }

    async fn nm(&self) -> Result<NetworkManagerProxy<'_>> {
        Ok(NetworkManagerProxy::new(&self.bus).await?)
    }

    async fn settings(&self) -> Result<SettingsProxy<'_>> {
        Ok(SettingsProxy::new(&self.bus).await?)
    }

    async fn connection_at(&self, path: &ObjectPath<'_>) -> Result<ConnectionProxy<'_>> {
        Ok(ConnectionProxy::builder(&self.bus)
            .path(path.to_owned())?
            .build()
            .await?)
    }

    /// Every settings connection, paired with its profile — the lookup the
    /// planner's `uuid: None` fallback and the observer both need.
    async fn all_connections(&self) -> Result<Vec<(OwnedObjectPath, SettingsMap)>> {
        let settings = self.settings().await?;
        let mut out = Vec::new();
        for path in settings.list_connections().await? {
            let proxy = self.connection_at(&path.as_ref()).await?;
            match proxy.get_settings().await {
                Ok(map) => out.push((path, map)),
                // A profile can vanish between the list and the read; that is
                // not an error, there is just nothing to report about it.
                Err(err) => tracing::debug!(%path, "skipping unreadable connection: {err}"),
            }
        }
        Ok(out)
    }

    /// Resolve a connection profile by uuid, falling back to the interface
    /// name it is bound to.
    async fn find_connection(
        &self,
        uuid: Option<&str>,
        interface: &str,
    ) -> Result<Option<OwnedObjectPath>> {
        if let Some(uuid) = uuid {
            if let Ok(path) = self.settings().await?.get_connection_by_uuid(uuid).await {
                return Ok(Some(path));
            }
        }
        for (path, map) in self.all_connections().await? {
            if settings::get_str(&map, settings::CONNECTION, "interface-name") == Some(interface) {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    /// Device object path for an interface name, when the device exists.
    async fn device_path(&self, interface: &str) -> Result<Option<OwnedObjectPath>> {
        for path in self.nm().await?.get_all_devices().await? {
            let device = self.device_at(&path.as_ref()).await?;
            if device.interface().await.ok().as_deref() == Some(interface) {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    async fn device_at(&self, path: &ObjectPath<'_>) -> Result<DeviceProxy<'_>> {
        Ok(DeviceProxy::builder(&self.bus)
            .path(path.to_owned())?
            .build()
            .await?)
    }

    async fn observe_device(
        &self,
        path: &OwnedObjectPath,
        altnames: &HashMap<String, String>,
    ) -> Result<Option<ObservedLink>> {
        let device = self.device_at(&path.as_ref()).await?;
        let Ok(name) = device.interface().await else {
            return Ok(None);
        };
        let device_type = device.device_type().await.unwrap_or(0);
        let kind = match device_type {
            DEVICE_TYPE_ETHERNET => LinkKind::Ethernet,
            DEVICE_TYPE_BOND => LinkKind::Bond,
            DEVICE_TYPE_BRIDGE => LinkKind::Bridge,
            DEVICE_TYPE_VLAN => LinkKind::Vlan,
            _ => LinkKind::Other,
        };
        // Only physical devices can be a BMC gadget; a bond or bridge reports
        // no driver worth checking, and skipping one Lumen created would make
        // its own configuration invisible.
        if kind == LinkKind::Ethernet {
            let driver = device.driver().await.unwrap_or_default();
            if EXCLUDED_DRIVERS.contains(&driver.as_str()) {
                tracing::debug!(%name, %driver, "skipping BMC/IPMI USB gadget");
                return Ok(None);
            }
        }
        let raw_state = device.state().await.unwrap_or(0);
        let state = match raw_state {
            STATE_ACTIVATED => LinkState::Activated,
            STATE_UNMANAGED => LinkState::Unmanaged,
            STATE_DISCONNECTED | STATE_UNAVAILABLE => LinkState::Disconnected,
            STATE_DEACTIVATING => LinkState::Disconnected,
            0 => LinkState::Unknown,
            // Everything between DISCONNECTED and ACTIVATED is the
            // activation ladder; the console shows one "coming up".
            _ => LinkState::Activating,
        };

        let mut link = ObservedLink {
            name: name.clone(),
            altname: altnames.get(&name).cloned(),
            kind,
            state,
            managed: device.managed().await.unwrap_or(false),
            mtu: device.mtu().await.ok().filter(|mtu| *mtu > 0),
            ..ObservedLink::default()
        };

        // Wired properties exist for every device NM considers ethernet-like,
        // which includes bonds, bridges, and VLANs.
        if let Ok(wired) = DeviceWiredProxy::builder(&self.bus)
            .path(path.as_ref().to_owned())?
            .build()
            .await
        {
            link.perm_mac = wired.perm_hw_address().await.ok().filter(is_real_mac);
            link.mac = wired.hw_address().await.ok().filter(is_real_mac);
            link.carrier = wired.carrier().await.unwrap_or(false);
            link.speed_mbps = wired.speed().await.ok().filter(|speed| *speed > 0);
            if link.speed_mbps.is_some() {
                // NM reports speed but not duplex; a link that negotiated a
                // speed at all is full duplex on anything Lumen supports.
                link.duplex = Some(Duplex::Full);
            }
        }

        if let Ok(ip4) = device.ip4_config().await {
            if ip4.as_str() != "/" {
                if let Ok(config) = Ip4ConfigProxy::builder(&self.bus)
                    .path(ip4.clone())?
                    .build()
                    .await
                {
                    link.addresses = config
                        .address_data()
                        .await
                        .unwrap_or_default()
                        .iter()
                        .filter_map(address_entry)
                        .collect();
                    link.gateway = config
                        .gateway()
                        .await
                        .ok()
                        .filter(|gateway| !gateway.is_empty());
                }
            }
        }

        // Profile-derived facts: uuid/id, controller, VLAN and bond details.
        // Prefer the active connection; fall back to the stored profile bound
        // to this interface so an inactive-but-configured link still reports
        // what it is.
        let mut profile: Option<SettingsMap> = None;
        if let Ok(active) = device.active_connection().await {
            if active.as_str() != "/" {
                if let Ok(proxy) = ActiveConnectionProxy::builder(&self.bus)
                    .path(active.clone())?
                    .build()
                    .await
                {
                    link.connection_uuid = proxy.uuid().await.ok();
                    link.connection_id = proxy.id().await.ok();
                    if let Ok(settings_path) = proxy.connection().await {
                        if let Ok(connection) = self.connection_at(&settings_path.as_ref()).await {
                            profile = connection.get_settings().await.ok();
                        }
                    }
                }
            }
        }
        if profile.is_none() {
            for (_, map) in self.all_connections().await? {
                if settings::get_str(&map, settings::CONNECTION, "interface-name") == Some(&name) {
                    link.connection_uuid =
                        settings::get_str(&map, settings::CONNECTION, "uuid").map(String::from);
                    link.connection_id =
                        settings::get_str(&map, settings::CONNECTION, "id").map(String::from);
                    profile = Some(map);
                    break;
                }
            }
        }
        if let Some(profile) = profile {
            link.controller = settings::get_controller(&profile).map(String::from);
            // What the profile *asks for*, which is not the same question as
            // what `addresses` above reports: a DHCP link with a lease and a
            // static link with the same address look identical on the wire.
            link.ip = settings::get_ip(&profile);
            if kind == LinkKind::Vlan {
                link.vlan_id = settings::get_u32(&profile, settings::VLAN, "id")
                    .and_then(|id| u16::try_from(id).ok());
                link.parent =
                    settings::get_str(&profile, settings::VLAN, "parent").map(String::from);
            }
            if kind == LinkKind::Bond {
                link.bond_mode = settings::get_bond_option(&profile, "mode").and_then(|mode| {
                    match mode.as_str() {
                        "active-backup" => Some(BondMode::ActiveBackup),
                        "802.3ad" => Some(BondMode::Ieee8023ad),
                        "balance-xor" => Some(BondMode::BalanceXor),
                        _ => None,
                    }
                });
            }
        }

        Ok(Some(link))
    }

    async fn apply_one(&self, op: &Op) -> Result<()> {
        match op {
            Op::AddConnection { spec } => {
                let map = settings::to_settings(spec, None)
                    .with_context(|| format!("building settings for {}", spec.interface))?;
                self.settings()
                    .await?
                    .add_connection(map)
                    .await
                    .with_context(|| format!("adding connection for {}", spec.interface))?;
            }
            Op::UpdateConnection { uuid, spec } => {
                let path = self
                    .find_connection(uuid.as_deref(), &spec.interface)
                    .await?;
                match path {
                    Some(path) => {
                        // Keep the profile's identity: NM matches the keyfile
                        // by uuid, so writing a fresh one would leave the old
                        // file behind, autoconnecting.
                        let connection = self.connection_at(&path.as_ref()).await?;
                        let existing = connection.get_settings().await.ok();
                        let uuid = existing
                            .as_ref()
                            .and_then(|map| settings::get_str(map, settings::CONNECTION, "uuid"))
                            .map(String::from)
                            .or_else(|| uuid.clone());
                        let map = settings::to_settings(spec, uuid.as_deref())
                            .with_context(|| format!("building settings for {}", spec.interface))?;
                        connection
                            .update(map)
                            .await
                            .with_context(|| format!("updating {}", spec.interface))?;
                    }
                    None => {
                        // The profile went away between planning and applying.
                        // Creating it is what the plan meant.
                        let map = settings::to_settings(spec, None)?;
                        self.settings().await?.add_connection(map).await?;
                    }
                }
            }
            Op::DeleteConnection { uuid, interface } => {
                if let Some(path) = self.find_connection(uuid.as_deref(), interface).await? {
                    self.connection_at(&path.as_ref())
                        .await?
                        .delete()
                        .await
                        .with_context(|| format!("deleting the profile for {interface}"))?;
                }
            }
            Op::ActivateConnection { interface } => {
                let connection = self
                    .find_connection(None, interface)
                    .await?
                    .ok_or_else(|| NetError::NotFound(format!("no profile for {interface}")))?;
                // "/" for the device: a bridge or bond that does not exist yet
                // is created by the activation itself.
                let device = self.device_path(interface).await?;
                let root = ObjectPath::try_from("/").expect("\"/\" is a valid object path");
                let device_path = match &device {
                    Some(path) => path.as_ref(),
                    None => root.as_ref(),
                };
                self.nm()
                    .await?
                    .activate_connection(&connection.as_ref(), &device_path, &root)
                    .await
                    .with_context(|| format!("activating {interface}"))?;
            }
            Op::DeactivateConnection { interface } => {
                let Some(path) = self.device_path(interface).await? else {
                    return Ok(());
                };
                let device = self.device_at(&path.as_ref()).await?;
                let Ok(active) = device.active_connection().await else {
                    return Ok(());
                };
                if active.as_str() == "/" {
                    return Ok(());
                }
                self.nm()
                    .await?
                    .deactivate_connection(&active.as_ref())
                    .await
                    .with_context(|| format!("deactivating {interface}"))?;
            }
        }
        Ok(())
    }
}

/// NM reports an empty string, and some drivers a zeroed address, when there
/// is no hardware address to report.
fn is_real_mac(mac: &String) -> bool {
    !mac.is_empty() && mac != "00:00:00:00:00:00"
}

/// One `AddressData` entry -> "192.168.10.5/24".
fn address_entry(entry: &HashMap<String, zbus::zvariant::OwnedValue>) -> Option<String> {
    let address = entry
        .get("address")
        .and_then(|v| <&str>::try_from(v).ok())?;
    let prefix = entry.get("prefix").and_then(|v| u32::try_from(v).ok())?;
    Some(format!("{address}/{prefix}"))
}

#[async_trait]
impl NetworkBackend for NmBackend {
    async fn observe(&self) -> Result<ObservedState> {
        let node = hostname();
        let altnames = altnames(std::path::Path::new(LINK_DIR));
        let mut links = Vec::new();
        for path in self.nm().await?.get_all_devices().await? {
            if let Some(link) = self.observe_device(&path, &altnames).await? {
                links.push(link);
            }
        }
        // Port lists are derived from the ports' own controller field so the
        // two views can never disagree.
        let pairs: Vec<(String, String)> = links
            .iter()
            .filter_map(|l| l.controller.clone().map(|c| (c, l.name.clone())))
            .collect();
        for link in &mut links {
            link.ports = pairs
                .iter()
                .filter(|(controller, _)| controller == &link.name)
                .map(|(_, port)| port.clone())
                .collect();
            link.ports.sort();
        }
        links.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ObservedState { node, links })
    }

    async fn apply(&self, ops: &[Op]) -> Result<()> {
        for op in ops {
            tracing::info!(op = %op, "applying");
            self.apply_one(op).await?;
        }
        // Make sure NM has re-read anything written straight to disk by an
        // earlier stage (the installer's keyfiles) before we hand control back.
        let _ = self.settings().await?.reload_connections().await;
        Ok(())
    }

    async fn checkpoint_create(&self, rollback_secs: u32) -> Result<CheckpointId> {
        let path = self
            .nm()
            .await?
            .checkpoint_create(&[], rollback_secs, CHECKPOINT_FLAGS)
            .await
            .context("creating a NetworkManager checkpoint")?;
        Ok(CheckpointId(path.as_str().to_string()))
    }

    async fn checkpoint_destroy(&self, id: &CheckpointId) -> Result<()> {
        let path = object_path(id)?;
        self.nm()
            .await?
            .checkpoint_destroy(&path)
            .await
            .context("destroying the checkpoint")?;
        Ok(())
    }

    async fn checkpoint_rollback(&self, id: &CheckpointId) -> Result<()> {
        let path = object_path(id)?;
        let results = self
            .nm()
            .await?
            .checkpoint_rollback(&path)
            .await
            .context("rolling back the checkpoint")?;
        for (device, result) in results {
            if result != 0 {
                tracing::warn!(device = %device, result, "device did not roll back cleanly");
            }
        }
        Ok(())
    }

    async fn checkpoint_extend(&self, id: &CheckpointId, add_secs: u32) -> Result<()> {
        let path = object_path(id)?;
        self.nm()
            .await?
            .checkpoint_adjust_rollback_timeout(&path, add_secs)
            .await
            .context("extending the checkpoint window")?;
        Ok(())
    }

    async fn checkpoints(&self) -> Result<Vec<CheckpointId>> {
        Ok(self
            .nm()
            .await?
            .checkpoints()
            .await?
            .into_iter()
            .map(|path| CheckpointId(path.as_str().to_string()))
            .collect())
    }
}

fn object_path(id: &CheckpointId) -> Result<ObjectPath<'_>> {
    ObjectPath::try_from(id.0.as_str())
        .map_err(|err| NetError::Backend(anyhow!("bad checkpoint id {}: {err}", id.0)))
}

/// Where lumen-nicnames writes its `70-lumen-nic<N>.link` files.
const LINK_DIR: &str = "/etc/systemd/network";

/// `nicN` -> the name the kernel gave the adapter before udev renamed it.
///
/// Read from lumen-nicnames' own link files rather than from the kernel: an
/// alternative name is not exposed in sysfs, and the netlink property list is
/// a whole dependency for one string per NIC. The tool that does the renaming
/// is also the one that knows what the name used to be, so it records it —
/// `AlternativeName=` for udev, and this reads the same line back.
///
/// A missing or unreadable directory is not an error: the column is simply
/// empty, which is also the honest answer on a box whose NICs were never
/// renamed.
fn altnames(dir: &std::path::Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_lumen_link = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("70-lumen-nic") && name.ends_with(".link"));
        if !is_lumen_link {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut name = None;
        let mut altname = None;
        for line in text.lines().map(str::trim) {
            if let Some(value) = line.strip_prefix("Name=") {
                name = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("AlternativeName=") {
                altname = Some(value.to_string());
            }
        }
        if let (Some(name), Some(altname)) = (name, altname) {
            out.insert(name, altname);
        }
    }
    out
}

/// The node name observed state is grouped by. `/etc/hostname` is not read —
/// `ProtectSystem=strict` leaves /etc readable, but the kernel's own value is
/// the one that matters and needs no file at all.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "lumen".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_flags_delete_new_connections_and_disconnect_new_devices() {
        assert_eq!(CHECKPOINT_FLAGS, 6);
        assert_eq!(CHECKPOINT_FLAGS & 0x01, 0, "DESTROY_ALL must stay off");
        assert_eq!(
            CHECKPOINT_FLAGS & 0x08,
            0,
            "ALLOW_OVERLAPPING must stay off"
        );
    }

    #[test]
    fn address_entries_become_cidr() {
        let mut entry: HashMap<String, zbus::zvariant::OwnedValue> = HashMap::new();
        entry.insert(
            "address".into(),
            zbus::zvariant::Value::from("192.168.10.5")
                .try_into()
                .unwrap(),
        );
        entry.insert(
            "prefix".into(),
            zbus::zvariant::Value::from(24u32).try_into().unwrap(),
        );
        assert_eq!(address_entry(&entry).as_deref(), Some("192.168.10.5/24"));
    }

    #[test]
    fn empty_and_zero_macs_are_not_real() {
        assert!(!is_real_mac(&String::new()));
        assert!(!is_real_mac(&"00:00:00:00:00:00".to_string()));
        assert!(is_real_mac(&"52:54:00:aa:bb:00".to_string()));
    }

    #[test]
    fn a_hostname_is_always_reported() {
        assert!(!hostname().is_empty());
    }

    #[test]
    fn alternative_names_come_out_of_the_link_files() {
        let dir = std::env::temp_dir().join(format!(
            "lumen-net-altnames-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("70-lumen-nic0.link"),
            "[Match]\nPermanentMACAddress=52:54:00:aa:bb:00\n\n\
             [Link]\nName=nic0\nAlternativeName=enp1s0\n",
        )
        .unwrap();
        // A NIC that was already called nicN has nothing to record.
        std::fs::write(
            dir.join("70-lumen-nic1.link"),
            "[Match]\nPermanentMACAddress=52:54:00:aa:bb:01\n\n[Link]\nName=nic1\n",
        )
        .unwrap();
        // Somebody else's link file is none of our business.
        std::fs::write(
            dir.join("99-default.link"),
            "[Link]\nName=eth0\nAlternativeName=whatever\n",
        )
        .unwrap();

        let found = altnames(&dir);
        assert_eq!(found.get("nic0").map(String::as_str), Some("enp1s0"));
        assert_eq!(found.get("nic1"), None);
        assert_eq!(found.get("eth0"), None);

        // A box with no link files at all is empty, not an error.
        assert!(altnames(std::path::Path::new("/nonexistent-lumen-test")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
