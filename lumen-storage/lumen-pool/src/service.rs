//! The compute seam over vdisks and leases: `VmVolumes`, implemented a
//! second time.
//!
//! Everything here is naming and fan-out. The engine owns the bytes, the
//! daemon owns the sockets, and this service's whole job is to turn five
//! verbs the compute domain already speaks into control commands, while
//! respecting two facts about LumenFS that DRBD did not have:
//!
//! - **Creating and deleting a vdisk replicates by itself.** One member is
//!   told; both have it. There is nothing to fan out and nothing to unwind
//!   halfway.
//! - **Exports do not.** `/dev/ublkb<id>` is the same string on every
//!   member — which is what keeps one domain document valid everywhere —
//!   but the device exists only where the daemon has exported it. So the
//!   device is materialized where the machine is: here at create, and on
//!   the destination when a migration window opens.
//!
//! The second fact leaves one gap this slice does not close, and it is
//! recorded rather than papered over: an HA restart starts a machine on a
//! survivor without passing through `migration_window`, so nothing has
//! exported the device there. See the note on [`PoolService::migration_window`].

use async_trait::async_trait;
use lumen_drbd::{MigrationWindow, ReplicatedDisk, Result as SeamResult, VmDiskRequest, VmVolumes};

use crate::error::{PoolError, Result};
use crate::fleet::PoolFleet;
use crate::model::{self, DiskName};
use crate::state::{self, MemberView, PoolMember, PoolState};

/// How long the source waits for the destination to see the pen arrive.
/// Generous: it is one op crossing a healthy link, and the alternative to
/// waiting is telling the compute domain a migration finished before the
/// storage layer agrees.
const HANDOVER_TRIES: u32 = 100;
const HANDOVER_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

pub struct PoolService {
    fleet: std::sync::Arc<dyn PoolFleet>,
    /// The pool's name, reported as the disk's `cluster` so the storage and
    /// machine pages name the same thing.
    pool: String,
}

impl PoolService {
    pub fn new(fleet: std::sync::Arc<dyn PoolFleet>, pool: &str) -> PoolService {
        PoolService {
            fleet,
            pool: pool.to_string(),
        }
    }

    /// The pool's members, this node first — or a refusal that says there is
    /// no pool here rather than returning an empty list nobody checks.
    async fn members(&self) -> Result<Vec<String>> {
        let members = self.fleet.members().await?;
        if members.is_empty() {
            return Err(PoolError::Unavailable(
                "This node carries no LumenFS pool.".into(),
            ));
        }
        Ok(members)
    }

    async fn here(&self) -> Result<String> {
        Ok(self.members().await?[0].clone())
    }

    /// The disk behind a device path, if this engine made it. Every `None`
    /// here is a real answer: not our path, no pool, no such vdisk, or an
    /// id that decodes to no machine (a formatted brick's own vdisk 1 among
    /// them).
    async fn lookup(&self, device: &str) -> Result<Option<ReplicatedDisk>> {
        let Some(vdisk) = model::vdisk_of_device(device) else {
            return Ok(None);
        };
        let Some(name) = DiskName::from_vdisk(vdisk) else {
            return Ok(None);
        };
        let members = match self.fleet.members().await {
            Ok(members) if !members.is_empty() => members,
            _ => return Ok(None),
        };
        let Some((_, size_bytes)) = self
            .fleet
            .vdisks(&members[0])
            .await?
            .into_iter()
            .find(|(id, _)| *id == vdisk)
        else {
            return Ok(None);
        };
        Ok(Some(ReplicatedDisk {
            cluster: self.pool.clone(),
            name: name.to_string(),
            device: model::device_path(vdisk),
            size_bytes,
            members,
        }))
    }

    /// The pool as it is right now — one call, everything a console page
    /// renders. Read live and never stored: a pool's health is a fact about
    /// this instant.
    ///
    /// **No member's silence fails this.** Asking every member and refusing
    /// the whole view because one did not answer would blank the page in
    /// exactly the situation an operator opened it to understand, so a
    /// member that cannot be reached becomes a [`MemberView::Silent`] beside
    /// the others and the verdict downgrades to
    /// [`PoolHealth::Unknown`](crate::state::PoolHealth::Unknown). The
    /// replicated listings are read from the first member that *does*
    /// answer, because any member answers the same — and if none does, the
    /// members are still reported with their reasons.
    pub async fn state(&self) -> PoolState {
        let names = match self.fleet.members().await {
            Ok(names) if !names.is_empty() => names,
            // No pool here, or a fleet that cannot say: either way there is
            // nothing to describe, and that is a state rather than an error.
            _ => return PoolState::none(),
        };

        let mut members = Vec::with_capacity(names.len());
        for name in &names {
            let view = match self.fleet.status(name).await {
                Ok(status) => MemberView::Answered(status),
                Err(why) => MemberView::Silent(why.to_string()),
            };
            members.push(PoolMember {
                name: name.clone(),
                view,
            });
        }

        // Exports are per-member, so every member is asked; a silent one
        // contributes nothing rather than an empty list that would read as
        // "serving nothing".
        let answering: Vec<String> = members
            .iter()
            .filter(|m| m.view.status().is_some())
            .map(|m| m.name.clone())
            .collect();
        let mut exports: Vec<(String, Vec<u64>)> = Vec::new();
        for name in &answering {
            if let Ok(serving) = self.fleet.exports(name).await {
                exports.push((
                    name.clone(),
                    serving.into_iter().map(|(id, _)| id).collect(),
                ));
            }
        }

        // The vdisk listing and the leases are replicated, and they arrived
        // with the status — so one answering member speaks for the pool and
        // nothing is asked twice.
        let vdisks = members
            .iter()
            .filter_map(|m| m.view.status())
            .next()
            .map(|speaker| {
                speaker
                    .vdisks
                    .iter()
                    .map(|(vdisk, size_bytes)| {
                        let lease = speaker
                            .lease(*vdisk)
                            .map(|seen| (seen.holder, seen.handing_to));
                        state::vdisk_view(*vdisk, *size_bytes, lease, &exports)
                    })
                    .collect()
            })
            .unwrap_or_default();

        PoolState::assemble(members, vdisks)
    }
}

