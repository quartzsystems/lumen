//! The pool's members, as something the service can call.
//!
//! One trait, one verb per thing the seam needs, each addressed to a
//! member by name. The real implementation is a [`lumen_fsd::Client`] per
//! member over the Core network; the mock here is an in-memory pool that
//! models the parts of the engine's behaviour the service depends on —
//! notably that **creating and deleting a vdisk replicates by itself**
//! while **exports are per-member**, which is the asymmetry the whole
//! service is shaped around.
//!
//! The mock is not a stub returning `Ok`. It refuses what the daemon
//! refuses: exporting a vdisk that does not exist, exporting one whose pen
//! another member holds outside a migration window, and handing over a
//! window nobody opened. A mock that says yes to everything would let the
//! service's tests pass while the real thing failed at the first refusal.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::{PoolError, Result};
use crate::state::{BrickSeen, LeaseSeen, MemberStatus, Replication, TierCapacitySeen};

/// What a member's daemon can be asked. Names are cluster node names; the
/// `u8` ids are the node ids the engine's leases speak.
#[async_trait]
pub trait PoolFleet: Send + Sync {
    /// The pool's members, this node first. Empty means there is no pool
    /// on this node — the standalone appliance.
    async fn members(&self) -> Result<Vec<String>>;

    /// The engine's node id for a member, which is what a lease names.
    async fn node_id(&self, member: &str) -> Result<u8>;

    /// Everything one member says about itself in a single round trip:
    /// replication state, era, brick space, stream counters. The console's
    /// view is built from one of these per member, so it is one call rather
    /// than five.
    async fn status(&self, member: &str) -> Result<MemberStatus>;

    /// Every vdisk the pool holds, as `(id, size_bytes)`. Replicated
    /// state, so any member answers the same.
    async fn vdisks(&self, member: &str) -> Result<Vec<(u64, u64)>>;

    /// Create a vdisk on a tier. Replicated: one member is enough.
    async fn create_vdisk(&self, member: &str, vdisk: u64, size_bytes: u64, tier: u8)
        -> Result<()>;

    /// Every brick of one member's set, with its space in bytes.
    /// Per-member: each set is that machine's own disks.
    async fn brick_list(&self, member: &str) -> Result<Vec<BrickSeen>>;

    /// Delete a vdisk. Replicated: one member is enough.
    async fn delete_vdisk(&self, member: &str, vdisk: u64) -> Result<()>;

    /// Serve a vdisk as a guest device on this member, returning the path.
    /// Per-member: the path is the same string everywhere, but the device
    /// exists only where it has been exported.
    async fn export(&self, member: &str, vdisk: u64) -> Result<String>;

    /// Stop serving it here. The lease is untouched.
    async fn unexport(&self, member: &str, vdisk: u64) -> Result<()>;

    /// What this member is serving, as `(vdisk, device)`.
    async fn exports(&self, member: &str) -> Result<Vec<(u64, String)>>;

    /// Take a snapshot. Replicated: one member is enough, and both have it.
    async fn snapshot(&self, member: &str, vdisk: u64, snapshot: u64) -> Result<()>;

    /// Every snapshot, or one vdisk's, as `(vdisk, snapshot, size_bytes)`.
    /// Replicated state, so any member answers the same.
    async fn snapshots(&self, member: &str, vdisk: Option<u64>) -> Result<Vec<(u64, u64, u64)>>;

    /// Drop a snapshot. Replicated.
    async fn delete_snapshot(&self, member: &str, vdisk: u64, snapshot: u64) -> Result<()>;

    /// Put a vdisk back to a snapshot. Replicated — and refused by the
    /// member if it is serving the disk, which is the last line of a
    /// defence the orchestration layer makes first.
    async fn rollback(&self, member: &str, vdisk: u64, snapshot: u64) -> Result<()>;

