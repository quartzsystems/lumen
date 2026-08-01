//! The compute seam over vdisks and leases: `VmVolumes`, served.
//!
//! Everything here is naming and fan-out. The engine owns the bytes, the
//! daemon owns the sockets, and this service's whole job is to turn five
//! verbs the compute domain already speaks into control commands, while
//! respecting two facts about LumenFS:
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

use crate::error::{PoolError, Result, Result as SeamResult};
use crate::fleet::PoolFleet;
use crate::model::{self, DiskName};
use crate::state::{self, MemberView, PoolMember, PoolState, SnapshotView};
use crate::vm::{MigrationWindow, ReplicatedDisk, VmDiskRequest, VmVolumes};

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

    /// The pool's name — its cluster's, so every page calls it one thing.
    pub fn name(&self) -> &str {
        &self.pool
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

    /// Take a snapshot of a machine's disk.
    ///
    /// Replicated by the engine, so one member is told and both have it —
    /// which is the property that makes a snapshot worth offering at all: one
    /// that lived only on the member that took it would be gone exactly when
    /// it was wanted, after that member died.
    ///
    /// The id is the caller's: the console passes a Unix timestamp so the
    /// ids sort and display as the moments they are, and the engine refuses
    /// a duplicate rather than overwriting one.
    pub async fn snapshot(&self, device: &str, snapshot: u64) -> Result<()> {
        let vdisk = self.pooled(device)?;
        let here = self.here().await?;
        self.fleet.snapshot(&here, vdisk, snapshot).await
    }

    /// One disk's snapshots, newest id last.
    pub async fn snapshots(&self, device: &str) -> Result<Vec<SnapshotView>> {
        let vdisk = self.pooled(device)?;
        let here = self.here().await?;
        let mut listed: Vec<SnapshotView> = self
            .fleet
            .snapshots(&here, Some(vdisk))
            .await?
            .into_iter()
            .map(|(_, snapshot, size_bytes)| SnapshotView {
                snapshot,
                size_bytes,
            })
            .collect();
        listed.sort_by_key(|s| s.snapshot);
        Ok(listed)
    }

    /// Drop a snapshot, releasing whatever it alone was pinning.
    pub async fn delete_snapshot(&self, device: &str, snapshot: u64) -> Result<()> {
        let vdisk = self.pooled(device)?;
        let here = self.here().await?;
        self.fleet.delete_snapshot(&here, vdisk, snapshot).await
    }

    /// Put a disk back to a snapshot — **refused while it is open anywhere
    /// in the pool**.
    ///
    /// This is the contract the console has always promised for a rollback,
    /// and here it has to be built rather than inherited: the engine swaps
    /// the map root whenever it is asked and has no idea a guest is mounted
    /// on the other side of a device. Rolling back underneath a running
    /// machine would replace every block beneath a live filesystem without
    /// telling it.
    ///
    /// So every member is asked what it is serving first, and a single
    /// export anywhere refuses the whole operation. The member that owns the
    /// disk refuses too, which makes this belt and braces — deliberately,
    /// because the orchestration layer must not be the only thing standing
    /// between an operator and a corrupted guest.
    ///
    /// A member that cannot be reached also refuses it. That is the
    /// conservative direction: a silent member might be the one serving the
    /// disk, and "I could not check" must never read as "nobody has it".
    pub async fn rollback(&self, device: &str, snapshot: u64) -> Result<()> {
        let vdisk = self.pooled(device)?;
        let members = self.members().await?;
        for member in &members {
            let serving = self.fleet.exports(member).await.map_err(|err| {
                PoolError::Conflict(format!(
                    "cannot roll back {device}: {member} could not say whether it is serving the \
                     disk ({err})"
                ))
            })?;
            if serving.iter().any(|(id, _)| *id == vdisk) {
                return Err(PoolError::Conflict(format!(
                    "{device} is in use on {member} — stop the machine using it before rolling back"
                )));
            }
        }
        let here = &members[0];
        self.fleet.rollback(here, vdisk, snapshot).await
    }

    /// The vdisk behind a device path, refusing anything that is not a
    /// pooled machine disk by name rather than guessing.
    fn pooled(&self, device: &str) -> Result<u64> {
        model::vdisk_of_device(device)
            .filter(|vdisk| DiskName::from_vdisk(*vdisk).is_some())
            .ok_or_else(|| PoolError::NotFound(format!("{device} is not a disk this pool serves")))
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
        // nothing is asked twice. Snapshots are replicated too, but they are
        // not in the status line, so the same speaker is asked once for the
        // whole pool's rather than once per disk.
        let speaker = members
            .iter()
            .filter(|m| m.view.status().is_some())
            .map(|m| m.name.clone())
            .next();
        let snapshots = match &speaker {
            Some(speaker) => self
                .fleet
                .snapshots(speaker, None)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
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
                        state::vdisk_view(*vdisk, *size_bytes, lease, &exports, &snapshots)
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
        // Tier 0 for every machine disk until the create dialog offers the
        // choice — the compute seam deliberately says nothing about tiers.
        self.fleet
            .create_vdisk(&here, vdisk, request.size_bytes, 0)
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
                return Err(err);
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
            )));
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
                    )))
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
                )));
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
            )));
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
    /// `Accepted` hands the pen over from the source, waits for the
    /// destination to see it, and then takes the source's own device down —
    /// the mirror of what `Aborted` does to the destination's. Where the pen
    /// goes is read from the open window rather than passed in — the window
    /// is the record of its own destination, so there is no second place to
    /// disagree. A source export left standing is not a second writer (it is
    /// penless), but it is the export that would refuse the machine's way
    /// back — found on real hardware when the return migration was refused
    /// with "already exported".
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
            )));
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
                    return Err(err);
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
                        )))
                    }
                };
                self.fleet.relinquish(&here, vdisk, to).await?;
                // Wait for the far side to agree, so a caller told the
                // migration finished is told the truth. The window already
                // recorded *which* member the pen went to; at two members
                // "the one that isn't me" was the same answer, at three it
                // is an arbitrary wrong peer — so the name comes from the
                // recorded id, never from elimination.
                let mut destination = None;
                for member in disk.members.iter().filter(|member| *member != &here) {
                    if self.fleet.node_id(member).await? == to {
                        destination = Some(member.clone());
                        break;
                    }
                }
                let destination = destination.ok_or_else(|| {
                    PoolError::Backend(format!(
                        "the window on \"{device}\" names node {to}, which is no member of it"
                    ))
                })?;
                for attempt in 0..HANDOVER_TRIES {
                    match self.fleet.accept(&destination, vdisk).await {
                        Ok(()) => break,
                        Err(err) if attempt + 1 == HANDOVER_TRIES => {
                            return Err(PoolError::Backend(format!(
                                "the pen for \"{device}\" was handed to {destination} but never \
                                 arrived: {err}"
                            )))
                        }
                        Err(_) => tokio::time::sleep(HANDOVER_PAUSE).await,
                    }
                }
                // The machine writes on the destination now. The source's
                // penless export cannot take a second writer, but it is a
                // device nobody will use again — and the export that would
                // refuse the machine's way back. Aborted takes the far
                // side's device down; this is its mirror.
                self.fleet.unexport(&here, vdisk).await?;
                Ok(())
            }
            MigrationWindow::Aborted => {
                // Who the window aimed at, read before the abort closes
                // it: only that member's device is unwanted — a third
                // member was never part of this migration, and unexporting
                // it would tear down a device an HA restart may be using.
                let aimed = match self.fleet.lease(&here, vdisk).await? {
                    Some((_, Some(to))) => Some(to),
                    _ => None,
                };
                self.fleet.abort(&here, vdisk).await?;
                // The device on the abandoned destination is no longer
                // wanted. Its failure must not mask the migration's own,
                // so it is logged rather than raised.
                for member in disk.members.iter().filter(|member| *member != &here) {
                    if let Some(to) = aimed {
                        if self.fleet.node_id(member).await.ok() != Some(to) {
                            continue;
                        }
                    }
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