#[async_trait]
impl VmVolumes for PoolService {
    async fn create_disk(&self, request: &VmDiskRequest) -> SeamResult<ReplicatedDisk> {
        let name = DiskName::parse(&request.name).ok_or_else(|| {
            PoolError::Conflict(format!(
                "\"{}\" is not a machine disk name this pool can place. Pooled disks take \
                 their identity from the name, so it must read vm-<vmid>-disk-<n>, with at \
                 most {} disks on a machine.",
                request.name,
                model::DISKS_PER_MACHINE
            ))
        })?;
        let vdisk = name
            .vdisk()
            .ok_or_else(|| PoolError::Conflict(format!("\"{name}\" is outside this pool's ids")))?;
        let members = self.members().await?;
        let here = members[0].clone();

        // Creation replicates, so one member is told and both have it.
        self.fleet
            .create_vdisk(&here, vdisk, request.size_bytes)
            .await?;

        // The device is materialized where the machine is. If that fails,
        // the vdisk goes with it — a disk nobody can open is worse than no
        // disk, because it looks like progress.
        let device = match self.fleet.export(&here, vdisk).await {
            Ok(device) => device,
            Err(err) => {
                if let Err(unwind) = self.fleet.delete_vdisk(&here, vdisk).await {
                    tracing::error!(
                        %name, %unwind,
                        "the export failed and the vdisk could not be removed either"
                    );
                }
                return Err(err.into());
            }
        };
        tracing::info!(%name, vdisk, %device, "pooled disk created");
        Ok(ReplicatedDisk {
            cluster: self.pool.clone(),
            name: name.to_string(),
            device,
            size_bytes: request.size_bytes,
            members,
        })
    }

    async fn disk_of(&self, device: &str) -> SeamResult<Option<ReplicatedDisk>> {
        Ok(self.lookup(device).await?)
    }

    async fn destroy_disk(&self, device: &str) -> SeamResult<()> {
        let Some(disk) = self.lookup(device).await? else {
            return Err(PoolError::NotFound(format!(
                "\"{device}\" is not a pooled disk this node knows."
            ))
            .into());
        };
        let vdisk = model::vdisk_of_device(device).expect("looked up by this path");

        // Every device first, then the vdisk. Deleting the vdisk while a
        // member still served it would leave a block device whose backing
        // store is gone — a reader that hangs or lies.
        for member in &disk.members {
            match self.fleet.exports(member).await {
                Ok(serving) if serving.iter().any(|(id, _)| *id == vdisk) => {
                    self.fleet.unexport(member, vdisk).await?;
                }
                Ok(_) => {}
                // A member we cannot reach cannot be serving anything we can
                // rely on either; deleting would strand it.
                Err(err) => {
                    return Err(PoolError::Backend(format!(
                        "cannot tell whether {member} is still serving {device}: {err}"
                    ))
                    .into())
                }
            }
        }
        self.fleet.delete_vdisk(&disk.members[0], vdisk).await?;
        tracing::info!(name = %disk.name, vdisk, "pooled disk destroyed");
        Ok(())
    }

    async fn common_members(&self, devices: &[String]) -> SeamResult<Vec<String>> {
        let members = self.members().await?;
        for device in devices {
            if self.lookup(device).await?.is_none() {
                return Err(PoolError::Conflict(format!(
                    "\"{device}\" is not a pooled disk, so the machines using it cannot be \
                     placed by this pool."
                ))
                .into());
            }
        }
        // Every member of a quorate pool can serve every vdisk: placement
        // is by content hash, so no member holds a better share of any one
        // disk than another. That is the whole of pooled HA eligibility —
        // the sweep asks this question and needs no other change.
        Ok(members)
    }