    /// Who holds a vdisk's pen as this member sees it, and whether a window
    /// is open: `(holder, handing_to)`. `None` means nobody has claimed it.
    /// The open window is the record of its own destination, which is how a
    /// handover knows where it was aimed without being told twice.
    async fn lease(&self, member: &str, vdisk: u64) -> Result<Option<(u8, Option<u8>)>>;

    /// Open the migration window toward `to`. Runs on the holder.
    async fn handover(&self, member: &str, vdisk: u64, to: u8) -> Result<()>;

    /// Hand the pen over. Runs on the holder; the destination then sees it.
    async fn relinquish(&self, member: &str, vdisk: u64, to: u8) -> Result<()>;

    /// Close a window the migration did not use. Runs on the holder.
    async fn abort(&self, member: &str, vdisk: u64) -> Result<()>;

    /// Ask whether the pen has arrived and settled. Runs on the
    /// destination, and takes nothing.
    async fn accept(&self, member: &str, vdisk: u64) -> Result<()>;
}

// ---------------------------------------------------------------------------
// The mock: an in-memory pool that refuses what the real one refuses.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lease {
    holder: u8,
    handing_to: Option<u8>,
}

#[derive(Default)]
struct MockInner {
    /// `None` is the standalone appliance: no pool here.
    members: Option<Vec<String>>,
    vdisks: BTreeMap<u64, u64>,
    /// `(vdisk, snapshot) -> size_bytes`. Replicated, like the vdisks.
    snapshots: BTreeMap<(u64, u64), u64>,
    leases: BTreeMap<u64, Lease>,
    /// Per member, the vdisks it is serving — the asymmetry that matters.
    exports: BTreeMap<String, Vec<u64>>,
    fail_next_export: bool,
    /// What a member reports about itself, when a test cares. Absent means
    /// the plausible default: Synced, era 1, room to spare.
    statuses: BTreeMap<String, MemberStatus>,
    /// Members whose daemon cannot be reached — the case a view must
    /// present rather than drop.
    silent: BTreeMap<String, String>,
}

pub struct MockFleet {
    inner: Mutex<MockInner>,
}

impl MockFleet {
    /// No pool on this node: every call explains itself.
    pub fn standalone() -> MockFleet {
        MockFleet {
            inner: Mutex::new(MockInner::default()),
        }
    }

    /// A pool whose members are `members`, this node first. Node ids are
    /// assigned by position, matching how a two-node pool is formatted.
    pub fn pooled(members: &[&str]) -> MockFleet {
        MockFleet {
            inner: Mutex::new(MockInner {
                members: Some(members.iter().map(|m| (*m).to_string()).collect()),
                ..MockInner::default()
            }),
        }
    }

    /// Make the next export fail once — the moment a create must unwind.
    pub fn fail_next_export(&self) {
        self.inner.lock().unwrap().fail_next_export = true;
    }

    /// Make a member report something other than the healthy default.
    pub fn set_status(&self, member: &str, status: MemberStatus) {
        self.inner
            .lock()
            .unwrap()
            .statuses
            .insert(member.to_string(), status);
    }

    /// Make a member's daemon unreachable, with a reason.
    pub fn silence(&self, member: &str, why: &str) {
        self.inner
            .lock()
            .unwrap()
            .silent
            .insert(member.to_string(), why.to_string());
    }

    /// Which vdisks a member is serving right now.
    pub fn exported_on(&self, member: &str) -> Vec<u64> {
        self.inner
            .lock()
            .unwrap()
            .exports
            .get(member)
            .cloned()
            .unwrap_or_default()
    }

    /// Every vdisk that exists, sorted — replicated state.
    pub fn existing(&self) -> Vec<u64> {
        self.inner.lock().unwrap().vdisks.keys().copied().collect()
    }

    /// Who holds a vdisk's pen, and whether a window is open.
    pub fn lease(&self, vdisk: u64) -> Option<(u8, Option<u8>)> {
        self.inner
            .lock()
            .unwrap()
            .leases
            .get(&vdisk)
            .map(|l| (l.holder, l.handing_to))
    }

