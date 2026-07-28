//! The networking domain's one entry point.
//!
//! Everything the control plane's HTTP handlers do goes through here: staging
//! a change, validating it, applying it inside a checkpoint, confirming it,
//! and rolling it back. The handlers deserialize, call one method, serialize —
//! no validation, no planning, and no D-Bus above this line.
//!
//! ## Staged apply with auto-revert
//!
//! A networking mistake costs a truck roll, so nothing is applied the moment
//! it is requested:
//!
//! 1. Changes are *staged* into a pending target and validated as they go.
//! 2. `apply` takes a NetworkManager checkpoint, pushes the plan, and returns
//!    an absolute confirm deadline.
//! 3. `confirm` destroys the checkpoint; the change becomes permanent.
//! 4. Nobody confirming is the interesting case: NetworkManager restores the
//!    checkpoint by itself, in its own process, so this works even if the
//!    control plane died halfway through the apply.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::backend::NetworkBackend;
use crate::error::{NetError, Result};
use crate::model::{
    Bond, BondMode, Bridge, Duplex, IpConfig, LacpRate, LinkKind, ManagementRef,
    NetworkDesiredState, Vlan, XmitHashPolicy,
};
use crate::plan::plan;
use crate::state::{derive_desired, LinkState, ObservedLink, ObservedState};
use crate::store::{now_unix, CheckpointRecord, Store};
use crate::validate::{validate, Acknowledgements, ValidationCode, ValidationError};

/// Where a link stands relative to what has been committed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeState {
    #[default]
    Unchanged,
    Created,
    Modified,
    Deleted,
}

/// One row of the console's interface table. Everything a row needs is here,
/// so rendering never needs a second round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkView {
    pub name: String,
    /// The kernel's name for this NIC before it was pinned to `nicN`.
    pub altname: Option<String>,
    pub kind: LinkKind,
    /// Whether the appliance intends this link to be up.
    pub admin_up: bool,
    /// Where NetworkManager has actually got to.
    pub oper_state: LinkState,
    pub carrier: bool,
    pub perm_mac: Option<String>,
    pub mac: Option<String>,
    pub speed_mbps: Option<u32>,
    pub duplex: Option<Duplex>,
    pub mtu: Option<u32>,
    /// Addresses the box actually has on the link right now.
    pub addresses: Vec<String>,
    pub gateway: Option<String>,
    pub dns: Vec<String>,
    /// The addressing the configuration asks for — what the dialog edits, and
    /// what the table shows for a link that is staged but not yet applied.
    pub ip: IpConfig,
    pub controller: Option<String>,
    pub ports: Vec<String>,
    pub bond_mode: Option<BondMode>,
    pub vlan_id: Option<u16>,
    pub parent: Option<String>,
    /// "VLAN aware" — a bridge that passes tagged frames to its ports. Always
    /// false for anything that is not a bridge.
    pub vlan_aware: bool,
    /// Free text the operator wrote about this link.
    pub comment: Option<String>,
    /// This link carries the management address.
    pub management: bool,
    /// Physical NICs and the management link are never deletable.
    pub deletable: bool,
    /// Why it is not deletable, for a control that explains itself instead of
    /// being silently greyed out.
    pub delete_blocked_reason: Option<String>,
    pub change: ChangeState,
    /// Present on the box right now (a staged create is not, yet).
    pub present: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeInterfaces {
    pub node: String,
    pub interfaces: Vec<LinkView>,
}

/// GET /api/network/interfaces. Grouped by node even though there is exactly
/// one today: the shape must not change when clustering lands, and the console
/// renders its per-node layout from it now.
#[derive(Debug, Clone, Serialize)]
pub struct InterfacesResponse {
    pub nodes: Vec<NodeInterfaces>,
}

