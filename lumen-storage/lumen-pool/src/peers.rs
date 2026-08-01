//! Reaching a member's daemon — including the members this node cannot dial.
//!
//! ## The constraint this module exists for
//!
//! The pool daemon's control surface **binds to loopback and stays there**.
//! That is a deliberate decision recorded in two places: the shipped unit
//! passes `--control 127.0.0.1:7799`, and `lumen-pool.xml` opens the peer
//! link's ports while pointedly not opening the control port, because the
//! control surface is the local orchestration seam rather than a cluster
//! protocol — `fence-peer` is on it, and a fence verdict arriving from
//! off-box is not something this appliance accepts.
//!
//! So a control connection reaches exactly one daemon: this node's own.
//! [`SocketFleet`](crate::SocketFleet) addressing every member by socket is
//! honest only where every daemon happens to be loopback-reachable, which is
//! true of two daemons in one test process and false of two machines. On
//! real hardware the peer's control port refuses the connection.
//!
//! That matters more than it sounds, because the seam's verbs are not all
//! local. `migration_window(Open)` exports the disk on the **destination**,
//! `destroy_disk` unexports on **every** member, and an observed view asks
//! **each** member how it is doing. Without a way to reach a peer's daemon,
//! live migration cannot work between two machines and every remote member
//! reads as unreachable.
//!
//! ## The shape, which is the one the rest of the appliance already uses
//!
//! The control plane already has an authenticated channel to every other
//! member — the same one that hands over a corosync configuration. So a
//! verb for a remote member travels: this
//! node's control plane → that node's control plane → its own daemon over
//! *its* loopback. Each daemon is still only ever spoken to by the machine
//! it runs on.
//!
//! [`PoolVerb`] is a **closed enum**, not a command string. The peer route
//! that carries it is authenticated, but a member being able to ask a peer
//! to run one of a fixed set of named verbs is a different thing from being
//! able to ask it to run whatever it likes, and only one of those can be
//! reviewed.
//!
//! [`execute`] is the single definition of what each verb *does*, used by
//! whichever machine owns the daemon — this node calling its own, or a
//! control plane serving a peer's request. Two copies of that mapping would
//! be two chances for a local call and a remote one to mean different
//! things.

use std::net::SocketAddr;

use async_trait::async_trait;
use lumen_fsd::{Client, ReplState};
use serde::{Deserialize, Serialize};

use crate::error::{PoolError, Result};
use crate::fleet::PoolFleet;
use crate::state::{BrickSeen, LeaseSeen, MemberStatus, Replication, TierCapacitySeen};

/// One thing a member's daemon can be asked to do. Closed on purpose: see
/// the module note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "kebab-case")]
pub enum PoolVerb {
    Status,
    Vdisks,
    /// Every brick of the member's set, with its space in bytes.
    BrickList,
    CreateVdisk {
        vdisk: u64,
        size_bytes: u64,
        /// The device class the vdisk's data lives on. Defaulted so a verb
        /// serialized before tiers existed still deserializes — as tier 0,
        /// which is what every caller before tiers meant.
        #[serde(default)]
        tier: u8,
    },
    DeleteVdisk {
        vdisk: u64,
    },
    Export {
        vdisk: u64,
    },
    Unexport {
        vdisk: u64,
    },
    Exports,
    Snapshot {
        vdisk: u64,
        snapshot: u64,
    },
    Snapshots {
        vdisk: Option<u64>,
    },
    DeleteSnapshot {
        vdisk: u64,
        snapshot: u64,
    },
    Rollback {
        vdisk: u64,
        snapshot: u64,
    },
    Lease {
        vdisk: u64,
    },
    Handover {
        vdisk: u64,
        to: u8,
    },
    Relinquish {
        vdisk: u64,
        to: u8,
    },
    Abort {
        vdisk: u64,
    },
    Accept {
        vdisk: u64,
    },
    /// The cluster's verdict, mesh form: this member is to continue
    /// without `node`, at the era the verdict layer computed from every
    /// survivor's reported floor. The computation lives beside the quorum
    /// knowledge — here, above the daemons — never in the engine.
    FenceMember {
        node: u8,
        era: u64,
    },
    /// Open a reassignment toward `members` at `version`; the member's
    /// own maintenance drives the moves.
    Reassign {
        version: u64,
        members: Vec<u8>,
    },
    /// `(pending version, blocks still owed)` — the rebalance progress a
    /// workflow polls before it may commit anywhere.
    ReassignStatus,
    /// Commit — only after every member reports zero owed.
    CommitReassign,
}

