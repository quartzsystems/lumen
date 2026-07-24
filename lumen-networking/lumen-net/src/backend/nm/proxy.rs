//! zbus proxies for the NetworkManager surface Lumen uses.
//!
//! Only the members listed in docs/networking.md are declared — this is a
//! deliberate narrow window onto a very wide API, not a binding.

use std::collections::HashMap;

use zbus::proxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

/// NetworkManager's connection settings: `a{sa{sv}}`.
pub type SettingsMap = HashMap<String, HashMap<String, OwnedValue>>;

#[proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
pub trait NetworkManager {
    fn get_all_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    fn activate_connection(
        &self,
        connection: &ObjectPath<'_>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<OwnedObjectPath>;

    fn deactivate_connection(&self, active_connection: &ObjectPath<'_>) -> zbus::Result<()>;

    /// An empty `devices` list means "every device", which is what a
    /// management-network change needs: the blast radius is the whole box.
    fn checkpoint_create(
        &self,
        devices: &[ObjectPath<'_>],
        rollback_timeout: u32,
        flags: u32,
    ) -> zbus::Result<OwnedObjectPath>;

    fn checkpoint_destroy(&self, checkpoint: &ObjectPath<'_>) -> zbus::Result<()>;

    /// Returns per-device rollback results; a non-zero value is a device that
    /// could not be restored.
    fn checkpoint_rollback(
        &self,
        checkpoint: &ObjectPath<'_>,
    ) -> zbus::Result<HashMap<String, u32>>;

    fn checkpoint_adjust_rollback_timeout(
        &self,
        checkpoint: &ObjectPath<'_>,
        add_timeout: u32,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn checkpoints(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    /// Daemon version, e.g. "1.54.0". Read once at startup to confirm the
    /// controller=/port-type= property spelling is supported.
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Settings",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Settings"
)]
pub trait Settings {
    fn list_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn add_connection(&self, connection: SettingsMap) -> zbus::Result<OwnedObjectPath>;
    fn get_connection_by_uuid(&self, uuid: &str) -> zbus::Result<OwnedObjectPath>;
    fn reload_connections(&self) -> zbus::Result<bool>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Settings.Connection",
    default_service = "org.freedesktop.NetworkManager"
)]
pub trait Connection {
    fn get_settings(&self) -> zbus::Result<SettingsMap>;
    fn update(&self, properties: SettingsMap) -> zbus::Result<()>;
    fn delete(&self) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
pub trait Device {
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn managed(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn mtu(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn active_connection(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property, name = "Ip4Config")]
    fn ip4_config(&self) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wired",
    default_service = "org.freedesktop.NetworkManager"
)]
pub trait DeviceWired {
    /// Burned-in address. This is the value a bridge MAC is pinned to.
    #[zbus(property)]
    fn perm_hw_address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn hw_address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn speed(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn carrier(&self) -> zbus::Result<bool>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.IP4Config",
    default_service = "org.freedesktop.NetworkManager"
)]
pub trait Ip4Config {
    /// `aa{sv}` with "address" (s) and "prefix" (u) per entry.
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
    #[zbus(property)]
    fn gateway(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
pub trait ActiveConnection {
    /// Path of the settings connection this activation came from.
    #[zbus(property)]
    fn connection(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn uuid(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
}