/// GET /api/network/pending — the staged target, what it changes, whether it
/// validates, and any checkpoint still waiting to be confirmed.
#[derive(Debug, Clone, Serialize)]
pub struct PendingResponse {
    pub node: String,
    /// Absent when nothing is staged.
    pub target: Option<NetworkDesiredState>,
    pub changes: Vec<PendingChange>,
    pub errors: Vec<ValidationError>,
    /// True when applying would move the management address off a reachable
    /// link — the console must collect the acknowledgement before it applies.
    pub requires_disconnect_ack: bool,
    pub checkpoint: Option<CheckpointView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingChange {
    pub link: String,
    pub kind: LinkKind,
    pub change: ChangeState,
}

/// An outstanding checkpoint as the console's countdown sees it.
#[derive(Debug, Clone, Serialize)]
pub struct CheckpointView {
    pub id: String,
    /// Absolute deadline, seconds since the epoch — the console counts down to
    /// this rather than to a duration, so a reload cannot drift it.
    pub confirm_deadline: u64,
    /// Seconds left as of this response.
    pub seconds_remaining: u64,
    /// The window in force, after any extensions.
    pub rollback_secs: u32,
}

/// POST /api/network/apply.
#[derive(Debug, Clone, Serialize)]
pub struct ApplyResponse {
    pub checkpoint: CheckpointView,
    /// What was pushed, in order, for the log and for review.
    pub operations: Vec<String>,
}

/// POST /api/network/management-bridge.
#[derive(Debug, Clone, Serialize)]
pub struct ManagementBridgeResponse {
    /// The bridge that now carries (or already carried) the address.
    pub bridge: String,
    /// False when a bridge was already carrying it — idempotent success.
    pub converted: bool,
    /// Absent when nothing had to change.
    pub checkpoint: Option<CheckpointView>,
    pub operations: Vec<String>,
}

/// PATCH bodies. Absent fields are left alone; there is no way to clear a
/// value back to "unset" through a patch — create the link again for that.
///
/// `ip` and `comment` are common to all four: the console's dialogs are the
/// same form with different middles, and an address is now a property of the
/// link rather than of the appliance.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NicPatch {
    pub ip: Option<IpConfig>,
    pub comment: Option<String>,
    pub mtu: Option<u32>,
    pub autoneg: Option<bool>,
    pub speed: Option<u32>,
    pub duplex: Option<Duplex>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgePatch {
    pub ip: Option<IpConfig>,
    pub comment: Option<String>,
    pub ports: Option<Vec<String>>,
    pub stp: Option<bool>,
    pub forward_delay: Option<u32>,
    pub vlan_filtering: Option<bool>,
    pub mac_address: Option<String>,
    pub mtu: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BondPatch {
    pub ip: Option<IpConfig>,
    pub comment: Option<String>,
    pub mode: Option<BondMode>,
    pub ports: Option<Vec<String>>,
    pub miimon: Option<u32>,
    pub lacp_rate: Option<LacpRate>,
    pub xmit_hash_policy: Option<XmitHashPolicy>,
    pub primary: Option<String>,
    pub mtu: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VlanPatch {
    pub ip: Option<IpConfig>,
    pub comment: Option<String>,
    pub parent: Option<String>,
    pub vlan_id: Option<u16>,
    pub mtu: Option<u32>,
}

/// An empty comment means "no comment", not a comment that is the empty
/// string — the console clears the field by sending `""`.
fn comment_of(patch: Option<String>) -> Option<Option<String>> {
    patch.map(|text| tidy_comment(Some(text)))
}

fn tidy_comment(comment: Option<String>) -> Option<String> {
    let trimmed = comment?.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

pub struct NetworkService {
    backend: Arc<dyn NetworkBackend>,
    store: Store,
    confirm_secs: u32,
    /// Serializes every mutation. Two applies racing for one checkpoint is
    /// exactly the failure this domain exists to prevent.
    gate: Mutex<()>,
}

impl NetworkService {
    pub fn new(
        backend: Arc<dyn NetworkBackend>,
        state_dir: &std::path::Path,
        confirm_secs: u32,
    ) -> Self {
        Self {
            backend,
            store: Store::new(state_dir),
            confirm_secs,
            gate: Mutex::new(()),
        }
    }

    pub fn confirm_secs(&self) -> u32 {
        self.confirm_secs
    }

    // --- reads -----------------------------------------------------------

    pub async fn observe(&self) -> Result<ObservedState> {
        self.backend.observe().await
    }

    /// The committed configuration, seeded from the box the first time.
    pub async fn config(&self) -> Result<NetworkDesiredState> {
        if let Some(desired) = self.store.desired()? {
            return Ok(desired);
        }
        Ok(derive_desired(&self.backend.observe().await?))
    }

    /// The staged target if there is one, otherwise the committed state — the
    /// document every mutation edits.
    async fn working(&self) -> Result<NetworkDesiredState> {
        match self.store.pending()? {
            Some(pending) => Ok(pending),
            None => self.config().await,
        }
    }

    pub async fn interfaces(&self) -> Result<InterfacesResponse> {
        let observed = self.backend.observe().await?;
        let desired = self.config().await?;
        let pending = self.store.pending()?;
        let interfaces = views(&observed, &desired, pending.as_ref());
        Ok(InterfacesResponse {
            nodes: vec![NodeInterfaces {
                node: observed.node.clone(),
                interfaces,
            }],
        })
    }

    pub async fn interface(&self, name: &str) -> Result<LinkView> {
        self.interfaces()
            .await?
            .nodes
            .into_iter()
            .flat_map(|node| node.interfaces)
            .find(|view| view.name == name)
            .ok_or_else(|| NetError::NotFound(format!("No interface named \"{name}\".")))
    }

    pub async fn pending(&self) -> Result<PendingResponse> {
        let observed = self.backend.observe().await?;
        let desired = self.config().await?;
        let pending = self.store.pending()?;

        let (errors, requires_ack) = match &pending {
            Some(target) => {
                let all = validate(target, &observed, Acknowledgements::default());
                let requires_ack = all
                    .iter()
                    .any(|e| e.code == ValidationCode::ManagementDisconnect);
                let hard: Vec<ValidationError> = all
                    .into_iter()
                    .filter(|e| e.code != ValidationCode::ManagementDisconnect)
                    .collect();
                (hard, requires_ack)
            }
            None => (Vec::new(), false),
        };

        Ok(PendingResponse {
            node: observed.node.clone(),
            changes: pending
                .as_ref()
                .map(|target| changes(&desired, target))
                .unwrap_or_default(),
            target: pending,
            errors,
            requires_disconnect_ack: requires_ack,
            checkpoint: self.checkpoint_view().await?,
        })
    }

    /// The outstanding checkpoint, if one is still outstanding.
    ///
    /// The backend is the authority: when NetworkManager has already rolled
    /// back on its own, the record here is stale and is cleared. That is what
    /// makes the expire-without-confirm path resolve without anyone driving it.
    pub async fn checkpoint_view(&self) -> Result<Option<CheckpointView>> {
        let Some(record) = self.store.checkpoint()? else {
            return Ok(None);
        };
        let live = self.backend.checkpoints().await.unwrap_or_default();
        if !live.contains(&record.id) {
            tracing::info!(
                checkpoint = %record.id,
                "checkpoint is gone — the change was reverted automatically"
            );
            self.store.clear_checkpoint()?;
            return Ok(None);
        }
        let now = now_unix();
        Ok(Some(CheckpointView {
            id: record.id.0.clone(),
            confirm_deadline: record.confirm_deadline,
            seconds_remaining: record.confirm_deadline.saturating_sub(now),
            rollback_secs: record.rollback_secs,
        }))
    }

    // --- staging ---------------------------------------------------------

    async fn stage(&self, target: NetworkDesiredState) -> Result<PendingResponse> {
        let observed = self.backend.observe().await?;
        // A staged change must be applicable on its own. The one error a stage
        // tolerates is the management-disconnect acknowledgement, which is
        // collected at apply time, not while editing.
        let errors: Vec<ValidationError> =
            validate(&target, &observed, Acknowledgements::default())
                .into_iter()
                .filter(|e| e.code != ValidationCode::ManagementDisconnect)
                .collect();
        if !errors.is_empty() {
            return Err(NetError::Invalid(errors));
        }
        self.store.set_pending(&target)?;
        self.pending().await
    }

    pub async fn discard(&self) -> Result<PendingResponse> {
        let _guard = self.gate.lock().await;
        self.store.clear_pending()?;
        self.pending().await
    }

    pub async fn create_bridge(&self, mut bridge: Bridge) -> Result<PendingResponse> {
        let _guard = self.gate.lock().await;
        let mut target = self.working().await?;
        reject_existing(&target, &bridge.name)?;
        bridge.comment = tidy_comment(bridge.comment);
        target.bridges.push(bridge);
        self.stage(target).await
    }

    pub async fn create_bond(&self, mut bond: Bond) -> Result<PendingResponse> {
        let _guard = self.gate.lock().await;
        let mut target = self.working().await?;
        reject_existing(&target, &bond.name)?;
        bond.comment = tidy_comment(bond.comment);
        target.bonds.push(bond);
        self.stage(target).await
    }

    pub async fn create_vlan(&self, mut vlan: Vlan) -> Result<PendingResponse> {
        let _guard = self.gate.lock().await;
        let mut target = self.working().await?;
        reject_existing(&target, &vlan.name)?;
        vlan.comment = tidy_comment(vlan.comment);
        target.vlans.push(vlan);
        self.stage(target).await
    }

    pub async fn update_bridge(&self, name: &str, patch: BridgePatch) -> Result<PendingResponse> {
        let _guard = self.gate.lock().await;
        let mut target = self.working().await?;
        let bridge = target
            .bridges
            .iter_mut()
            .find(|b| b.name == name)
            .ok_or_else(|| NetError::NotFound(format!("No bridge named \"{name}\".")))?;
        if let Some(ip) = patch.ip {
            bridge.ip = ip;
        }
        if let Some(comment) = comment_of(patch.comment) {
            bridge.comment = comment;
        }
        if let Some(ports) = patch.ports {
            bridge.ports = ports;
        }
        if let Some(stp) = patch.stp {
            bridge.stp = stp;
        }
        if let Some(delay) = patch.forward_delay {
            bridge.forward_delay = Some(delay);
        }
        if let Some(filtering) = patch.vlan_filtering {
            bridge.vlan_filtering = filtering;
        }
        if let Some(mac) = patch.mac_address {
            bridge.mac_address = Some(mac);
        }
        if let Some(mtu) = patch.mtu {
            bridge.mtu = Some(mtu);
        }
        self.stage(target).await
    }

    pub async fn update_bond(&self, name: &str, patch: BondPatch) -> Result<PendingResponse> {
        let _guard = self.gate.lock().await;
        let mut target = self.working().await?;
        let bond = target
            .bonds
            .iter_mut()
            .find(|b| b.name == name)
            .ok_or_else(|| NetError::NotFound(format!("No bond named \"{name}\".")))?;
        if let Some(ip) = patch.ip {
            bond.ip = ip;
        }
        if let Some(comment) = comment_of(patch.comment) {
            bond.comment = comment;
        }
        if let Some(mode) = patch.mode {
            bond.mode = mode;
        }
        if let Some(ports) = patch.ports {
            bond.ports = ports;
        }
        if let Some(miimon) = patch.miimon {
            bond.miimon = Some(miimon);
        }
        if let Some(rate) = patch.lacp_rate {
            bond.lacp_rate = Some(rate);
        }
        if let Some(policy) = patch.xmit_hash_policy {
            bond.xmit_hash_policy = Some(policy);
        }
        if let Some(primary) = patch.primary {
            bond.primary = Some(primary);
        }
        if let Some(mtu) = patch.mtu {
            bond.mtu = Some(mtu);
        }
        self.stage(target).await
    }

    pub async fn update_vlan(&self, name: &str, patch: VlanPatch) -> Result<PendingResponse> {
        let _guard = self.gate.lock().await;
        let mut target = self.working().await?;
        let vlan = target
            .vlans
            .iter_mut()
            .find(|v| v.name == name)
            .ok_or_else(|| NetError::NotFound(format!("No VLAN interface named \"{name}\".")))?;
        if let Some(ip) = patch.ip {
            vlan.ip = ip;
        }
        if let Some(comment) = comment_of(patch.comment) {
            vlan.comment = comment;
        }
        if let Some(parent) = patch.parent {
            vlan.parent = parent;
        }
        if let Some(id) = patch.vlan_id {
            vlan.vlan_id = id;
        }
        if let Some(mtu) = patch.mtu {
            vlan.mtu = Some(mtu);
        }
        self.stage(target).await
    }

    pub async fn update_nic(&self, name: &str, patch: NicPatch) -> Result<PendingResponse> {
        let _guard = self.gate.lock().await;
        let mut target = self.working().await?;
        let nic = target
            .nics
            .iter_mut()
            .find(|n| n.name == name)
            .ok_or_else(|| NetError::NotFound(format!("No interface named \"{name}\".")))?;
        if let Some(ip) = patch.ip {
            nic.ip = ip;
        }
        if let Some(comment) = comment_of(patch.comment) {
            nic.comment = comment;
        }
        if let Some(mtu) = patch.mtu {
            nic.mtu = Some(mtu);
        }
        if let Some(autoneg) = patch.autoneg {
            nic.autoneg = Some(autoneg);
        }
        if let Some(speed) = patch.speed {
            nic.speed = Some(speed);
        }
        if let Some(duplex) = patch.duplex {
            nic.duplex = Some(duplex);
        }
        self.stage(target).await
    }

    pub async fn delete_link(&self, name: &str, kind: LinkKind) -> Result<PendingResponse> {
        let _guard = self.gate.lock().await;
        let mut target = self.working().await?;
        if target.management.link == name {
            return Err(NetError::Conflict(format!(
                "\"{name}\" carries the management address and cannot be removed. Move the \
                 address to another link first."
            )));
        }
        let found = match kind {
            LinkKind::Bridge => remove_named(&mut target.bridges, name, |b| &b.name),
            LinkKind::Bond => remove_named(&mut target.bonds, name, |b| &b.name),
            LinkKind::Vlan => remove_named(&mut target.vlans, name, |v| &v.name),
            _ => {
                return Err(NetError::Conflict(format!(
                    "\"{name}\" is a physical interface and cannot be removed."
                )))
            }
        };
        if !found {
            return Err(NetError::NotFound(format!("No link named \"{name}\".")));
        }
        // A link that is going away cannot stay a port of anything.
        for bridge in &mut target.bridges {
            bridge.ports.retain(|p| p != name);
        }
        for bond in &mut target.bonds {
            bond.ports.retain(|p| p != name);
        }
        self.stage(target).await
    }

    // --- apply / confirm / rollback --------------------------------------

    pub async fn apply(&self, ack: Acknowledgements) -> Result<ApplyResponse> {
        let _guard = self.gate.lock().await;
        let Some(target) = self.store.pending()? else {
            return Err(NetError::Conflict(
                "There are no staged changes to apply.".into(),
            ));
        };
        self.apply_target(target, ack).await
    }

    /// Validate, checkpoint, push. Shared by `apply` and the management-bridge
    /// conversion so both get the same auto-revert protection.
    async fn apply_target(
        &self,
        target: NetworkDesiredState,
        ack: Acknowledgements,
    ) -> Result<ApplyResponse> {
        if self.checkpoint_view().await?.is_some() {
            return Err(NetError::Conflict(
                "A change is already waiting to be confirmed. Confirm or roll it back first."
                    .into(),
            ));
        }

        let observed = self.backend.observe().await?;
        let errors = validate(&target, &observed, ack);
        if !errors.is_empty() {
            return Err(NetError::Invalid(errors));
        }

        let ops = plan(&target, &observed);
        let operations: Vec<String> = ops.iter().map(|op| op.to_string()).collect();

        // The checkpoint comes first, always: from here on NetworkManager
        // restores the box by itself if we never come back.
        let id = self.backend.checkpoint_create(self.confirm_secs).await?;
        let record = CheckpointRecord {
            id: id.clone(),
            confirm_deadline: now_unix() + u64::from(self.confirm_secs),
            rollback_secs: self.confirm_secs,
            target: target.clone(),
        };
        self.store.set_checkpoint(&record)?;

        if let Err(err) = self.backend.apply(&ops).await {
            // Do not make the operator wait out the window for a failure we
            // already know about.
            tracing::error!("apply failed, rolling back immediately: {err}");
            let _ = self.backend.checkpoint_rollback(&id).await;
            self.store.clear_checkpoint()?;
            return Err(err);
        }

        Ok(ApplyResponse {
            checkpoint: CheckpointView {
                id: id.0,
                confirm_deadline: record.confirm_deadline,
                seconds_remaining: u64::from(self.confirm_secs),
                rollback_secs: self.confirm_secs,
            },
            operations,
        })
    }

    /// Destroy the checkpoint: the change is now permanent.
    pub async fn confirm(&self) -> Result<PendingResponse> {
        let _guard = self.gate.lock().await;
        let record = self.outstanding().await?;
        self.backend.checkpoint_destroy(&record.id).await?;
        self.store.set_desired(&record.target)?;
        self.store.clear_pending()?;
        self.store.clear_checkpoint()?;
        tracing::info!(checkpoint = %record.id, "network change confirmed");
        self.pending().await
    }

    /// Roll back now instead of waiting out the window. The staged target is
    /// deliberately left staged: the operator will usually want to fix it and
    /// try again, not retype it.
    pub async fn rollback(&self) -> Result<PendingResponse> {
        let _guard = self.gate.lock().await;
        let record = self.outstanding().await?;
        self.backend.checkpoint_rollback(&record.id).await?;
        self.store.clear_checkpoint()?;
        tracing::warn!(checkpoint = %record.id, "network change rolled back");
        self.pending().await
    }

    /// Give a slow operator more time.
    pub async fn extend(&self, add_secs: u32) -> Result<CheckpointView> {
        let _guard = self.gate.lock().await;
        let mut record = self.outstanding().await?;
        self.backend.checkpoint_extend(&record.id, add_secs).await?;
        record.confirm_deadline += u64::from(add_secs);
        record.rollback_secs += add_secs;
        self.store.set_checkpoint(&record)?;
        Ok(CheckpointView {
            id: record.id.0,
            confirm_deadline: record.confirm_deadline,
            seconds_remaining: record.confirm_deadline.saturating_sub(now_unix()),
            rollback_secs: record.rollback_secs,
        })
    }

    async fn outstanding(&self) -> Result<CheckpointRecord> {
        if self.checkpoint_view().await?.is_none() {
            return Err(NetError::Conflict(
                "There is no change waiting to be confirmed.".into(),
            ));
        }
        self.store
            .checkpoint()?
            .ok_or_else(|| NetError::Conflict("There is no change waiting to be confirmed.".into()))
    }

    // --- management bridge ------------------------------------------------

    /// Convert a flat `nicN` management connection into `brN` + port.
    ///
    /// Idempotent: a management address already on a bridge is success, not an
    /// error. Runs through the same checkpointed apply as everything else, so
    /// a conversion that goes wrong self-heals.
    pub async fn management_bridge(&self) -> Result<ManagementBridgeResponse> {
        let _guard = self.gate.lock().await;
        let observed = self.backend.observe().await?;
        let desired = self.config().await?;

        let link = desired.management.link.clone();
        if link.is_empty() {
            return Err(NetError::Conflict(
                "No link carries the management address, so there is nothing to convert.".into(),
            ));
        }
        if desired.bridge(&link).is_some() {
            return Ok(ManagementBridgeResponse {
                bridge: link,
                converted: false,
                checkpoint: None,
                operations: Vec::new(),
            });
        }
        if desired.nic(&link).is_none() {
            return Err(NetError::Conflict(format!(
                "\"{link}\" carries the management address but is not a physical interface — \
                 convert it by editing the configuration instead."
            )));
        }
        if self.store.pending()?.is_some() {
            return Err(NetError::Conflict(
                "There are staged changes. Apply or discard them before converting the \
                 management interface."
                    .into(),
            ));
        }

        let bridge_name = next_bridge_name(&desired);
        // Pin the bridge to the NIC's burned-in address. Without this the
        // bridge takes the lowest MAC among its ports, so the day a second NIC
        // joins, the management MAC changes underneath the DHCP reservation
        // and any switch-side port security.
        let mac = observed
            .link(&link)
            .and_then(|l| l.perm_mac.clone().or_else(|| l.mac.clone()));
        if mac.is_none() {
            tracing::warn!(nic = %link, "no permanent MAC to pin the management bridge to");
        }

        let mut target = desired.clone();
        target.bridges.push(Bridge {
            name: bridge_name.clone(),
            // Exactly the addressing that was there before, moved rather than
            // copied: a port may not keep an address of its own.
            ip: desired.ip_of(&link),
            comment: None,
            ports: vec![link.clone()],
            stp: false,
            forward_delay: Some(0),
            vlan_filtering: false,
            mac_address: mac,
            mtu: observed.link(&link).and_then(|l| l.mtu),
        });
        target.set_ip(&link, crate::model::IpConfig::Disabled);
        target.management = ManagementRef {
            link: bridge_name.clone(),
            legacy_ip: None,
        };

        // Stage and apply as one operation: the box must never be left with
        // the address on neither link.
        self.store.set_pending(&target)?;
        // Moving the address IS the point of this call, so the acknowledgement
        // is implicit — the checkpoint is what makes that safe.
        let applied = self
            .apply_target(
                target,
                Acknowledgements {
                    may_disconnect: true,
                },
            )
            .await;
        match applied {
            Ok(response) => Ok(ManagementBridgeResponse {
                bridge: bridge_name,
                converted: true,
                checkpoint: Some(response.checkpoint),
                operations: response.operations,
            }),
            Err(err) => {
                // Nothing was applied, so nothing should look staged either.
                self.store.clear_pending()?;
                Err(err)
            }
        }
    }

    // --- startup ----------------------------------------------------------

    /// Adopt or clear whatever a previous run left behind.
    ///
    /// A checkpoint we have a record for is adopted: NetworkManager has been
    /// watching it the whole time we were down, and the operator can still
    /// confirm it. A checkpoint we have no record for is an orphan from a run
    /// that died mid-apply — it is rolled back, because "undo the change
    /// nobody confirmed" is the safe direction, and because leaving it would
    /// block every future apply.
    pub async fn reconcile(&self) -> Result<()> {
        let record = self.store.checkpoint()?;
        let live = match self.backend.checkpoints().await {
            Ok(live) => live,
            Err(err) => {
                tracing::warn!("could not list checkpoints at startup: {err}");
                return Ok(());
            }
        };

        if let Some(record) = &record {
            if live.contains(&record.id) {
                tracing::warn!(
                    checkpoint = %record.id,
                    deadline = record.confirm_deadline,
                    "adopting a network change that is still waiting to be confirmed"
                );
            } else {
                tracing::info!(
                    checkpoint = %record.id,
                    "a network change was reverted while the control plane was down"
                );
                self.store.clear_checkpoint()?;
            }
        }

        for id in live {
            if record.as_ref().is_some_and(|r| r.id == id) {
                continue;
            }
            tracing::warn!(checkpoint = %id, "rolling back an orphaned checkpoint");
            if let Err(err) = self.backend.checkpoint_rollback(&id).await {
                tracing::error!(checkpoint = %id, "could not roll it back: {err}");
            }
        }
        Ok(())
    }
}

fn remove_named<T>(items: &mut Vec<T>, name: &str, key: impl Fn(&T) -> &String) -> bool {
    let before = items.len();
    items.retain(|item| key(item) != name);
    items.len() != before
}

fn reject_existing(target: &NetworkDesiredState, name: &str) -> Result<()> {
    if target.kind_of(name).is_some() {
        return Err(NetError::Invalid(vec![ValidationError {
            code: ValidationCode::DuplicateName,
            link: Some(name.to_string()),
            field: Some("name".into()),
            message: format!("\"{name}\" already exists."),
        }]));
    }
    Ok(())
}

/// br0, or the first free brN when it is taken.
fn next_bridge_name(desired: &NetworkDesiredState) -> String {
    (0..)
        .map(|n| format!("br{n}"))
        .find(|name| desired.kind_of(name).is_none())
        .expect("an unbounded search always finds a free name")
}

/// What the staged target changes relative to what is committed.
fn changes(desired: &NetworkDesiredState, target: &NetworkDesiredState) -> Vec<PendingChange> {
    let mut out: Vec<PendingChange> = Vec::new();
    let entry = |link: &str, kind: LinkKind, change: ChangeState| PendingChange {
        link: link.to_string(),
        kind,
        change,
    };

    for (name, kind) in target.link_kinds() {
        match change_of(desired, target, name, kind) {
            ChangeState::Unchanged => {}
            change => out.push(entry(name, kind, change)),
        }
    }
    for (name, kind) in desired.link_kinds() {
        if target.kind_of(name).is_none() {
            out.push(entry(name, kind, ChangeState::Deleted));
        }
    }
    if desired.management != target.management && target.kind_of(&target.management.link).is_some()
    {
        // A management move shows up as a change on the link that gains it,
        // even when the link itself is otherwise untouched.
        let already = out.iter().any(|c| c.link == target.management.link);
        if !already {
            let kind = target.kind_of(&target.management.link).unwrap_or_default();
            out.push(entry(&target.management.link, kind, ChangeState::Modified));
        }
    }
    out.sort_by(|a, b| a.link.cmp(&b.link));
    out
}

fn change_of(
    desired: &NetworkDesiredState,
    target: &NetworkDesiredState,
    name: &str,
    kind: LinkKind,
) -> ChangeState {
    if desired.kind_of(name).is_none() {
        return ChangeState::Created;
    }
    // Enslavement lives on the controller, but it is a change to the port as
    // far as the operator reading the table is concerned.
    if desired.controller_of(name) != target.controller_of(name) {
        return ChangeState::Modified;
    }
    let same = match kind {
        LinkKind::Bridge => desired.bridge(name) == target.bridge(name),
        LinkKind::Bond => desired.bond(name) == target.bond(name),
        LinkKind::Vlan => desired.vlan(name) == target.vlan(name),
        LinkKind::Ethernet => desired.nic(name) == target.nic(name),
        LinkKind::Other => true,
    };
    if same {
        ChangeState::Unchanged
    } else {
        ChangeState::Modified
    }
}

/// Loopback is present on every Linux box, is not configurable, and is never
/// what an operator opened this page to look at.
fn is_loopback(link: &ObservedLink) -> bool {
    link.name == "lo"
}

/// Build the console's rows: every link the box has, plus every link only the
/// staged target has, ordered so a controller is immediately followed by its
/// ports.
fn views(
    observed: &ObservedState,
    desired: &NetworkDesiredState,
    pending: Option<&NetworkDesiredState>,
) -> Vec<LinkView> {
    let effective = pending.unwrap_or(desired);
    let management = effective.management.link.as_str();

    let mut rows: Vec<LinkView> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for link in observed.links.iter().filter(|l| !is_loopback(l)) {
        rows.push(view_of(link, desired, pending, management, true));
        seen.push(link.name.clone());
    }
    // Links that only exist in the staged target: shown so the operator sees
    // the whole picture before applying, not just what already happened.
    for (name, kind) in effective.link_kinds() {
        if seen.iter().any(|s| s == name) {
            continue;
        }
        let placeholder = ObservedLink {
            name: name.to_string(),
            kind,
            state: LinkState::Unknown,
            ..ObservedLink::default()
        };
        rows.push(view_of(&placeholder, desired, pending, management, false));
    }

    order_by_controller(rows)
}

fn view_of(
    link: &ObservedLink,
    desired: &NetworkDesiredState,
    pending: Option<&NetworkDesiredState>,
    management: &str,
    present: bool,
) -> LinkView {
    let effective = pending.unwrap_or(desired);
    let name = link.name.as_str();
    let change = match pending {
        None => ChangeState::Unchanged,
        Some(target) => match target.kind_of(name) {
            None if desired.kind_of(name).is_some() => ChangeState::Deleted,
            None => ChangeState::Unchanged,
            Some(kind) => change_of(desired, target, name, kind),
        },
    };

    // The staged target is what the row should show for structural fields —
    // ports move the moment they are staged, not when they are applied.
    let staged_ports = effective.ports_of(name);
    let ports = if staged_ports.is_empty() {
        link.ports.clone()
    } else {
        staged_ports.to_vec()
    };
    let controller = effective
        .controller_of(name)
        .map(String::from)
        .or_else(|| link.controller.clone());

    let is_management = name == management;
    let kind = effective.kind_of(name).unwrap_or(link.kind);
    let physical = kind == LinkKind::Ethernet || link.kind == LinkKind::Ethernet;
    let other = kind == LinkKind::Other;
    let delete_blocked_reason = if is_management {
        Some(format!(
            "\"{name}\" carries the management address. Move the address to another link \
             before removing it."
        ))
    } else if physical {
        Some(format!(
            "\"{name}\" is a physical interface — it can be configured, but not removed."
        ))
    } else if other {
        Some(format!("\"{name}\" is not managed by Lumen."))
    } else {
        None
    };

    LinkView {
        name: link.name.clone(),
        altname: link.altname.clone(),
        kind,
        admin_up: effective.kind_of(name).is_some() && change != ChangeState::Deleted,
        oper_state: link.state,
        carrier: link.carrier,
        perm_mac: link.perm_mac.clone(),
        mac: link.mac.clone(),
        speed_mbps: link.speed_mbps,
        duplex: link.duplex,
        mtu: effective
            .nic(name)
            .and_then(|n| n.mtu)
            .or_else(|| effective.bridge(name).and_then(|b| b.mtu))
            .or_else(|| effective.bond(name).and_then(|b| b.mtu))
            .or_else(|| effective.vlan(name).and_then(|v| v.mtu))
            .or(link.mtu),
        addresses: link.addresses.clone(),
        gateway: link.gateway.clone(),
        dns: link.dns.clone(),
        // The configuration, not the box: a staged address must show in the
        // table before anyone applies it.
        ip: effective.ip_of(name),
        controller,
        ports,
        bond_mode: effective.bond(name).map(|b| b.mode).or(link.bond_mode),
        vlan_id: effective.vlan(name).map(|v| v.vlan_id).or(link.vlan_id),
        parent: effective
            .vlan(name)
            .map(|v| v.parent.clone())
            .or_else(|| link.parent.clone()),
        vlan_aware: effective.bridge(name).is_some_and(|b| b.vlan_filtering),
        comment: effective.comment_of(name).map(String::from),
        management: is_management,
        deletable: delete_blocked_reason.is_none(),
        delete_blocked_reason,
        change,
        present,
    }
}

/// Controllers first, each immediately followed by its ports (recursively),
/// then everything else. Raw alphabetical order scatters a bond's members
/// across the table, which is exactly when an operator is trying to read them
/// together.
fn order_by_controller(rows: Vec<LinkView>) -> Vec<LinkView> {
    fn rank(kind: LinkKind) -> u8 {
        match kind {
            LinkKind::Bridge => 0,
            LinkKind::Bond => 1,
            LinkKind::Vlan => 2,
            LinkKind::Ethernet => 3,
            LinkKind::Other => 4,
        }
    }

    let mut remaining = rows;
    remaining.sort_by(|a, b| rank(a.kind).cmp(&rank(b.kind)).then(a.name.cmp(&b.name)));

    let mut out: Vec<LinkView> = Vec::new();
    while !remaining.is_empty() {
        // The next root: a link whose controller is not itself still waiting.
        let index = remaining
            .iter()
            .position(|row| match &row.controller {
                None => true,
                Some(controller) => !remaining.iter().any(|other| &other.name == controller),
            })
            .unwrap_or(0);
        let row = remaining.remove(index);
        let name = row.name.clone();
        out.push(row);
        // …then everything enslaved to it, in the same order.
        let mut children: Vec<LinkView> = Vec::new();
        remaining.retain(|candidate| {
            if candidate.controller.as_deref() == Some(name.as_str()) {
                children.push(candidate.clone());
                false
            } else {
                true
            }
        });
        for child in children.into_iter().rev() {
            remaining.insert(0, child);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;
    use crate::plan::Op;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lumen-net-service-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn service(tag: &str) -> (NetworkService, Arc<MockBackend>, PathBuf) {
        let dir = temp_dir(tag);
        let backend = Arc::new(MockBackend::appliance());
        let service = NetworkService::new(backend.clone(), &dir, 60);
        (service, backend, dir)
    }

    fn bridge(name: &str, ports: &[&str]) -> Bridge {
        Bridge {
            name: name.into(),
            ports: ports.iter().map(|p| p.to_string()).collect(),
            ..Bridge::default()
        }
    }

    #[tokio::test]
    async fn config_is_seeded_from_the_box_when_nothing_is_committed() {
        let (service, _backend, dir) = service("seed");
        let config = service.config().await.unwrap();
        assert_eq!(config.management.link, "nic0");
        assert_eq!(config.nics.len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stage_apply_confirm() {
        let (service, backend, dir) = service("confirm");
        let pending = service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        assert_eq!(pending.changes.len(), 2); // br1 created, nic1 modified
        assert!(pending.errors.is_empty());

        let applied = service.apply(Acknowledgements::default()).await.unwrap();
        assert!(applied.checkpoint.seconds_remaining > 0);
        assert!(backend.state().link("br1").is_some());

        let after = service.confirm().await.unwrap();
        assert!(after.target.is_none(), "confirm clears the staged set");
        assert!(after.checkpoint.is_none());
        // Committed: the bridge is now part of the configuration.
        assert!(service.config().await.unwrap().bridge("br1").is_some());
        assert!(backend.checkpoints().await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stage_apply_then_expire_without_confirming() {
        let (service, backend, dir) = service("expire");
        let before = backend.state();
        service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        service.apply(Acknowledgements::default()).await.unwrap();
        assert!(backend.state().link("br1").is_some());

        // NetworkManager's own timer fires; nothing here drives it.
        backend.expire_checkpoints();

        let pending = service.pending().await.unwrap();
        assert!(pending.checkpoint.is_none(), "the checkpoint is gone");
        assert!(
            pending.target.is_some(),
            "the staged change stays staged so it can be fixed and retried"
        );
        assert_eq!(backend.state(), before, "the box is back where it started");
        // The configuration was never committed.
        assert!(service.config().await.unwrap().bridge("br1").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn rollback_reverts_now_and_keeps_the_staged_set() {
        let (service, backend, dir) = service("rollback");
        let before = backend.state();
        service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        service.apply(Acknowledgements::default()).await.unwrap();
        let after = service.rollback().await.unwrap();
        assert!(after.checkpoint.is_none());
        assert!(after.target.is_some());
        assert_eq!(backend.state(), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_second_apply_is_refused_while_one_is_outstanding() {
        let (service, _backend, dir) = service("second");
        service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        service.apply(Acknowledgements::default()).await.unwrap();
        let err = service
            .apply(Acknowledgements::default())
            .await
            .unwrap_err();
        assert!(matches!(err, NetError::Conflict(_)), "{err:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn extending_moves_the_deadline_out() {
        let (service, _backend, dir) = service("extend");
        service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        let applied = service.apply(Acknowledgements::default()).await.unwrap();
        let extended = service.extend(120).await.unwrap();
        assert_eq!(
            extended.confirm_deadline,
            applied.checkpoint.confirm_deadline + 120
        );
        assert_eq!(extended.rollback_secs, 180);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn staging_an_invalid_change_is_rejected_with_its_code() {
        let (service, _backend, dir) = service("invalid");
        let err = service
            .create_vlan(Vlan {
                name: "vlan9999".into(),
                parent: "nic1".into(),
                vlan_id: 9999,
                ..Vlan::default()
            })
            .await
            .unwrap_err();
        match err {
            NetError::Invalid(errors) => {
                assert_eq!(errors[0].code, ValidationCode::VlanIdOutOfRange)
            }
            other => panic!("expected a validation failure, got {other:?}"),
        }
        // Nothing was staged.
        assert!(service.pending().await.unwrap().target.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn deleting_the_management_link_is_refused() {
        let (service, _backend, dir) = service("delete-mgmt");
        let err = service
            .delete_link("nic0", LinkKind::Ethernet)
            .await
            .unwrap_err();
        assert!(matches!(err, NetError::Conflict(_)), "{err:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn deleting_a_bridge_releases_its_ports() {
        let (service, _backend, dir) = service("delete-bridge");
        service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        let pending = service.delete_link("br1", LinkKind::Bridge).await.unwrap();
        let target = pending.target.unwrap();
        assert!(target.bridge("br1").is_none());
        assert!(target.controller_of("nic1").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn discarding_clears_everything_staged() {
        let (service, _backend, dir) = service("discard");
        service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        let after = service.discard().await.unwrap();
        assert!(after.target.is_none());
        assert!(after.changes.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_management_bridge_conversion_preserves_the_address_and_pins_the_mac() {
        let (service, backend, dir) = service("mgmt-bridge");
        let response = service.management_bridge().await.unwrap();
        assert!(response.converted);
        assert_eq!(response.bridge, "br0");

        let state = backend.state();
        let br0 = state.link("br0").expect("bridge exists");
        assert_eq!(br0.addresses, vec!["192.168.10.5/24".to_string()]);
        assert_eq!(br0.mac.as_deref(), Some("52:54:00:aa:bb:00"));
        assert_eq!(br0.ports, vec!["nic0".to_string()]);
        assert!(
            state.link("nic0").unwrap().addresses.is_empty(),
            "the address moved, it was not duplicated"
        );

        service.confirm().await.unwrap();
        let config = service.config().await.unwrap();
        assert_eq!(config.management.link, "br0");
        // The address moved onto the bridge; the port keeps none.
        assert!(config.ip_of("br0").is_addressed());
        assert_eq!(config.ip_of("nic0"), IpConfig::Disabled);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn converting_an_already_bridged_management_link_is_idempotent_success() {
        let (service, _backend, dir) = service("mgmt-bridge-twice");
        service.management_bridge().await.unwrap();
        service.confirm().await.unwrap();

        let second = service.management_bridge().await.unwrap();
        assert!(!second.converted);
        assert_eq!(second.bridge, "br0");
        assert!(second.checkpoint.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_conversion_refuses_to_run_over_staged_changes() {
        let (service, _backend, dir) = service("mgmt-bridge-staged");
        service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        let err = service.management_bridge().await.unwrap_err();
        assert!(matches!(err, NetError::Conflict(_)), "{err:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn interfaces_report_management_and_deletability() {
        let (service, _backend, dir) = service("views");
        let response = service.interfaces().await.unwrap();
        assert_eq!(response.nodes.len(), 1);
        let rows = &response.nodes[0].interfaces;

        let nic0 = rows.iter().find(|r| r.name == "nic0").unwrap();
        assert!(nic0.management);
        assert!(!nic0.deletable);
        assert!(nic0.delete_blocked_reason.is_some());
        assert_eq!(nic0.addresses, vec!["192.168.10.5/24".to_string()]);

        let nic1 = rows.iter().find(|r| r.name == "nic1").unwrap();
        assert!(!nic1.management);
        assert!(!nic1.deletable, "a physical NIC is never deletable");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn rows_carry_the_alternative_name_and_never_the_loopback() {
        let (service, _backend, dir) = service("altnames");
        let rows = service
            .interfaces()
            .await
            .unwrap()
            .nodes
            .remove(0)
            .interfaces;
        assert!(
            !rows.iter().any(|r| r.name == "lo"),
            "loopback is not a row anyone can act on"
        );
        let nic0 = rows.iter().find(|r| r.name == "nic0").unwrap();
        assert_eq!(nic0.altname.as_deref(), Some("enp1s0"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The three columns that come from the configuration rather than from the
    /// box: a comment, VLAN awareness, and the addressing being asked for.
    #[tokio::test]
    async fn a_bridge_reports_its_comment_addressing_and_vlan_awareness() {
        let (service, _backend, dir) = service("bridge-view");
        service
            .create_bridge(Bridge {
                vlan_filtering: true,
                comment: Some("guest traffic".into()),
                ip: IpConfig::Static {
                    cidr: "10.0.0.5/24".into(),
                    gateway: "10.0.0.1".into(),
                    dns: vec![],
                },
                ..bridge("br1", &["nic1"])
            })
            .await
            .unwrap();
        let rows = service
            .interfaces()
            .await
            .unwrap()
            .nodes
            .remove(0)
            .interfaces;
        let br1 = rows.iter().find(|r| r.name == "br1").unwrap();
        assert!(br1.vlan_aware);
        assert_eq!(br1.comment.as_deref(), Some("guest traffic"));
        // An empty comment on create is no comment, exactly as on update —
        // the console sends "" for a field the operator left blank.
        service
            .create_bridge(Bridge {
                comment: Some("   ".into()),
                ..bridge("br2", &[])
            })
            .await
            .unwrap();
        assert_eq!(
            service
                .pending()
                .await
                .unwrap()
                .target
                .unwrap()
                .comment_of("br2"),
            None
        );
        assert!(matches!(br1.ip, IpConfig::Static { .. }));
        // Staged, so the box itself still reports no address on it.
        assert!(br1.addresses.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A comment is a change like any other: staged, applied, committed.
    #[tokio::test]
    async fn a_comment_can_be_set_and_cleared_on_a_nic() {
        let (service, _backend, dir) = service("nic-comment");
        service
            .update_nic(
                "nic1",
                NicPatch {
                    comment: Some("  spare  ".into()),
                    ..NicPatch::default()
                },
            )
            .await
            .unwrap();
        let target = service.pending().await.unwrap().target.unwrap();
        assert_eq!(target.comment_of("nic1"), Some("spare"));

        service
            .update_nic(
                "nic1",
                NicPatch {
                    comment: Some(String::new()),
                    ..NicPatch::default()
                },
            )
            .await
            .unwrap();
        let target = service.pending().await.unwrap().target.unwrap();
        assert_eq!(target.comment_of("nic1"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn staged_links_appear_in_the_table_with_a_badge() {
        let (service, _backend, dir) = service("badges");
        service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        let rows = service
            .interfaces()
            .await
            .unwrap()
            .nodes
            .remove(0)
            .interfaces;

        let br1 = rows.iter().find(|r| r.name == "br1").unwrap();
        assert_eq!(br1.change, ChangeState::Created);
        assert!(!br1.present, "it is staged, not on the box yet");
        assert!(br1.deletable);
        assert_eq!(br1.ports, vec!["nic1".to_string()]);

        let nic1 = rows.iter().find(|r| r.name == "nic1").unwrap();
        assert_eq!(nic1.change, ChangeState::Modified);
        assert_eq!(nic1.controller.as_deref(), Some("br1"));
        // A controller is listed immediately before the ports it owns.
        let br1_at = rows.iter().position(|r| r.name == "br1").unwrap();
        let nic1_at = rows.iter().position(|r| r.name == "nic1").unwrap();
        assert_eq!(nic1_at, br1_at + 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_staged_delete_stays_visible_until_it_is_applied() {
        let (service, _backend, dir) = service("staged-delete");
        service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        service.apply(Acknowledgements::default()).await.unwrap();
        service.confirm().await.unwrap();

        service.delete_link("br1", LinkKind::Bridge).await.unwrap();
        let rows = service
            .interfaces()
            .await
            .unwrap()
            .nodes
            .remove(0)
            .interfaces;
        let br1 = rows.iter().find(|r| r.name == "br1").expect("still shown");
        assert_eq!(br1.change, ChangeState::Deleted);
        assert!(br1.present);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn moving_the_management_address_needs_the_acknowledgement() {
        let (service, _backend, dir) = service("ack");
        // Build the target by hand: moving the address between links is the
        // conversion's job in the API, but the guard has to hold for any
        // target that does it, however it got staged.
        let mut target = service.config().await.unwrap();
        let moved = target.ip_of("nic0");
        target.bridges.push(Bridge {
            ip: moved,
            ..bridge("br1", &["nic1"])
        });
        target.set_ip("nic0", IpConfig::Disabled);
        target.management.link = "br1".into();
        service.store.set_pending(&target).unwrap();

        let pending = service.pending().await.unwrap();
        assert!(pending.requires_disconnect_ack);

        let err = service
            .apply(Acknowledgements::default())
            .await
            .unwrap_err();
        match err {
            NetError::Invalid(errors) => {
                assert_eq!(errors[0].code, ValidationCode::ManagementDisconnect)
            }
            other => panic!("expected the disconnect guard, got {other:?}"),
        }
        assert!(service
            .apply(Acknowledgements {
                may_disconnect: true
            })
            .await
            .is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_failed_apply_rolls_back_instead_of_waiting_out_the_window() {
        let (service, backend, dir) = service("apply-fail");
        let before = backend.state();
        service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        backend.fail_apply_after(1);

        assert!(service.apply(Acknowledgements::default()).await.is_err());
        assert_eq!(backend.state(), before);
        assert!(service.pending().await.unwrap().checkpoint.is_none());
        // …and a retry is possible immediately.
        assert!(service.apply(Acknowledgements::default()).await.is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn startup_rolls_back_an_orphaned_checkpoint() {
        let (service, backend, dir) = service("orphan");
        let before = backend.state();
        // A checkpoint nothing has a record of: a previous run died mid-apply.
        backend.checkpoint_create(60).await.unwrap();
        backend
            .apply(&[Op::AddConnection {
                spec: crate::plan::specs(&{
                    let mut d = service.config().await.unwrap();
                    d.bridges.push(bridge("br9", &[]));
                    d
                })["br9"]
                    .clone(),
            }])
            .await
            .unwrap();

        service.reconcile().await.unwrap();
        assert!(backend.checkpoints().await.unwrap().is_empty());
        assert_eq!(backend.state(), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn startup_adopts_a_checkpoint_it_has_a_record_for() {
        let (service, backend, dir) = service("adopt");
        service
            .create_bridge(bridge("br1", &["nic1"]))
            .await
            .unwrap();
        service.apply(Acknowledgements::default()).await.unwrap();

        service.reconcile().await.unwrap();
        assert_eq!(backend.checkpoints().await.unwrap().len(), 1);
        assert!(service.pending().await.unwrap().checkpoint.is_some());
        // …and it can still be confirmed.
        assert!(service.confirm().await.is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bridge_names_fill_the_first_free_slot() {
        let mut desired = NetworkDesiredState::default();
        assert_eq!(next_bridge_name(&desired), "br0");
        desired.bridges.push(bridge("br0", &[]));
        assert_eq!(next_bridge_name(&desired), "br1");
    }
}