/// What a verb answered. One variant per shape rather than a string, so a
/// caller cannot read a device path as a lease.
///
/// Adjacently tagged, not internally: internal tagging cannot serialize a
/// newtype variant holding a sequence or a string — `Vdisks` and `Device`
/// would fail at runtime, on the wire, on the first real call. The
/// round-trip test below covers **every** variant because the one it
/// originally covered was the one shape internal tagging happens to allow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", content = "value", rename_all = "kebab-case")]
// The status variant is the payload — a wire enum boxed to please a size
// lint would trade one allocation per answer for nothing a caller sees.
#[allow(clippy::large_enum_variant)]
pub enum PoolAnswer {
    Status(MemberStatus),
    Vdisks(Vec<(u64, u64)>),
    /// The member's bricks, from `brick-list`.
    Bricks(Vec<BrickSeen>),
    Exports(Vec<(u64, String)>),
    /// `(vdisk, snapshot, size_bytes)`.
    Snapshots(Vec<(u64, u64, u64)>),
    /// A device path, from `export`.
    Device(String),
    /// A lease, or nobody holding one.
    Lease(Option<LeaseSeen>),
    /// A reassignment's progress: `Some((version, owed))` while one is
    /// open, `None` when nothing is.
    Reassigning(Option<(u64, u64)>),
    /// The verb did what it said and had nothing to report.
    Done,
}

impl PoolAnswer {
    /// Read an answer as the shape the caller asked for. A mismatch is a
    /// backend failure rather than a panic: it means the far side is running
    /// something this one does not understand, which is a fact about the
    /// deployment and not a bug in the caller.
    fn wrong(&self, wanted: &str) -> PoolError {
        PoolError::Backend(format!(
            "the pool daemon answered with {self:?} where {wanted} was expected"
        ))
    }

    pub fn into_status(self) -> Result<MemberStatus> {
        match self {
            PoolAnswer::Status(status) => Ok(status),
            other => Err(other.wrong("a status")),
        }
    }

    pub fn into_vdisks(self) -> Result<Vec<(u64, u64)>> {
        match self {
            PoolAnswer::Vdisks(vdisks) => Ok(vdisks),
            other => Err(other.wrong("a vdisk listing")),
        }
    }

    pub fn into_bricks(self) -> Result<Vec<BrickSeen>> {
        match self {
            PoolAnswer::Bricks(bricks) => Ok(bricks),
            other => Err(other.wrong("a brick listing")),
        }
    }

    pub fn into_exports(self) -> Result<Vec<(u64, String)>> {
        match self {
            PoolAnswer::Exports(exports) => Ok(exports),
            other => Err(other.wrong("an export listing")),
        }
    }

    pub fn into_reassigning(self) -> Result<Option<(u64, u64)>> {
        match self {
            PoolAnswer::Reassigning(progress) => Ok(progress),
            other => Err(other.wrong("a reassignment's progress")),
        }
    }

    pub fn into_snapshots(self) -> Result<Vec<(u64, u64, u64)>> {
        match self {
            PoolAnswer::Snapshots(snapshots) => Ok(snapshots),
            other => Err(other.wrong("a snapshot listing")),
        }
    }

    pub fn into_device(self) -> Result<String> {
        match self {
            PoolAnswer::Device(device) => Ok(device),
            other => Err(other.wrong("a device path")),
        }
    }

    pub fn into_lease(self) -> Result<Option<LeaseSeen>> {
        match self {
            PoolAnswer::Lease(lease) => Ok(lease),
            other => Err(other.wrong("a lease")),
        }
    }

    pub fn into_done(self) -> Result<()> {
        match self {
            PoolAnswer::Done => Ok(()),
            other => Err(other.wrong("an acknowledgement")),
        }
    }
}