    /// The member's engine id — and the one place silence is enforced, so a
    /// silenced member refuses *every* verb rather than only the one a test
    /// happened to think of. An unreachable daemon does not answer some
    /// questions and not others.
    fn id_of(inner: &MockInner, member: &str) -> Result<u8> {
        let members = inner
            .members
            .as_ref()
            .ok_or_else(|| PoolError::Unavailable("this node carries no pool".into()))?;
        let id = members
            .iter()
            .position(|m| m == member)
            .map(|at| at as u8)
            .ok_or_else(|| PoolError::NotFound(format!("{member} is not a member of this pool")))?;
        match inner.silent.get(member) {
            Some(why) => Err(PoolError::Backend(why.clone())),
            None => Ok(id),
        }
    }
}

#[async_trait]
impl PoolFleet for MockFleet {
    async fn members(&self) -> Result<Vec<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .members
            .clone()
            .unwrap_or_default())
    }

    async fn node_id(&self, member: &str) -> Result<u8> {
        let inner = self.inner.lock().unwrap();
        MockFleet::id_of(&inner, member)
    }

    async fn status(&self, member: &str) -> Result<MemberStatus> {
        let inner = self.inner.lock().unwrap();
        let node = MockFleet::id_of(&inner, member)?;
        // The listings come from the mock's own state whether or not a test
        // pinned the health, so a status can never disagree with what the
        // other verbs would answer about the same pool.
        let vdisks = inner.vdisks.iter().map(|(id, size)| (*id, *size)).collect();
        let leases = inner
            .leases
            .iter()
            .map(|(vdisk, lease)| {
                (
                    *vdisk,
                    LeaseSeen {
                        holder: lease.holder,
                        era: 1,
                        handing_to: lease.handing_to,
                    },
                )
            })
            .collect();
        let pinned = inner.statuses.get(member);
        Ok(MemberStatus {
            node,
            replication: pinned.map_or(Replication::Synced, |s| s.replication),
            era: pinned.map_or(1, |s| s.era),
            accepts_writes: pinned.is_none_or(|s| s.accepts_writes),
            segments_free: pinned.map_or(20, |s| s.segments_free),
            segments_total: pinned.map_or(30, |s| s.segments_total),
            usable_bytes: pinned.map_or(30 << 30, |s| s.usable_bytes),
            free_bytes: pinned.map_or(20 << 30, |s| s.free_bytes),
            tiers: pinned.map_or_else(
                || {
                    vec![TierCapacitySeen {
                        tier: 0,
                        usable_bytes: 30 << 30,
                        free_bytes: 20 << 30,
                    }]
                },
                |s| s.tiers.clone(),
            ),
            vdisks,
            leases,
            stream: pinned.map_or((0, 0, 0), |s| s.stream),
        })
    }

    async fn vdisks(&self, member: &str) -> Result<Vec<(u64, u64)>> {
        let inner = self.inner.lock().unwrap();
        MockFleet::id_of(&inner, member)?;
        Ok(inner.vdisks.iter().map(|(id, size)| (*id, *size)).collect())
    }

    async fn create_vdisk(
        &self,
        member: &str,
        vdisk: u64,
        size_bytes: u64,
        _tier: u8,
    ) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let id = MockFleet::id_of(&inner, member)?;
        if inner.vdisks.contains_key(&vdisk) {
            return Err(PoolError::Conflict(format!("vdisk {vdisk} already exists")));
        }
        // Replicated, and whoever made it holds the pen.
        inner.vdisks.insert(vdisk, size_bytes);
        inner.leases.insert(
            vdisk,
            Lease {
                holder: id,
                handing_to: None,
            },
        );
        Ok(())
    }

    async fn brick_list(&self, member: &str) -> Result<Vec<BrickSeen>> {
        let inner = self.inner.lock().unwrap();
        MockFleet::id_of(&inner, member)?;
        // One mock brick per tier the member's pinned status reports —
        // and a single tier-0 holder when nothing is pinned, the shape a
        // fresh real member has.
        let tiers = inner.statuses.get(member).map_or_else(
            || {
                vec![TierCapacitySeen {
                    tier: 0,
                    usable_bytes: 30 << 30,
                    free_bytes: 20 << 30,
                }]
            },
            |s| s.tiers.clone(),
        );
        Ok(tiers
            .iter()
            .map(|t| BrickSeen {
                path: format!("/dev/disk/by-id/mock-{member}-tier{}", t.tier),
                uuid: format!("{:032x}", t.tier),
                tier: t.tier,
                wal_holder: t.tier == 0,
                usable_bytes: t.usable_bytes,
                free_bytes: t.free_bytes,
                payload_bytes: t.usable_bytes - t.free_bytes,
            })
            .collect())
    }

    async fn delete_vdisk(&self, member: &str, vdisk: u64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        MockFleet::id_of(&inner, member)?;
        if inner.vdisks.remove(&vdisk).is_none() {
            return Err(PoolError::NotFound(format!("no vdisk {vdisk}")));
        }
        inner.leases.remove(&vdisk);
        Ok(())
    }

    async fn export(&self, member: &str, vdisk: u64) -> Result<String> {
        let mut inner = self.inner.lock().unwrap();
        let id = MockFleet::id_of(&inner, member)?;
        if inner.fail_next_export {
            inner.fail_next_export = false;
            return Err(PoolError::Backend("the export did not come up".into()));
        }
        if !inner.vdisks.contains_key(&vdisk) {
            return Err(PoolError::NotFound(format!("no vdisk {vdisk}")));
        }
        // The attach, modelled: ours, or penless inside a window aimed
        // here, or somebody else's disk.
        match inner.leases.get(&vdisk).copied() {
            Some(lease) if lease.holder == id => {}
            Some(lease) if lease.handing_to == Some(id) => {}
            Some(lease) => {
                return Err(PoolError::Conflict(format!(
                    "node {} holds the writer lease for vdisk {vdisk}",
                    lease.holder
                )))
            }
            None => {
                inner.leases.insert(
                    vdisk,
                    Lease {
                        holder: id,
                        handing_to: None,
                    },
                );
            }
        }
        let serving = inner.exports.entry(member.to_string()).or_default();
        if !serving.contains(&vdisk) {
            serving.push(vdisk);
        }
        Ok(crate::model::device_path(vdisk))
    }

    async fn unexport(&self, member: &str, vdisk: u64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        MockFleet::id_of(&inner, member)?;
        match inner.exports.get_mut(member) {
            Some(serving) if serving.contains(&vdisk) => {
                serving.retain(|v| *v != vdisk);
                Ok(())
            }
            _ => Err(PoolError::NotFound(format!(
                "vdisk {vdisk} is not exported on {member}"
            ))),
        }
    }

    async fn exports(&self, member: &str) -> Result<Vec<(u64, String)>> {
        let inner = self.inner.lock().unwrap();
        MockFleet::id_of(&inner, member)?;
        Ok(inner
            .exports
            .get(member)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|vdisk| (vdisk, crate::model::device_path(vdisk)))
            .collect())
    }

    async fn snapshot(&self, member: &str, vdisk: u64, snapshot: u64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        MockFleet::id_of(&inner, member)?;
        let size = *inner
            .vdisks
            .get(&vdisk)
            .ok_or_else(|| PoolError::NotFound(format!("no vdisk {vdisk}")))?;
        if inner.snapshots.contains_key(&(vdisk, snapshot)) {
            return Err(PoolError::Conflict(format!(
                "vdisk {vdisk} already has a snapshot {snapshot}"
            )));
        }
        inner.snapshots.insert((vdisk, snapshot), size);
        Ok(())
    }

    async fn snapshots(&self, member: &str, vdisk: Option<u64>) -> Result<Vec<(u64, u64, u64)>> {
        let inner = self.inner.lock().unwrap();
        MockFleet::id_of(&inner, member)?;
        Ok(inner
            .snapshots
            .iter()
            .filter(|((id, _), _)| vdisk.is_none_or(|want| *id == want))
            .map(|((id, snapshot), size)| (*id, *snapshot, *size))
            .collect())
    }

    async fn delete_snapshot(&self, member: &str, vdisk: u64, snapshot: u64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        MockFleet::id_of(&inner, member)?;
        inner
            .snapshots
            .remove(&(vdisk, snapshot))
            .map(|_| ())
            .ok_or_else(|| PoolError::NotFound(format!("vdisk {vdisk} has no snapshot {snapshot}")))
    }

    async fn rollback(&self, member: &str, vdisk: u64, snapshot: u64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        MockFleet::id_of(&inner, member)?;
        // The daemon's own guard, modelled: a member refuses to roll back a
        // disk it is serving. The service refuses earlier and for the whole
        // pool, and this is what makes that belt-and-braces real in tests.
        if inner
            .exports
            .get(member)
            .is_some_and(|serving| serving.contains(&vdisk))
        {
            return Err(PoolError::Conflict(format!(
                "vdisk {vdisk} is being served on {member}"
            )));
        }
        let size = *inner.snapshots.get(&(vdisk, snapshot)).ok_or_else(|| {
            PoolError::NotFound(format!("vdisk {vdisk} has no snapshot {snapshot}"))
        })?;
        inner.vdisks.insert(vdisk, size);
        Ok(())
    }

    async fn lease(&self, member: &str, vdisk: u64) -> Result<Option<(u8, Option<u8>)>> {
        let inner = self.inner.lock().unwrap();
        MockFleet::id_of(&inner, member)?;
        Ok(inner
            .leases
            .get(&vdisk)
            .map(|lease| (lease.holder, lease.handing_to)))
    }

    async fn handover(&self, member: &str, vdisk: u64, to: u8) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let id = MockFleet::id_of(&inner, member)?;
        match inner.leases.get_mut(&vdisk) {
            Some(lease) if lease.holder == id => {
                lease.handing_to = Some(to);
                Ok(())
            }
            _ => Err(PoolError::Conflict(format!(
                "{member} does not hold the writer lease for vdisk {vdisk}"
            ))),
        }
    }

    async fn relinquish(&self, member: &str, vdisk: u64, to: u8) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let id = MockFleet::id_of(&inner, member)?;
        match inner.leases.get_mut(&vdisk) {
            Some(lease) if lease.holder == id && lease.handing_to == Some(to) => {
                lease.holder = to;
                lease.handing_to = None;
                Ok(())
            }
            _ => Err(PoolError::Conflict(format!(
                "vdisk {vdisk} has no handover open toward node {to}"
            ))),
        }
    }

    async fn abort(&self, member: &str, vdisk: u64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let id = MockFleet::id_of(&inner, member)?;
        match inner.leases.get_mut(&vdisk) {
            Some(lease) if lease.holder == id && lease.handing_to.is_some() => {
                lease.handing_to = None;
                Ok(())
            }
            _ => Err(PoolError::Conflict(format!(
                "vdisk {vdisk} has no window open"
            ))),
        }
    }

    async fn accept(&self, member: &str, vdisk: u64) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        let id = MockFleet::id_of(&inner, member)?;
        match inner.leases.get(&vdisk) {
            Some(lease) if lease.holder == id && lease.handing_to.is_none() => Ok(()),
            _ => Err(PoolError::Conflict(format!(
                "the pen for vdisk {vdisk} has not arrived at {member}"
            ))),
        }
    }
}