    /// Make the device exist here, because a machine is about to start.
    ///
    /// This is the verb an HA restart needs and the reason it exists: the
    /// sweep defines and starts a machine on a survivor without passing
    /// through a migration window, and a pooled device exists only where
    /// the daemon serves it. Starting a machine after the daemon restarted
    /// needs the same thing, which is why it belongs on every start rather
    /// than only on the failover path.
    ///
    /// Idempotent, and cheap when there is nothing to do: an export already
    /// up is left alone rather than cycled, because cycling it would take
    /// the device out from under whatever is using it.
    async fn ensure_local_device(&self, device: &str) -> SeamResult<()> {
        let Some(disk) = self.lookup(device).await? else {
            return Err(PoolError::NotFound(format!(
                "\"{device}\" is not a pooled disk this node knows."
            ))
            .into());
        };
        let vdisk = model::vdisk_of_device(device).expect("looked up by this path");
        let here = self.here().await?;
        if self
            .fleet
            .exports(&here)
            .await?
            .iter()
            .any(|(id, _)| *id == vdisk)
        {
            return Ok(());
        }
        // The export's own attach decides whether this node may write:
        // ours after a fence verdict retired the dead holder's lease,
        // penless inside a migration window, refused if a live peer holds
        // it — which is the right refusal, because then the machine should
        // not be starting here at all.
        self.fleet.export(&here, vdisk).await?;
        tracing::info!(name = %disk.name, vdisk, %here, "pooled device readied for a local start");
        Ok(())
    }

    /// The migration window, mapped onto the lease.
    ///
    /// `Open` opens the window **and exports on the destination**, because
    /// the destination needs its device before libvirt starts the domain
    /// there, and inside a window that export is penless: reads serve,
    /// writes refuse until the pen arrives.
    ///
    /// `Accepted` hands the pen over from the source and then waits for the
    /// destination to see it. Where it goes is read from the open window
    /// rather than passed in — the window is the record of its own
    /// destination, so there is no second place to disagree.
    ///
    /// **The recorded gap.** An HA restart does not pass through here: the
    /// sweep defines and starts the machine on a survivor, and nothing has
    /// exported the device on that node. Two ways to close it, neither
    /// chosen yet — let the export happen at attach time on any member
    /// (which means relaxing the daemon's "somebody else's disk" refusal to
    /// a penless open), or give the seam a verb for "make this device exist
    /// here". Until then, pooled disks migrate but do not fail over.
    async fn migration_window(&self, device: &str, window: MigrationWindow) -> SeamResult<()> {
        let Some(disk) = self.lookup(device).await? else {
            return Err(PoolError::NotFound(format!(
                "\"{device}\" is not a pooled disk this node knows."
            ))
            .into());
        };
        let vdisk = model::vdisk_of_device(device).expect("looked up by this path");
        let here = self.here().await?;

        match window {
            MigrationWindow::Open { destination } => {
                let to = self.fleet.node_id(&destination).await?;
                self.fleet.handover(&here, vdisk, to).await?;
                // Penless: the destination holds the disk open while the
                // machine still writes here.
                if let Err(err) = self.fleet.export(&destination, vdisk).await {
                    // Leave nothing half-open: a window with no device on
                    // the far side is a migration that cannot proceed.
                    if let Err(unwind) = self.fleet.abort(&here, vdisk).await {
                        tracing::error!(%device, %unwind, "the window would not close either");
                    }
                    return Err(err.into());
                }
                Ok(())
            }
            MigrationWindow::Accepted => {
                let to = match self.fleet.lease(&here, vdisk).await? {
                    Some((_, Some(to))) => to,
                    _ => {
                        return Err(PoolError::Conflict(format!(
                            "\"{device}\" has no migration window open, so there is no one to \
                             hand it to."
                        ))
                        .into())
                    }
                };
                self.fleet.relinquish(&here, vdisk, to).await?;
                // Wait for the far side to agree, so a caller told the
                // migration finished is told the truth.
                let destination = disk
                    .members
                    .iter()
                    .find(|member| *member != &here)
                    .cloned()
                    .unwrap_or_else(|| here.clone());
                for attempt in 0..HANDOVER_TRIES {
                    match self.fleet.accept(&destination, vdisk).await {
                        Ok(()) => return Ok(()),
                        Err(err) if attempt + 1 == HANDOVER_TRIES => {
                            return Err(PoolError::Backend(format!(
                                "the pen for \"{device}\" was handed to {destination} but never \
                                 arrived: {err}"
                            ))
                            .into())
                        }
                        Err(_) => tokio::time::sleep(HANDOVER_PAUSE).await,
                    }
                }
                Ok(())
            }
            MigrationWindow::Aborted => {
                self.fleet.abort(&here, vdisk).await?;
                // The device on the far side is no longer wanted. Its
                // failure must not mask the migration's own, so it is
                // logged rather than raised.
                for member in disk.members.iter().filter(|member| *member != &here) {
                    if let Err(err) = self.fleet.unexport(member, vdisk).await {
                        tracing::warn!(
                            %device, %member, %err,
                            "the abandoned destination export did not come down"
                        );
                    }
                }
                Ok(())
            }
        }
    }
}
