//! Observed pool state: what the members actually say, right now.
//!
//! The shapes the console renders, and the arithmetic that turns a handful
//! of per-member answers into one verdict. Deliberately **not** stored:
//! there is no pool record to consult here and nothing cached, because a
//! pool's health is a fact about this instant and a remembered one is a
//! stale one. The same rule `lumen-drbd`'s state module follows.
//!
//! Two honesty rules carried over from the DRBD side, both of which shaped
//! these types more than anything else:
//!
//! **A member that does not answer is presented, not dropped.** A pool of
//! two whose second member is unreachable is not a pool of one — it is a
//! pool of two with something wrong, and a view that silently listed one
//! member would describe a healthy single node. So [`MemberView`] has a
//! variant for silence, and it carries the reason.
//!
//! **A verdict is never better than its evidence.** With a member silent,
//! this cannot know whether the pool is fine or halved, so
//! [`PoolHealth::Unknown`] exists and is *not* folded into `Degraded`. The
//! console can say "cannot tell" where it would otherwise say something
//! confident and wrong.

use serde::{Deserialize, Serialize};

use crate::model::{device_path, DiskName};

/// A lease as a member reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseSeen {
    pub holder: u8,
    pub era: u64,
    pub handing_to: Option<u8>,
}

/// One member's daemon as it answered for itself. The engine's own words,
/// translated only as far as the console needs.
///
/// This carries the replicated listings as well as the member's own health,
/// because the daemon puts them in the same reply. Asking separately would
/// mean a round trip per vdisk to learn who holds each pen — and since the
/// fleet opens a fresh control connection per call, a pool of fifty disks
/// would cost fifty-one connections to draw one page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberStatus {
    /// The engine's node id — what a lease names.
    pub node: u8,
    pub replication: Replication,
    /// The data generation. A member at a lower era is behind and will adopt
    /// the higher one on rejoin, which is why the console shows it.
    pub era: u64,
    /// False while writes refuse: suspended without a verdict, or a resync
    /// target whose state is about to be replaced.
    pub accepts_writes: bool,
    pub segments_free: u64,
    pub segments_total: u64,
    /// Every vdisk the pool holds, as `(id, size_bytes)`. Replicated, so
    /// every answering member says the same.
    pub vdisks: Vec<(u64, u64)>,
    /// The leases this member knows about, by vdisk.
    pub leases: Vec<(u64, LeaseSeen)>,
    /// `(sent, peer_confirmed_durable, applied_from_peer)`.
    pub stream: (u64, u64, u64),
}

impl MemberStatus {
    /// The lease on one vdisk, as this member sees it.
    pub fn lease(&self, vdisk: u64) -> Option<LeaseSeen> {
        self.leases
            .iter()
            .find(|(id, _)| *id == vdisk)
            .map(|(_, lease)| *lease)
    }

    /// Free brick space as a percentage, for a meter. Zero segments total is
    /// a brick that has not been formatted, and answers `None` rather than
    /// dividing by it.
    pub fn free_percent(&self) -> Option<u8> {
        (self.segments_total > 0)
            .then(|| ((self.segments_free * 100) / self.segments_total).min(100) as u8)
    }
}

/// The replication state, as the console needs to name it.
///
/// A flattening of the engine's `ReplState`: `Resyncing` keeps its direction
/// because the two ends mean opposite things to an operator — a source is
/// still serving guests, a target is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Replication {
    /// No peer and no verdict. Reads serve, writes refuse, flushes park —
    /// the integrity stop, and the state an operator must act on.
    Suspended,
    /// Both members in lockstep; every acknowledgement is two-node durable.
    Synced,
    /// Alone under a fence verdict, at a bumped era. Writes serve, singly.
    Degraded,
    /// Reconciling with a returning peer.
    Resyncing { source: bool },
}

impl Replication {
    /// Whether this state is one an operator should be looking at.
    pub fn needs_attention(&self) -> bool {
        !matches!(self, Replication::Synced)
    }
}