/// Run one verb against a daemon over an open control connection.
///
/// The single definition of what each verb means. A control plane serving a
/// peer's request calls exactly this against its own loopback daemon, so a
/// remote verb and a local one cannot drift apart.
pub fn execute(client: &mut Client, verb: &PoolVerb) -> std::result::Result<PoolAnswer, String> {
    Ok(match *verb {
        PoolVerb::Status => PoolAnswer::Status(status_of(client)?),
        PoolVerb::Vdisks => PoolAnswer::Vdisks(client.vdisks()?),
        PoolVerb::BrickList => PoolAnswer::Bricks(
            client
                .brick_list()?
                .into_iter()
                .map(|brick| BrickSeen {
                    path: brick.path,
                    uuid: brick.uuid,
                    tier: brick.tier,
                    wal_holder: brick.wal_holder,
                    usable_bytes: brick.usable_bytes,
                    free_bytes: brick.free_bytes,
                    payload_bytes: brick.payload_bytes,
                })
                .collect(),
        ),
        PoolVerb::CreateVdisk {
            vdisk,
            size_bytes,
            tier,
        } => {
            client.create_vdisk(vdisk, size_bytes, tier)?;
            PoolAnswer::Done
        }
        PoolVerb::DeleteVdisk { vdisk } => {
            client.delete_vdisk(vdisk)?;
            PoolAnswer::Done
        }
        PoolVerb::Export { vdisk } => {
            // The device id is the vdisk id: one allocation, and the path
            // comes out identical on every member without coordination.
            let dev_id = u32::try_from(vdisk)
                .map_err(|_| format!("vdisk {vdisk} is beyond the ublk device id range"))?;
            PoolAnswer::Device(client.export(vdisk, dev_id)?)
        }
        PoolVerb::Unexport { vdisk } => {
            client.unexport(vdisk)?;
            PoolAnswer::Done
        }
        PoolVerb::Exports => PoolAnswer::Exports(client.exports()?),
        PoolVerb::Snapshot { vdisk, snapshot } => {
            client.snapshot(vdisk, snapshot)?;
            PoolAnswer::Done
        }
        PoolVerb::Snapshots { vdisk } => PoolAnswer::Snapshots(client.snapshots(vdisk)?),
        PoolVerb::DeleteSnapshot { vdisk, snapshot } => {
            client.delete_snapshot(vdisk, snapshot)?;
            PoolAnswer::Done
        }
        PoolVerb::Rollback { vdisk, snapshot } => {
            client.rollback(vdisk, snapshot)?;
            PoolAnswer::Done
        }
        PoolVerb::Lease { vdisk } => {
            PoolAnswer::Lease(client.lease(vdisk)?.map(|lease| LeaseSeen {
                holder: lease.holder,
                era: lease.era,
                handing_to: lease.handing_to,
            }))
        }
        PoolVerb::Handover { vdisk, to } => {
            client.handover(vdisk, to)?;
            PoolAnswer::Done
        }
        PoolVerb::Relinquish { vdisk, to } => {
            client.relinquish(vdisk, to)?;
            PoolAnswer::Done
        }
        PoolVerb::Abort { vdisk } => {
            client.abort(vdisk)?;
            PoolAnswer::Done
        }
        PoolVerb::Accept { vdisk } => {
            client.accept(vdisk)?;
            PoolAnswer::Done
        }
        PoolVerb::FenceMember { node, era } => {
            client.fence_member(node, era)?;
            PoolAnswer::Done
        }
        PoolVerb::Reassign {
            version,
            ref members,
        } => {
            client.reassign(version, members)?;
            PoolAnswer::Done
        }
        PoolVerb::ReassignStatus => PoolAnswer::Reassigning(
            client
                .reassign_status()?
                .map(|(version, owed)| (version, owed as u64)),
        ),
        PoolVerb::CommitReassign => {
            client.commit_reassign()?;
            PoolAnswer::Done
        }
    })
}

/// The daemon's own account of itself, in this crate's terms.
fn status_of(client: &mut Client) -> std::result::Result<MemberStatus, String> {
    let view = client.status()?;
    Ok(MemberStatus {
        node: view.node,
        replication: match view.state {
            ReplState::Suspended => Replication::Suspended,
            ReplState::Synced => Replication::Synced,
            ReplState::Degraded => Replication::Degraded,
            ReplState::Resyncing { source } => Replication::Resyncing { source },
        },
        era: view.era,
        accepts_writes: view.accepts_writes,
        segments_free: view.segments_free,
        segments_total: view.segments_total,
        usable_bytes: view.usable_bytes,
        free_bytes: view.free_bytes,
        tiers: view
            .tiers
            .iter()
            .map(|t| TierCapacitySeen {
                tier: t.tier,
                usable_bytes: t.usable_bytes,
                free_bytes: t.free_bytes,
            })
            .collect(),
        vdisks: view.vdisks,
        leases: view
            .leases
            .into_iter()
            .map(|(vdisk, lease)| {
                (
                    vdisk,
                    LeaseSeen {
                        holder: lease.holder,
                        era: lease.era,
                        handing_to: lease.handing_to,
                    },
                )
            })
            .collect(),
        stream: view.stream,
        peers: view.peers,
        map_version: view.map_version,
        seats: view.seats,
        reassign_pending: view.reassign_pending,
        pool_uuid: view.pool_uuid,
    })
}

