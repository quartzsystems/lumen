//! The real node, over the system bus.
//!
//! Only the members Lumen uses are declared — a deliberately narrow window onto
//! logind's API, not a binding, exactly as `lumen_net`'s NetworkManager proxies
//! are.
//!
//! ## Why logind and not `systemctl reboot`
//!
//! Both end in the same place, but only one of them can *schedule*.
//! `ScheduleShutdown` is what `shutdown -r +30` calls, and it brings three
//! things a timer of Lumen's own could not:
//!
//! - it survives the control plane restarting, because logind is holding it;
//! - every signed-in session is warned on its terminal, on logind's schedule
//!   rather than ours;
//! - `shutdown -c` at the keyboard cancels it, because there is only one
//!   schedule and it is the node's.
//!
//! Reboot-now goes through the same interface for consistency, and because
//! `Reboot(false)` is a policy-checked call rather than a signal — a node that
//! refuses says so.

use async_trait::async_trait;
use zbus::zvariant::OwnedValue;
use zbus::{proxy, Connection};

use crate::backend::{PowerBackend, ScheduledPower};
use crate::error::{Result, SysError};
use crate::model::PowerAction;

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Login1Manager {
    /// `interactive` is false: this daemon is already root and there is no
    /// human at a polkit prompt to answer one.
    fn reboot(&self, interactive: bool) -> zbus::Result<()>;

    fn power_off(&self, interactive: bool) -> zbus::Result<()>;

    /// `kind` is "reboot", "poweroff", or "halt"; `usec` is CLOCK_REALTIME
    /// microseconds, not a delay.
    fn schedule_shutdown(&self, kind: &str, usec: u64) -> zbus::Result<()>;

    /// False when there was nothing scheduled.
    fn cancel_scheduled_shutdown(&self) -> zbus::Result<bool>;

    /// `(st)` — the kind and the moment. An empty kind means nothing is
    /// scheduled, which is how logind spells "none" rather than by unsetting
    /// the property.
    #[zbus(property)]
    fn scheduled_shutdown(&self) -> zbus::Result<OwnedValue>;
}

pub struct LogindBackend {
    connection: Connection,
}

impl LogindBackend {
    /// Connect to the system bus.
    ///
    /// Fails when there is no bus, which the caller turns into the
    /// [`super::unavailable`] backend rather than into a daemon that will not
    /// start — an operator whose node is misbehaving needs the console more
    /// than usual.
    pub async fn connect() -> anyhow::Result<Self> {
        let connection = Connection::system().await?;
        // Prove logind is actually there rather than discovering it at the
        // moment somebody presses Restart.
        let proxy = Login1ManagerProxy::new(&connection).await?;
        let _ = proxy.scheduled_shutdown().await?;
        Ok(Self { connection })
    }

    async fn proxy(&self) -> Result<Login1ManagerProxy<'_>> {
        Login1ManagerProxy::new(&self.connection)
            .await
            .map_err(|err| SysError::backend(anyhow::anyhow!("reaching logind: {err}")))
    }
}

/// Microseconds per second, which is the unit logind takes and nothing else in
/// this tree uses.
const USEC: u64 = 1_000_000;

#[async_trait]
impl PowerBackend for LogindBackend {
    async fn power(&self, action: PowerAction) -> Result<()> {
        let proxy = self.proxy().await?;
        let result = match action {
            PowerAction::Reboot => proxy.reboot(false).await,
            PowerAction::PowerOff => proxy.power_off(false).await,
        };
        result.map_err(|err| {
            SysError::backend(anyhow::anyhow!(
                "the node refused to {}: {err}",
                action.as_sentence()
            ))
        })
    }

    async fn schedule(&self, action: PowerAction, at: u64) -> Result<()> {
        self.proxy()
            .await?
            .schedule_shutdown(action.as_schedule_kind(), at.saturating_mul(USEC))
            .await
            .map_err(|err| {
                SysError::backend(anyhow::anyhow!(
                    "the node refused to schedule a {}: {err}",
                    action.as_sentence()
                ))
            })
    }

    async fn cancel(&self) -> Result<bool> {
        self.proxy()
            .await?
            .cancel_scheduled_shutdown()
            .await
            .map_err(|err| SysError::backend(anyhow::anyhow!("cancelling: {err}")))
    }

    async fn scheduled(&self) -> Result<Option<ScheduledPower>> {
        let raw = self
            .proxy()
            .await?
            .scheduled_shutdown()
            .await
            .map_err(|err| SysError::backend(anyhow::anyhow!("reading the schedule: {err}")))?;
        Ok(parse_scheduled(&raw))
    }
}

/// Read logind's `(st)` into something this crate has a name for.
///
/// Tolerant on purpose: the property is a struct of a string and a
/// microsecond timestamp, and an empty string is how logind spells "nothing is
/// scheduled". A shape this does not recognise reads as nothing scheduled
/// rather than as an error, because the alternative is a Maintenance page that
/// will not load on a systemd that changed a signature.
fn parse_scheduled(raw: &OwnedValue) -> Option<ScheduledPower> {
    let structure = raw.downcast_ref::<zbus::zvariant::Structure>().ok()?;
    let fields = structure.fields();
    let kind: String = fields.first()?.downcast_ref::<String>().ok()?;
    let usec: u64 = fields.get(1)?.downcast_ref::<u64>().ok()?;
    if kind.is_empty() || usec == 0 {
        return None;
    }
    Some(ScheduledPower {
        action: PowerAction::parse_schedule_kind(&kind)?,
        at: usec / USEC,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{StructureBuilder, Value};

    fn scheduled_value(kind: &str, usec: u64) -> OwnedValue {
        let structure = StructureBuilder::new()
            .add_field(kind.to_string())
            .add_field(usec)
            .build()
            .expect("a (st) structure");
        OwnedValue::try_from(Value::from(structure)).expect("owned")
    }

    #[test]
    fn a_schedule_comes_back_in_seconds_rather_than_loginds_microseconds() {
        let value = scheduled_value("reboot", 1_800_000_000 * 1_000_000);
        assert_eq!(
            parse_scheduled(&value),
            Some(ScheduledPower {
                action: PowerAction::Reboot,
                at: 1_800_000_000,
            })
        );
    }

    /// logind spells "nothing scheduled" with an empty kind rather than by
    /// unsetting the property, so this must not read as a restart at the
    /// epoch.
    #[test]
    fn an_empty_schedule_is_nothing_rather_than_a_restart_in_1970() {
        assert_eq!(parse_scheduled(&scheduled_value("", 0)), None);
        assert_eq!(parse_scheduled(&scheduled_value("", 12_345)), None);
        assert_eq!(parse_scheduled(&scheduled_value("reboot", 0)), None);
    }

    /// A dry run is systemd rehearsing, not a restart anybody should be shown
    /// a countdown for.
    #[test]
    fn a_kind_this_console_has_no_name_for_reads_as_nothing() {
        assert_eq!(
            parse_scheduled(&scheduled_value("dry-reboot", 1_800_000_000 * 1_000_000)),
            None
        );
    }
}