/// How a member answered, or that it did not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemberView {
    Answered(MemberStatus),
    /// The daemon could not be reached, or said something unintelligible.
    /// The reason travels with it: "cannot reach lumen02's pool daemon" is
    /// an actionable sentence and "unknown" is not.
    Silent(String),
}

impl MemberView {
    pub fn status(&self) -> Option<&MemberStatus> {
        match self {
            MemberView::Answered(status) => Some(status),
            MemberView::Silent(_) => None,
        }
    }
}

/// One member of the pool, named and observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolMember {
    pub name: String,
    pub view: MemberView,
}

/// One vdisk as the pool describes it.
///
/// The name is *derived*, not looked up — see [`crate::model`]. A vdisk that
/// does not decode to a machine disk (the one a `format` mints, say) is
/// listed with `disk: None` rather than hidden: the console should show
/// everything the pool is carrying, including anything it did not put there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VdiskView {
    pub vdisk: u64,
    pub size_bytes: u64,
    /// The machine disk this backs, if the id decodes to one.
    pub disk: Option<DiskName>,
    /// Identical on every member, and present only where exported.
    pub device: String,
    /// The engine node id holding the pen, if anybody does.
    pub holder: Option<u8>,
    /// Set while a migration window is open, naming its destination.
    pub handing_to: Option<u8>,
    /// The member names actually serving this vdisk as a device.
    pub exported_on: Vec<String>,
    /// Retained map roots, newest id last. Replicated like the vdisk
    /// itself, so this is the pool's history and not one member's.
    pub snapshots: Vec<SnapshotView>,
}

/// One retained point in a vdisk's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotView {
    /// The id the snapshot was taken under. The console passes a Unix
    /// timestamp, so these sort and display as the moments they are.
    pub snapshot: u64,
    /// The vdisk's size when it was taken — a rollback restores this too.
    pub size_bytes: u64,
}

impl VdiskView {
    /// The compute domain's name for this disk, or the bare id for a vdisk
    /// that is not a machine's.
    pub fn label(&self) -> String {
        match &self.disk {
            Some(disk) => disk.to_string(),
            None => format!("vdisk {}", self.vdisk),
        }
    }

    /// Whether a migration is in flight on this disk.
    pub fn migrating(&self) -> bool {
        self.handing_to.is_some()
    }
}

/// The pool's health, as one word for a badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PoolHealth {
    /// Every member answered, every one Synced.
    Healthy,
    /// Every member answered, and at least one is not Synced — a fenced
    /// survivor, a resync in progress, or a suspension.
    Degraded,
    /// At least one member did not answer. Distinct from `Degraded` on
    /// purpose: this is missing evidence, not observed trouble.
    Unknown,
    /// There is no pool on this node — the standalone appliance.
    None,
}

/// A pool as one console page needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolState {
    pub members: Vec<PoolMember>,
    pub vdisks: Vec<VdiskView>,
    pub health: PoolHealth,
}

impl PoolState {
    /// The standalone appliance: no pool, and saying so is not an error.
    pub fn none() -> PoolState {
        PoolState {
            members: Vec::new(),
            vdisks: Vec::new(),
            health: PoolHealth::None,
        }
    }

    /// Assemble a view from what the members said. `vdisks` is the
    /// replicated listing (any member answers the same, so it is read once)
    /// joined with per-member exports and the leases whoever answered
    /// reported.
    pub fn assemble(members: Vec<PoolMember>, vdisks: Vec<VdiskView>) -> PoolState {
        let health = verdict(&members);
        PoolState {
            members,
            vdisks,
            health,
        }
    }

    /// The members that answered, by name — the ones a caller can act on.
    pub fn answering(&self) -> Vec<&str> {
        self.members
            .iter()
            .filter(|m| m.view.status().is_some())
            .map(|m| m.name.as_str())
            .collect()
    }