/// The channel to another member's control plane.
///
/// Implemented by the control plane over its existing authenticated peer
/// transport, and in memory by tests. The local node is never routed through
/// this — see [`PeeredFleet`].
#[async_trait]
pub trait PoolPeers: Send + Sync {
    /// Ask `member`'s control plane to run `verb` against its own daemon.
    async fn run(&self, member: &str, verb: PoolVerb) -> Result<PoolAnswer>;
}

/// The fleet as it works on real hardware: this node's daemon over loopback,
/// every other member through its own control plane.
///
/// The split is not an optimisation. It is the only arrangement in which
/// each daemon is spoken to by the machine it runs on, which is what lets
/// the control surface stay bound to loopback.
pub struct PeeredFleet {
    /// This node's name, and the first member reported — the order
    /// `ReplicatedDisk::members` and `here()` both depend on.
    local: String,
    /// This node's daemon control address. Loopback, and the only socket
    /// this process opens to a daemon.
    control: SocketAddr,
    members: Vec<String>,
    peers: std::sync::Arc<dyn PoolPeers>,
}

impl PeeredFleet {
    /// `members` must name every member of the pool, `local` among them.
    /// The listing is reordered so the local node comes first.
    pub fn new(
        local: &str,
        control: SocketAddr,
        members: &[String],
        peers: std::sync::Arc<dyn PoolPeers>,
    ) -> PeeredFleet {
        let mut ordered: Vec<String> = Vec::with_capacity(members.len());
        ordered.push(local.to_string());
        ordered.extend(members.iter().filter(|m| *m != local).cloned());
        PeeredFleet {
            local: local.to_string(),
            control,
            members: ordered,
            peers,
        }
    }

    /// Run a verb where the member can hear it. This is the whole point of
    /// the module in four lines.
    async fn at(&self, member: &str, verb: PoolVerb) -> Result<PoolAnswer> {
        if !self.members.iter().any(|m| m == member) {
            return Err(PoolError::NotFound(format!(
                "{member} is not a member of this pool"
            )));
        }
        if member == self.local {
            self.locally(verb).await
        } else {
            self.peers.run(member, verb).await
        }
    }

    /// Against this node's own daemon, off the reactor: the control client
    /// is blocking by design, and blocking a reactor thread is how an async
    /// service quietly stops answering unrelated requests.
    async fn locally(&self, verb: PoolVerb) -> Result<PoolAnswer> {
        let control = self.control;
        tokio::task::spawn_blocking(move || {
            let mut client = Client::connect(control).map_err(|err| {
                PoolError::Backend(format!("cannot reach this node's pool daemon: {err}"))
            })?;
            // A daemon that refuses is a `Conflict`: every refusal on that
            // surface is a state the caller can act on.
            execute(&mut client, &verb).map_err(PoolError::Conflict)
        })
        .await
        .map_err(|err| PoolError::Backend(format!("the control call did not run: {err}")))?
    }
}

#[async_trait]
impl PoolFleet for PeeredFleet {
    async fn members(&self) -> Result<Vec<String>> {
        Ok(self.members.clone())
    }

    async fn node_id(&self, member: &str) -> Result<u8> {
        Ok(self.status(member).await?.node)
    }

    async fn status(&self, member: &str) -> Result<MemberStatus> {
        self.at(member, PoolVerb::Status).await?.into_status()
    }

    async fn vdisks(&self, member: &str) -> Result<Vec<(u64, u64)>> {
        self.at(member, PoolVerb::Vdisks).await?.into_vdisks()
    }

    async fn create_vdisk(
        &self,
        member: &str,
        vdisk: u64,
        size_bytes: u64,
        tier: u8,
    ) -> Result<()> {
        self.at(
            member,
            PoolVerb::CreateVdisk {
                vdisk,
                size_bytes,
                tier,
            },
        )
        .await?
        .into_done()
    }

    async fn brick_list(&self, member: &str) -> Result<Vec<BrickSeen>> {
        self.at(member, PoolVerb::BrickList).await?.into_bricks()
    }

    async fn delete_vdisk(&self, member: &str, vdisk: u64) -> Result<()> {
        self.at(member, PoolVerb::DeleteVdisk { vdisk })
            .await?
            .into_done()
    }

    async fn export(&self, member: &str, vdisk: u64) -> Result<String> {
        self.at(member, PoolVerb::Export { vdisk })
            .await?
            .into_device()
    }

    async fn unexport(&self, member: &str, vdisk: u64) -> Result<()> {
        self.at(member, PoolVerb::Unexport { vdisk })
            .await?
            .into_done()
    }

    async fn exports(&self, member: &str) -> Result<Vec<(u64, String)>> {
        self.at(member, PoolVerb::Exports).await?.into_exports()
    }

    async fn snapshot(&self, member: &str, vdisk: u64, snapshot: u64) -> Result<()> {
        self.at(member, PoolVerb::Snapshot { vdisk, snapshot })
            .await?
            .into_done()
    }

    async fn snapshots(&self, member: &str, vdisk: Option<u64>) -> Result<Vec<(u64, u64, u64)>> {
        self.at(member, PoolVerb::Snapshots { vdisk })
            .await?
            .into_snapshots()
    }

    async fn delete_snapshot(&self, member: &str, vdisk: u64, snapshot: u64) -> Result<()> {
        self.at(member, PoolVerb::DeleteSnapshot { vdisk, snapshot })
            .await?
            .into_done()
    }

    async fn rollback(&self, member: &str, vdisk: u64, snapshot: u64) -> Result<()> {
        self.at(member, PoolVerb::Rollback { vdisk, snapshot })
            .await?
            .into_done()
    }

    async fn lease(&self, member: &str, vdisk: u64) -> Result<Option<(u8, Option<u8>)>> {
        Ok(self
            .at(member, PoolVerb::Lease { vdisk })
            .await?
            .into_lease()?
            .map(|lease| (lease.holder, lease.handing_to)))
    }

    async fn handover(&self, member: &str, vdisk: u64, to: u8) -> Result<()> {
        self.at(member, PoolVerb::Handover { vdisk, to })
            .await?
            .into_done()
    }

    async fn relinquish(&self, member: &str, vdisk: u64, to: u8) -> Result<()> {
        self.at(member, PoolVerb::Relinquish { vdisk, to })
            .await?
            .into_done()
    }

    async fn abort(&self, member: &str, vdisk: u64) -> Result<()> {
        self.at(member, PoolVerb::Abort { vdisk })
            .await?
            .into_done()
    }