    /// The member name behind an engine node id, which is the translation
    /// the console needs to render a lease as something an operator
    /// recognises: "lumen02 holds the pen", not "node 1 holds the pen".
    pub fn member_named(&self, node: u8) -> Option<&str> {
        self.members
            .iter()
            .find(|m| m.view.status().is_some_and(|s| s.node == node))
            .map(|m| m.name.as_str())
    }
}

/// The pool's health from its members' answers. Missing evidence outranks
/// observed trouble: a silent member could be carrying either.
fn verdict(members: &[PoolMember]) -> PoolHealth {
    if members.is_empty() {
        return PoolHealth::None;
    }
    if members.iter().any(|m| m.view.status().is_none()) {
        return PoolHealth::Unknown;
    }
    if members
        .iter()
        .filter_map(|m| m.view.status())
        .any(|s| s.replication.needs_attention())
    {
        return PoolHealth::Degraded;
    }
    PoolHealth::Healthy
}

/// Build a vdisk view from the replicated listing plus what was observed.
/// `exports` is `(member name, vdisks it serves)`; `snapshots` is the
/// pool-wide listing as `(vdisk, snapshot, size_bytes)`.
pub fn vdisk_view(
    vdisk: u64,
    size_bytes: u64,
    lease: Option<(u8, Option<u8>)>,
    exports: &[(String, Vec<u64>)],
    snapshots: &[(u64, u64, u64)],
) -> VdiskView {
    let mut mine: Vec<SnapshotView> = snapshots
        .iter()
        .filter(|(id, _, _)| *id == vdisk)
        .map(|(_, snapshot, size_bytes)| SnapshotView {
            snapshot: *snapshot,
            size_bytes: *size_bytes,
        })
        .collect();
    mine.sort_by_key(|s| s.snapshot);
    VdiskView {
        vdisk,
        size_bytes,
        disk: DiskName::from_vdisk(vdisk),
        device: device_path(vdisk),
        holder: lease.map(|(holder, _)| holder),
        handing_to: lease.and_then(|(_, handing)| handing),
        exported_on: exports
            .iter()
            .filter(|(_, serving)| serving.contains(&vdisk))
            .map(|(member, _)| member.clone())
            .collect(),
        snapshots: mine,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synced(node: u8) -> MemberStatus {
        MemberStatus {
            node,
            replication: Replication::Synced,
            era: 1,
            accepts_writes: true,
            segments_free: 20,
            segments_total: 30,
            vdisks: Vec::new(),
            leases: Vec::new(),
            stream: (5, 5, 0),
        }
    }

    fn member(name: &str, view: MemberView) -> PoolMember {
        PoolMember {
            name: name.to_string(),
            view,
        }
    }

    #[test]
    fn a_pool_whose_members_all_answer_synced_is_healthy() {
        let state = PoolState::assemble(
            vec![
                member("lumen01", MemberView::Answered(synced(0))),
                member("lumen02", MemberView::Answered(synced(1))),
            ],
            vec![],
        );
        assert_eq!(state.health, PoolHealth::Healthy);
        assert_eq!(state.answering(), vec!["lumen01", "lumen02"]);
        // The translation the console needs: a node id is not a name.
        assert_eq!(state.member_named(1), Some("lumen02"));
        assert_eq!(state.member_named(7), None);
    }

    #[test]
    fn a_silent_member_is_listed_and_the_verdict_admits_it_cannot_tell() {
        // The failure this exists to prevent: a pool of two with one member
        // unreachable reading as a healthy pool of one.
        let state = PoolState::assemble(
            vec![
                member("lumen01", MemberView::Answered(synced(0))),
                member(
                    "lumen02",
                    MemberView::Silent("cannot reach lumen02's pool daemon".into()),
                ),
            ],
            vec![],
        );
        assert_eq!(state.members.len(), 2, "the silent member was dropped");
        assert_eq!(state.health, PoolHealth::Unknown);
        assert_eq!(state.answering(), vec!["lumen01"]);
        // And the reason survives to the console rather than becoming "unknown".
        let MemberView::Silent(why) = &state.members[1].view else {
            panic!("expected silence");
        };
        assert!(why.contains("lumen02"), "{why}");
    }

    #[test]
    fn missing_evidence_outranks_observed_trouble() {
        // A degraded member AND a silent one: the answer is Unknown, because
        // the silent one could be anything — including the worse case.
        let mut degraded = synced(0);
        degraded.replication = Replication::Degraded;
        let state = PoolState::assemble(
            vec![
                member("lumen01", MemberView::Answered(degraded)),
                member("lumen02", MemberView::Silent("no answer".into())),
            ],
            vec![],
        );
        assert_eq!(state.health, PoolHealth::Unknown);
    }

    #[test]
    fn every_state_but_synced_wants_an_operators_attention() {
        assert!(!Replication::Synced.needs_attention());
        for state in [
            Replication::Suspended,
            Replication::Degraded,
            Replication::Resyncing { source: true },
            Replication::Resyncing { source: false },
        ] {
            assert!(state.needs_attention(), "{state:?} was called fine");
        }
        let mut resyncing = synced(0);
        resyncing.replication = Replication::Resyncing { source: false };
        let state = PoolState::assemble(
            vec![member("lumen01", MemberView::Answered(resyncing))],
            vec![],
        );
        assert_eq!(state.health, PoolHealth::Degraded);
    }

    #[test]
    fn no_pool_is_a_state_not_an_error() {
        let none = PoolState::none();
        assert_eq!(none.health, PoolHealth::None);
        assert!(none.members.is_empty());
        assert_eq!(verdict(&[]), PoolHealth::None);
    }

    #[test]
    fn a_vdisk_carries_the_machine_disk_it_backs_and_where_it_is_served() {
        let exports = vec![
            ("lumen01".to_string(), vec![1795]),
            ("lumen02".to_string(), vec![]),
        ];
        // Snapshots arrive as the whole pool's listing and are filtered to
        // this disk — another vdisk's history must not appear here.
        let snapshots = vec![
            (1795, 300, 512 << 20),
            (1795, 100, 512 << 20),
            (7, 200, 1 << 30),
        ];
        let view = vdisk_view(1795, 512 << 20, Some((0, Some(1))), &exports, &snapshots);
        assert_eq!(view.disk, Some(DiskName { vmid: 7, index: 3 }));
        assert_eq!(view.label(), "vm-7-disk-3");
        assert_eq!(view.device, "/dev/ublkb1795");
        assert_eq!(view.holder, Some(0));
        assert!(view.migrating(), "an open window is a migration in flight");
        assert_eq!(view.exported_on, vec!["lumen01"]);
        assert_eq!(
            view.snapshots
                .iter()
                .map(|s| s.snapshot)
                .collect::<Vec<_>>(),
            vec![100, 300],
            "this disk's snapshots, oldest first, and nobody else's"
        );
    }

    #[test]
    fn a_vdisk_that_is_not_a_machines_is_shown_rather_than_hidden() {
        // Vdisk 1 is what `format` mints. It is not a machine disk, and the
        // console should still show that the pool is carrying it.
        let view = vdisk_view(1, 1 << 30, None, &[], &[]);
        assert_eq!(view.disk, None);
        assert_eq!(view.label(), "vdisk 1");
        assert_eq!(view.holder, None);
        assert!(!view.migrating());
        assert!(view.exported_on.is_empty());
        assert!(view.snapshots.is_empty());
    }

    #[test]
    fn free_space_reads_as_a_percentage_and_an_unformatted_brick_says_nothing() {
        assert_eq!(synced(0).free_percent(), Some(66));
        let mut empty = synced(0);
        empty.segments_total = 0;
        empty.segments_free = 0;
        assert_eq!(empty.free_percent(), None, "no dividing by an empty brick");
    }
}