    async fn accept(&self, member: &str, vdisk: u64) -> Result<()> {
        self.at(member, PoolVerb::Accept { vdisk })
            .await?
            .into_done()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what it was asked and by whom, and answers plausibly. What
    /// matters in these tests is *which member's* verbs arrive here at all:
    /// anything reaching this mock is something the local control connection
    /// could not have done.
    #[derive(Default)]
    struct RecordingPeers {
        seen: std::sync::Mutex<Vec<(String, PoolVerb)>>,
    }

    impl RecordingPeers {
        fn seen(&self) -> Vec<(String, PoolVerb)> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PoolPeers for RecordingPeers {
        async fn run(&self, member: &str, verb: PoolVerb) -> Result<PoolAnswer> {
            self.seen
                .lock()
                .unwrap()
                .push((member.to_string(), verb.clone()));
            Ok(match verb {
                PoolVerb::Export { vdisk } => PoolAnswer::Device(crate::device_path(vdisk)),
                PoolVerb::Exports => PoolAnswer::Exports(Vec::new()),
                PoolVerb::Vdisks => PoolAnswer::Vdisks(Vec::new()),
                PoolVerb::Lease { .. } => PoolAnswer::Lease(None),
                PoolVerb::Status => PoolAnswer::Status(MemberStatus {
                    node: 1,
                    replication: Replication::Synced,
                    era: 1,
                    accepts_writes: true,
                    segments_free: 10,
                    segments_total: 20,
                    usable_bytes: 0,
                    free_bytes: 0,
                    tiers: Vec::new(),
                    vdisks: Vec::new(),
                    leases: Vec::new(),
                    stream: (0, 0, 0),
                    peers: Vec::new(),
                    map_version: None,
                    seats: None,
                    reassign_pending: None,
                    pool_uuid: None,
                }),
                _ => PoolAnswer::Done,
            })
        }
    }

    /// An address nothing is listening on: any verb that tries to go local
    /// fails loudly rather than quietly succeeding, which is what makes the
    /// routing assertions below mean something.
    fn nowhere() -> SocketAddr {
        "127.0.0.1:1".parse().unwrap()
    }

    fn fleet() -> (std::sync::Arc<RecordingPeers>, PeeredFleet) {
        let peers = std::sync::Arc::new(RecordingPeers::default());
        let members = vec!["lumen01".to_string(), "lumen02".to_string()];
        let fleet = PeeredFleet::new("lumen01", nowhere(), &members, peers.clone());
        (peers, fleet)
    }

    #[tokio::test]
    async fn a_remote_members_verbs_travel_by_the_peer_channel() {
        // The bug this module exists to fix: these verbs used to be dialled
        // straight at the peer's control port, which on real hardware is
        // bound to its own loopback and refuses.
        let (peers, fleet) = fleet();
        fleet.export("lumen02", 1795).await.unwrap();
        fleet.unexport("lumen02", 1795).await.unwrap();
        fleet.accept("lumen02", 1795).await.unwrap();
        fleet.status("lumen02").await.unwrap();

        let seen = peers.seen();
        assert_eq!(
            seen.iter().map(|(m, _)| m.as_str()).collect::<Vec<_>>(),
            vec!["lumen02"; 4],
            "every remote verb should have gone through the peer channel"
        );
        assert_eq!(seen[0].1, PoolVerb::Export { vdisk: 1795 });
        assert_eq!(seen[2].1, PoolVerb::Accept { vdisk: 1795 });
    }

    #[tokio::test]
    async fn this_nodes_verbs_never_leave_the_machine() {
        // The other half: the local daemon is reached over loopback and the
        // peer channel is not involved. The control address points at
        // nothing, so a local verb fails — and failing *locally* is the
        // proof it was not routed away.
        let (peers, fleet) = fleet();
        let err = fleet.status("lumen01").await.unwrap_err();
        assert!(
            err.to_string().contains("this node's pool daemon"),
            "a local verb should have been tried locally: {err}"
        );
        assert!(
            peers.seen().is_empty(),
            "this node's own daemon must not be reached through a peer"
        );
    }

    #[tokio::test]
    async fn the_local_member_is_first_however_the_members_arrive() {
        // `here()` and `ReplicatedDisk::members` both lean on this order.
        let peers = std::sync::Arc::new(RecordingPeers::default());
        let members = vec![
            "lumen03".to_string(),
            "lumen02".to_string(),
            "lumen01".to_string(),
        ];
        let fleet = PeeredFleet::new("lumen02", nowhere(), &members, peers);
        assert_eq!(
            fleet.members().await.unwrap(),
            vec!["lumen02", "lumen03", "lumen01"]
        );
    }

    #[tokio::test]
    async fn a_member_outside_the_pool_is_named_rather_than_dialled() {
        let (peers, fleet) = fleet();
        let err = fleet.status("lumen09").await.unwrap_err();
        assert!(err.to_string().contains("lumen09"), "{err}");
        assert!(peers.seen().is_empty(), "a stranger was asked anyway");
    }

    #[tokio::test]
    async fn an_answer_of_the_wrong_shape_is_refused_rather_than_misread() {
        struct Confused;
        #[async_trait]
        impl PoolPeers for Confused {
            async fn run(&self, _member: &str, _verb: PoolVerb) -> Result<PoolAnswer> {
                // A lease where a device path was asked for.
                Ok(PoolAnswer::Lease(None))
            }
        }
        let members = vec!["lumen01".to_string(), "lumen02".to_string()];
        let fleet = PeeredFleet::new(
            "lumen01",
            nowhere(),
            &members,
            std::sync::Arc::new(Confused),
        );
        let err = fleet.export("lumen02", 1).await.unwrap_err();
        assert!(err.to_string().contains("device path"), "{err}");
    }

    #[test]
    fn a_verb_survives_the_journey_between_control_planes() {
        // The peer channel carries these as JSON, so the enum has to
        // round-trip — and it is a closed set, not a command string.
        for verb in [
            PoolVerb::Status,
            PoolVerb::Vdisks,
            PoolVerb::BrickList,
            PoolVerb::CreateVdisk {
                vdisk: 1795,
                size_bytes: 512 << 20,
                tier: 1,
            },
            PoolVerb::DeleteVdisk { vdisk: 1795 },
            PoolVerb::Export { vdisk: 1795 },
            PoolVerb::Unexport { vdisk: 1795 },
            PoolVerb::Exports,
            PoolVerb::Snapshot {
                vdisk: 1795,
                snapshot: 7,
            },
            PoolVerb::Snapshots { vdisk: None },
            PoolVerb::Snapshots { vdisk: Some(1795) },
            PoolVerb::DeleteSnapshot {
                vdisk: 1795,
                snapshot: 7,
            },
            PoolVerb::Rollback {
                vdisk: 1795,
                snapshot: 7,
            },
            PoolVerb::Lease { vdisk: 1795 },
            PoolVerb::Handover { vdisk: 1795, to: 1 },
            PoolVerb::Relinquish { vdisk: 1795, to: 1 },
            PoolVerb::Abort { vdisk: 1795 },
            PoolVerb::Accept { vdisk: 1795 },
            PoolVerb::FenceMember { node: 2, era: 5 },
            PoolVerb::Reassign {
                version: 3,
                members: vec![0, 1, 2],
            },
            PoolVerb::ReassignStatus,
            PoolVerb::CommitReassign,
        ] {
            let json = serde_json::to_string(&verb).unwrap();
            assert_eq!(serde_json::from_str::<PoolVerb>(&json).unwrap(), verb);
        }
        // EVERY answer variant, deliberately: the tagging that looked fine
        // for `Status` — a map, the one shape internal tagging allows —
        // failed at serialization for a sequence or a string, and only a
        // sweep of the whole enum catches the next variant added wrong.
        for answer in [
            PoolAnswer::Status(MemberStatus {
                node: 1,
                replication: Replication::Resyncing { source: false },
                era: 4,
                accepts_writes: false,
                segments_free: 1,
                segments_total: 30,
                usable_bytes: 5_505_024,
                free_bytes: 196_608,
                tiers: vec![
                    TierCapacitySeen {
                        tier: 0,
                        usable_bytes: 3_538_944,
                        free_bytes: 196_608,
                    },
                    TierCapacitySeen {
                        tier: 1,
                        usable_bytes: 1_966_080,
                        free_bytes: 0,
                    },
                ],
                vdisks: vec![(1795, 512 << 20)],
                leases: vec![(
                    1795,
                    LeaseSeen {
                        holder: 0,
                        era: 4,
                        handing_to: Some(1),
                    },
                )],
                stream: (9, 9, 3),
                peers: vec![(1, 9, 9, 3), (2, 4, 4, 4)],
                map_version: Some(2),
                seats: Some(171),
                reassign_pending: Some(3),
                pool_uuid: Some("ab".repeat(16)),
            }),
            PoolAnswer::Vdisks(vec![(1795, 512 << 20), (2, 8 << 20)]),
            PoolAnswer::Vdisks(Vec::new()),
            PoolAnswer::Bricks(vec![BrickSeen {
                path: "/dev/disk/by-id/nvme-eui.0001".into(),
                uuid: "c1".repeat(16),
                tier: 0,
                wal_holder: true,
                usable_bytes: 2_752_512,
                free_bytes: 2_752_512,
                payload_bytes: 0,
            }]),
            PoolAnswer::Bricks(Vec::new()),
            PoolAnswer::Exports(vec![(1795, "/dev/ublkb1795".into())]),
            PoolAnswer::Snapshots(vec![(1795, 7, 512 << 20)]),
            PoolAnswer::Device("/dev/ublkb1795".into()),
            PoolAnswer::Lease(Some(LeaseSeen {
                holder: 0,
                era: 4,
                handing_to: None,
            })),
            PoolAnswer::Lease(None),
            PoolAnswer::Reassigning(Some((3, 41))),
            PoolAnswer::Reassigning(None),
            PoolAnswer::Done,
        ] {
            let json = serde_json::to_string(&answer)
                .unwrap_or_else(|err| panic!("{answer:?} would not even serialize: {err}"));
            assert_eq!(
                serde_json::from_str::<PoolAnswer>(&json).unwrap(),
                answer,
                "{json} did not survive"
            );
        }
    }
}
