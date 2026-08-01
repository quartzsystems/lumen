//! The control surface: one line in, one line out — and the typed client
//! that speaks it.
//!
//! This lives in the library rather than the binary because it has two
//! callers with equal standing. An operator types these verbs at a socket;
//! the orchestration layer (`lumen-pool`, and the `VmVolumes`
//! implementation over it) issues the same ones from Rust. A protocol with
//! two consumers should have one definition, and the tests should be able
//! to drive a real daemon the way the real caller will.
//!
//! Every reply is a single line beginning `ok` or `error`, and the ones a
//! program reads are `key=value` so parsing needs no grammar. That keeps
//! the surface honest in both directions: `lease 2` answering
//! `ok: holder=0 era=1 handing=1` is as readable at a shell as it is from
//! [`Client::lease`].
//!
//! Trust model, stated because it is currently thin: this binds where the
//! operator says and nowhere by default, and `fence-peer` here is the
//! break-glass until lumen-pool wires the cluster's own machinery to it.
//! It carries the same weight as `pcs stonith confirm` — saying it about a
//! peer that is not dead is how two writers happen.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use lumen_fs::{hash_block, NodeId, ReplState};

use crate::daemon::Daemon;

/// How long a client waits for a reply. Generous: a `scrub` or a `gc` on a
/// large brick is legitimately slow, and a teardown waits on a kernel.
const REPLY_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// The server side.

/// How long a connected client may sit silent before its turn ends. The
/// surface serves one connection at a time, so a half-open socket — a
/// killed client mid-exchange — used to wedge it for everyone; this was
/// found on real hardware, where exactly that happened. The deadline
/// bounds the *silence between verbs*, never a verb's own work.
const IDLE_DEADLINE: Duration = Duration::from_secs(60);

/// Serve the control surface until the listener dies. One connection at a
/// time, and one verb at a time within it: these are administrative acts,
/// and serializing them means an operator and an orchestrator can never
/// interleave halfway through one.
pub fn serve(listener: TcpListener, daemon: &Daemon) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        // A dead client's connection must end, not hold the line: reads
        // and writes both time out, and the loop moves to the next caller.
        let _ = stream.set_read_timeout(Some(IDLE_DEADLINE));
        let _ = stream.set_write_timeout(Some(IDLE_DEADLINE));
        let Ok(clone) = stream.try_clone() else {
            continue;
        };
        let mut reader = BufReader::new(clone);
        let mut line = String::new();
        while {
            line.clear();
            matches!(reader.read_line(&mut line), Ok(n) if n > 0)
        } {
            let reply = command(daemon, line.trim());
            if stream.write_all(reply.as_bytes()).is_err()
                || stream.write_all(b"\n").is_err()
                || stream.flush().is_err()
            {
                break;
            }
        }
    }
}

/// The verbs, dispatched on words so arguments are ordinary rather than a
/// special case.
pub fn command(daemon: &Daemon, line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    let number = |index: usize| -> Result<u64, String> {
        words
            .get(index)
            .ok_or_else(|| "error: missing argument".to_string())?
            .parse::<u64>()
            .map_err(|_| format!("error: not a number: {}", words[index]))
    };
    // Every arm below yields `Result<String, String>`, collapsed once at
    // the end — so a bad argument reads exactly like an engine refusal,
    // and neither can forget its prefix.
    let outcome = (|| -> Result<String, String> {
        Ok(match words.first().copied().unwrap_or("") {
            "status" => status_line(daemon),
            "vdisks" => {
                let listed: Vec<String> = daemon
                    .guest()
                    .vdisks()
                    .iter()
                    .map(|(id, size)| format!("{id}={size}"))
                    .collect();
                listed.join(" ")
            }
            "vdisk-create" => {
                let (vdisk, size) = (number(1)?, number(2)?);
                // The tier is optional and 0 when unsaid — the one tier
                // every set has, and what every caller before tiers meant.
                let tier = match words.get(3) {
                    Some(_) => number(3)? as u8,
                    None => 0,
                };
                daemon
                    .guest()
                    .create_vdisk(vdisk, size, tier)
                    .map(|()| format!("vdisk {vdisk} of {size} bytes on tier {tier}"))
                    .map_err(|err| err.to_string())?
            }
            "vdisk-delete" => {
                let vdisk = number(1)?;
                daemon
                    .guest()
                    .delete_vdisk(vdisk)
                    .map(|()| format!("vdisk {vdisk} gone"))
                    .map_err(|err| err.to_string())?
            }
            "export" => {
                let (vdisk, dev_id) = (number(1)?, number(2)?);
                daemon.export(vdisk, dev_id as u32)?
            }
            "unexport" => {
                let vdisk = number(1)?;
                daemon.unexport(vdisk).map(|()| format!("vdisk {vdisk}"))?
            }
            "exports" => {
                let listed: Vec<String> = daemon
                    .exports()
                    .iter()
                    .map(|(vdisk, device)| format!("{vdisk}={device}"))
                    .collect();
                listed.join(" ")
            }
            "snapshot" => {
                let (vdisk, snapshot) = (number(1)?, number(2)?);
                daemon
                    .guest()
                    .snapshot_vdisk(vdisk, snapshot)
                    .map(|()| format!("vdisk={vdisk} snapshot={snapshot}"))
                    .map_err(|err| err.to_string())?
            }
            "snapshots" => {
                // Optionally narrowed to one vdisk: a machine's page wants
                // its own disk's history, not the whole pool's.
                let only = match words.get(1) {
                    Some(_) => Some(number(1)?),
                    None => None,
                };
                let listed: Vec<String> = daemon
                    .guest()
                    .snapshots()
                    .into_iter()
                    .filter(|(vdisk, _, _)| only.is_none_or(|want| *vdisk == want))
                    .map(|(vdisk, snapshot, size)| format!("{vdisk}:{snapshot}:{size}"))
                    .collect();
                listed.join(" ")
            }
            "snapshot-delete" => {
                let (vdisk, snapshot) = (number(1)?, number(2)?);
                daemon
                    .guest()
                    .delete_snapshot(vdisk, snapshot)
                    .map(|()| format!("vdisk={vdisk} snapshot={snapshot}"))
                    .map_err(|err| err.to_string())?
            }
            "rollback" => {
                // Through the daemon, not the guest handle: the refusal
                // while this member is serving the disk lives there.
                let (vdisk, snapshot) = (number(1)?, number(2)?);
                daemon
                    .rollback_vdisk(vdisk, snapshot)
                    .map(|()| format!("vdisk={vdisk} snapshot={snapshot}"))?
            }
            "lease" => {
                let vdisk = number(1)?;
                // Existence first: "nobody holds it" and "there is no such
                // vdisk" are different answers, and a caller deciding
                // whether a device is one of ours needs to tell them apart.
                daemon
                    .guest()
                    .vdisk_size(vdisk)
                    .map_err(|err| err.to_string())?;
                match daemon.guest().lease(vdisk) {
                    Some(lease) => format!(
                        "holder={} era={}{}",
                        lease.holder,
                        lease.era,
                        match lease.handing_to {
                            Some(to) => format!(" handing={to}"),
                            None => String::new(),
                        }
                    ),
                    None => "unheld".into(),
                }
            }
            // The migration window, all of it driven from the source:
            // `handover` opens it, `relinquish` hands the pen over, `abort`
            // closes a window nobody used. `accept` is the destination's
            // only verb and it merely asks whether the pen has arrived —
            // nothing there can take it.
            "handover" => {
                let (vdisk, to) = (number(1)?, number(2)?);
                daemon
                    .guest()
                    .begin_handover(vdisk, to as NodeId)
                    .map(|()| format!("window open on vdisk {vdisk} toward node {to}"))
                    .map_err(|err| err.to_string())?
            }
            "relinquish" => {
                let (vdisk, to) = (number(1)?, number(2)?);
                daemon
                    .guest()
                    .relinquish(vdisk, to as NodeId)
                    .map(|()| format!("vdisk {vdisk} handed to node {to}"))
                    .map_err(|err| err.to_string())?
            }
            "accept" => {
                let vdisk = number(1)?;
                daemon
                    .guest()
                    .accept_handover(vdisk)
                    .map(|()| format!("vdisk {vdisk} is ours"))
                    .map_err(|err| err.to_string())?
            }
            "abort" => {
                let vdisk = number(1)?;
                daemon
                    .guest()
                    .abort_handover(vdisk)
                    .map(|()| format!("window on vdisk {vdisk} closed"))
                    .map_err(|err| err.to_string())?
            }
            "fence-peer" => {
                // Two forms: bare (the two-member legacy — the sole peer,
                // the local floor) and `fence-peer <node> <era>`, where
                // the verdict layer computed one era from every
                // survivor's reported floor.
                if words.len() >= 3 {
                    let (node, era) = (number(1)?, number(2)?);
                    daemon
                        .fence_member(node as u8, era)
                        .map(|()| format!("continuing without node {node} at era {era}"))
                        .map_err(|err| err.to_string())?
                } else {
                    daemon
                        .fence_peer()
                        .map(|()| "continuing alone under the verdict".to_string())
                        .map_err(|err| err.to_string())?
                }
            }
            "reassign" => {
                let version = number(1)?;
                let members: Vec<u8> = words[2..]
                    .iter()
                    .map(|word| word.parse::<u8>().map_err(|_| "bad member id".to_string()))
                    .collect::<Result<Vec<u8>, String>>()?;
                if members.len() < 2 {
                    return Err("a reassignment names at least two members".into());
                }
                daemon
                    .reassign(version, &members)
                    .map(|()| format!("reassignment to version {version} open"))
                    .map_err(|err| err.to_string())?
            }
            "reassign-status" => match daemon.reassign_status().map_err(|err| err.to_string())? {
                Some((version, owed)) => format!("pending={version} owed={owed}"),
                None => "pending=none".into(),
            },
            "reassign-commit" => daemon
                .commit_reassign()
                .map(|()| "the new map governs".to_string())
                .map_err(|err| err.to_string())?,
            "checkpoint" => daemon
                .checkpoint()
                .map(|()| String::new())
                .map_err(|err| err.to_string())?,
            "gc" => daemon
                .collect_garbage()
                .map(|stats| {
                    format!(
                        "dropped={} moved={} freed={}",
                        stats.blocks_dropped, stats.blocks_moved, stats.segments_freed
                    )
                })
                .map_err(|err| err.to_string())?,
            // Starts the pass and answers immediately — the old form held
            // the engine for the whole walk, which parked every guest
            // write behind an integrity check nobody was waiting on.
            // Progress rides `scrub-status` and the status line.
            "scrub" => daemon
                .start_scrub()
                .map(|total| format!("started total={total}"))?,
            "scrub-status" => {
                let (running, verified, total, last) = daemon.scrub_progress();
                let mut line = format!("running={running}");
                if running {
                    line.push_str(&format!(" verified={verified} total={total}"));
                }
                match last {
                    Some((at, verified, corrupt, missing)) => line.push_str(&format!(
                        " last={at} last_verified={verified} last_corrupt={corrupt} \
                         last_missing={missing}"
                    )),
                    None => line.push_str(" last=never"),
                }
                line
            }
            "hash" => {
                let vdisk = number(1)?;
                vdisk_content_hash(daemon, vdisk)?
            }
            "brick-list" => {
                let report = daemon.status().report;
                let mut entries = Vec::new();
                for tier in &report.tiers {
                    for brick in &tier.bricks {
                        // The path came off serve's own argv, where a
                        // whitespace path was refused — so these records
                        // stay parseable by splitting on spaces.
                        let path = daemon
                            .brick_path(&brick.brick_uuid)
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "?".into());
                        entries.push(format!(
                            "path={path},uuid={},tier={},wal={},usable={},free={},used={}",
                            hex(&brick.brick_uuid),
                            brick.tier,
                            u8::from(brick.wal_holder),
                            brick.space.usable_bytes,
                            brick.space.free_bytes,
                            brick.space.payload_bytes,
                        ));
                    }
                }
                entries.join(" ")
            }
            "capacity" => {
                let report = daemon.status().report;
                let entries: Vec<String> = report
                    .tiers
                    .iter()
                    .map(|tier| {
                        format!(
                            "tier{0}.usable={1} tier{0}.free={2} tier{0}.used={3}",
                            tier.tier,
                            tier.space.usable_bytes,
                            tier.space.free_bytes,
                            tier.space.payload_bytes,
                        )
                    })
                    .collect();
                entries.join(" ")
            }
            _ => {
                return Err("unknown command. node: status, capacity, brick-list, \
                            fence-peer [node era], checkpoint, gc, scrub, scrub-status. \
                            placement: reassign <version> <member>..., \
                            reassign-status, reassign-commit. vdisks: vdisks, \
                            vdisk-create <id> <bytes> [tier], \
                            vdisk-delete <id>, hash <id>. exports: export <id> <dev>, \
                            unexport <id>, exports. migration: lease <id>, \
                            handover <id> <to>, relinquish <id> <to>, \
                            accept <id>, abort <id>"
                    .into())
            }
        })
    })();
    match outcome {
        Ok(detail) if detail.is_empty() => "ok".into(),
        Ok(detail) => format!("ok: {detail}"),
        Err(why) if why.starts_with("error:") => why,
        Err(why) => format!("error: {why}"),
    }
}

/// The engine's state as one word, plus the direction a resync is going.
///
/// `ReplState` derives `Debug`, and `Resyncing { source: true }` renders
/// with spaces in it — so a `{:?}` here would put a value containing spaces
/// into a space-separated line, and nothing after `state=` could be parsed.
/// The direction rides its own key instead, absent for the other three
/// states rather than filled in with a lie.
fn state_words(state: ReplState) -> (&'static str, Option<&'static str>) {
    match state {
        ReplState::Suspended => ("suspended", None),
        ReplState::Synced => ("synced", None),
        ReplState::Degraded => ("degraded", None),
        ReplState::Resyncing { source: true } => ("resyncing", Some("source")),
        ReplState::Resyncing { source: false } => ("resyncing", Some("target")),
    }
}

fn status_line(daemon: &Daemon) -> String {
    let s = daemon.status();
    let (state, sync) = state_words(s.state);
    let vdisks: Vec<String> = s
        .vdisks
        .iter()
        .map(|(id, size)| format!("{id}:{size}"))
        .collect();
    let leases: Vec<String> = s
        .leases
        .iter()
        .map(|(vdisk, lease)| {
            format!(
                "{vdisk}:{}@{}{}",
                lease.holder,
                lease.era,
                match lease.handing_to {
                    Some(to) => format!("->{to}"),
                    None => String::new(),
                }
            )
        })
        .collect();
    let mut line = format!("node={} state={state}", s.node);
    if let Some(sync) = sync {
        line.push_str(&format!(" sync={sync}"));
    }
    // The segment counts stay for continuity; the byte figures beside
    // them are what the pool's capacity is computed from, summed and per
    // tier — one status round trip carries the whole picture.
    let mut usable = 0u64;
    let mut free_bytes = 0u64;
    let mut tier_fields = String::new();
    for tier in &s.report.tiers {
        usable += tier.space.usable_bytes;
        free_bytes += tier.space.free_bytes;
        tier_fields.push_str(&format!(
            " tier{0}.usable={1} tier{0}.free={2}",
            tier.tier, tier.space.usable_bytes, tier.space.free_bytes
        ));
    }
    line.push_str(&format!(
        " era={} era_target={} writes={} free={}/{} usable={usable} \
         free_bytes={free_bytes}{tier_fields} vdisks={} leases={} sent={} durable={} applied={}",
        s.era,
        s.era_target,
        if s.accepts_writes { "open" } else { "held" },
        s.space.segments_free,
        s.space.segments_total,
        vdisks.join(","),
        leases.join(","),
        s.stream.0,
        s.stream.1,
        s.stream.2,
    ));
    // The mesh's honest stream picture, one dot-key trio per peer, plus
    // the map facts a placement-aware caller reads.
    for (peer, sent, durable, applied) in &s.peers {
        line.push_str(&format!(
            " peer{peer}.sent={sent} peer{peer}.durable={durable} peer{peer}.applied={applied}"
        ));
    }
    if let Some(version) = s.map_version {
        line.push_str(&format!(" map={version}"));
    }
    if let Some(seats) = s.seats {
        line.push_str(&format!(" seats={seats}"));
    }
    line.push_str(&format!(" pool={}", hex(&s.pool_uuid)));
    if let Some(version) = s.reassign_pending {
        line.push_str(&format!(" reassign={version}"));
    }
    if let Some((verified, total)) = s.scrub {
        line.push_str(&format!(" scrub={verified}/{total}"));
    }
    line
}

fn hex(uuid: &[u8; 16]) -> String {
    uuid.iter().map(|b| format!("{b:02x}")).collect()
}

/// A deterministic content hash of a whole vdisk (a hash of per-chunk
/// hashes), for comparing two members' answers from the outside — the
/// diagnostic that localizes "who lost it" when a byte goes missing.
fn vdisk_content_hash(daemon: &Daemon, vdisk: u64) -> Result<String, String> {
    const CHUNK: u64 = 16 << 20;
    let guest = daemon.guest();
    let size = guest.vdisk_size(vdisk).map_err(|err| err.to_string())?;
    let mut digests = Vec::new();
    let mut offset = 0;
    while offset < size {
        let len = CHUNK.min(size - offset);
        let chunk = guest
            .read(vdisk, offset, len)
            .map_err(|err| err.to_string())?;
        digests.extend_from_slice(hash_block(&chunk).as_bytes());
        offset += len;
    }
    Ok(hash_block(&digests)
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

// ---------------------------------------------------------------------------
// The client side: what an orchestrator calls.

/// What a lease looks like to a caller that is not the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseView {
    pub holder: NodeId,
    pub era: u64,
    pub handing_to: Option<NodeId>,
}

/// One member's whole picture in one round trip: who it is, what the
/// replication is doing, how much brick is left, and every vdisk with its
/// pen. The console renders a pool from one of these per member, which is
/// why it carries the listings rather than making the caller ask again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusView {
    pub node: NodeId,
    pub state: ReplState,
    pub era: u64,
    /// False while writes refuse — suspended without a verdict, or a resync
    /// target about to be replaced.
    pub accepts_writes: bool,
    pub segments_free: u64,
    pub segments_total: u64,
    /// This member's space in bytes, labelled usable — every segment
    /// outside the collection reserve at full-block density, summed across
    /// its bricks. Dedupe only makes it conservative.
    pub usable_bytes: u64,
    pub free_bytes: u64,
    /// The same figures per tier, ascending — what a pool-wide capacity
    /// takes its per-tier minimum over.
    pub tiers: Vec<TierBytes>,
    pub vdisks: Vec<(u64, u64)>,
    pub leases: Vec<(u64, LeaseView)>,
    /// `(sent, peer_confirmed_durable, applied_from_peer)` — the counters
    /// that convicted the elided-flush bug, so they are worth carrying.
    pub stream: (u64, u64, u64),
    /// The same, per peer — `(peer, sent, durable, applied)`, the mesh's
    /// honest form. Empty from a pre-mesh daemon.
    pub peers: Vec<(u8, u64, u64, u64)>,
    /// The floor a fence-verdict era must clear from this member's
    /// vantage — what the verdict layer maxes over survivors. Absent from
    /// a pre-mesh daemon.
    pub era_target: Option<u64>,
    /// The committed slice map's version; `None` when unplaced.
    pub map_version: Option<u64>,
    /// How many of the 256 slices this member homes; `None` when unplaced.
    pub seats: Option<u64>,
    /// The pool identity, lowercase hex — what a grow workflow formats a
    /// newcomer's bricks with. Absent from a pre-mesh daemon.
    pub pool_uuid: Option<String>,
    /// The version a reassignment is moving to, while one is open.
    pub reassign_pending: Option<u64>,
    /// A background scrub in flight: `(records verified, records total)`.
    pub scrub: Option<(u64, u64)>,
}

/// One tier's byte figures as a member reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierBytes {
    pub tier: u8,
    pub usable_bytes: u64,
    pub free_bytes: u64,
}

/// One brick as `brick-list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrickView {
    /// The path the daemon opened it from — the device name an operator
    /// recognizes.
    pub path: String,
    /// The brick's own identity, lowercase hex.
    pub uuid: String,
    pub tier: u8,
    pub wal_holder: bool,
    pub usable_bytes: u64,
    pub free_bytes: u64,
    pub payload_bytes: u64,
}

/// Parse `1:0@3->1,2:1@3` — the lease listing inside a status line.
fn parse_leases(field: &str) -> Option<Vec<(u64, LeaseView)>> {
    if field.is_empty() {
        return Some(Vec::new());
    }
    field
        .split(',')
        .map(|entry| {
            let (vdisk, rest) = entry.split_once(':')?;
            let (rest, handing_to) = match rest.split_once("->") {
                Some((rest, to)) => (rest, Some(to.parse().ok()?)),
                None => (rest, None),
            };
            let (holder, era) = rest.split_once('@')?;
            Some((
                vdisk.parse().ok()?,
                LeaseView {
                    holder: holder.parse().ok()?,
                    era: era.parse().ok()?,
                    handing_to,
                },
            ))
        })
        .collect()
}

/// Parse `1:1073741824,2:536870912` — a `(id, bytes)` listing.
fn parse_sized(field: &str) -> Option<Vec<(u64, u64)>> {
    if field.is_empty() {
        return Some(Vec::new());
    }
    field
        .split(',')
        .map(|entry| {
            let (id, size) = entry.split_once(':')?;
            Some((id.parse().ok()?, size.parse().ok()?))
        })
        .collect()
}

/// Parse a whole status reply. Keys are read by name, not position, so a
/// later field can be added without breaking a caller — and anything
/// missing or malformed is refused rather than defaulted, because a status
/// that reads `era 0, nothing exported` when it could not be parsed is how
/// an orchestrator concludes a healthy pool is empty.
fn parse_status(reply: &str) -> Option<StatusView> {
    let mut node = None;
    let mut state = None;
    let mut sync = None;
    let mut era = None;
    let mut writes = None;
    let mut free = None;
    let mut total = None;
    let mut usable_bytes = None;
    let mut free_bytes = None;
    let mut tiers: Vec<TierBytes> = Vec::new();
    let mut vdisks = None;
    let mut leases = None;
    let (mut sent, mut durable, mut applied) = (None, None, None);
    let mut peers: Vec<(u8, u64, u64, u64)> = Vec::new();
    let mut era_target = None;
    let mut map_version = None;
    let mut seats = None;
    let mut pool_uuid = None;
    let mut reassign_pending = None;
    let mut scrub = None;
    for token in reply.split_whitespace() {
        let (key, value) = token.split_once('=')?;
        // Per-peer stream figures ride keys of the form peerN.sent /
        // peerN.durable / peerN.applied — the peer id is data, so it
        // lives in the key the same way a tier number does.
        if let Some(rest) = key.strip_prefix("peer") {
            if let Some((peer, field)) = rest.split_once('.') {
                let peer: u8 = peer.parse().ok()?;
                let value: u64 = value.parse().ok()?;
                let entry = match peers.iter_mut().find(|p| p.0 == peer) {
                    Some(entry) => entry,
                    None => {
                        peers.push((peer, 0, 0, 0));
                        peers.last_mut().unwrap()
                    }
                };
                match field {
                    "sent" => entry.1 = value,
                    "durable" => entry.2 = value,
                    "applied" => entry.3 = value,
                    _ => {}
                }
                continue;
            }
        }
        // Per-tier byte figures ride keys of the form tierN.usable /
        // tierN.free — a tier number is data, so it lives in the key the
        // same way it lives in the report.
        if let Some(rest) = key.strip_prefix("tier") {
            if let Some((tier, field)) = rest.split_once('.') {
                let tier: u8 = tier.parse().ok()?;
                let value: u64 = value.parse().ok()?;
                let entry = match tiers.iter_mut().find(|t| t.tier == tier) {
                    Some(entry) => entry,
                    None => {
                        tiers.push(TierBytes {
                            tier,
                            usable_bytes: 0,
                            free_bytes: 0,
                        });
                        tiers.last_mut().unwrap()
                    }
                };
                match field {
                    "usable" => entry.usable_bytes = value,
                    "free" => entry.free_bytes = value,
                    _ => {}
                }
                continue;
            }
        }
        match key {
            "node" => node = Some(value.parse().ok()?),
            "state" => state = Some(value),
            "sync" => sync = Some(value),
            "era" => era = Some(value.parse().ok()?),
            "writes" => {
                writes = Some(match value {
                    "open" => true,
                    "held" => false,
                    _ => return None,
                })
            }
            "free" => {
                let (f, t) = value.split_once('/')?;
                free = Some(f.parse().ok()?);
                total = Some(t.parse().ok()?);
            }
            "usable" => usable_bytes = Some(value.parse().ok()?),
            "free_bytes" => free_bytes = Some(value.parse().ok()?),
            "vdisks" => vdisks = Some(parse_sized(value)?),
            "leases" => leases = Some(parse_leases(value)?),
            "sent" => sent = Some(value.parse().ok()?),
            "durable" => durable = Some(value.parse().ok()?),
            "applied" => applied = Some(value.parse().ok()?),
            "era_target" => era_target = Some(value.parse().ok()?),
            "map" => map_version = Some(value.parse().ok()?),
            "seats" => seats = Some(value.parse().ok()?),
            "pool" => pool_uuid = Some(value.to_string()),
            "reassign" => reassign_pending = Some(value.parse().ok()?),
            "scrub" => {
                let (done, of) = value.split_once('/')?;
                scrub = Some((done.parse().ok()?, of.parse().ok()?));
            }
            // An unknown key is a newer daemon, not a broken one.
            _ => {}
        }
    }
    peers.sort_by_key(|p| p.0);
    tiers.sort_by_key(|t| t.tier);
    let state = match (state?, sync) {
        ("suspended", _) => ReplState::Suspended,
        ("synced", _) => ReplState::Synced,
        ("degraded", _) => ReplState::Degraded,
        ("resyncing", Some("source")) => ReplState::Resyncing { source: true },
        ("resyncing", Some("target")) => ReplState::Resyncing { source: false },
        // Resyncing without a direction says less than nothing: guessing
        // would make a target that refuses writes look like a serving source.
        _ => return None,
    };
    Some(StatusView {
        node: node?,
        state,
        era: era?,
        accepts_writes: writes?,
        segments_free: free?,
        segments_total: total?,
        usable_bytes: usable_bytes?,
        free_bytes: free_bytes?,
        tiers,
        vdisks: vdisks?,
        leases: leases?,
        stream: (sent?, durable?, applied?),
        peers,
        era_target,
        map_version,
        seats,
        pool_uuid,
        reassign_pending,
        scrub,
    })
}

/// A connection to one daemon's control surface.
pub struct Client {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Client {
    pub fn connect(addr: SocketAddr) -> Result<Client, String> {
        let stream =
            TcpStream::connect(addr).map_err(|err| format!("cannot reach {addr}: {err}"))?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT)).ok();
        stream.set_nodelay(true).ok();
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|err| format!("cannot split the control socket: {err}"))?,
        );
        Ok(Client { stream, reader })
    }

    /// Issue one verb. `Ok` carries whatever followed `ok:`, empty for a
    /// bare `ok`; `Err` carries the daemon's reason with its prefix
    /// stripped, so callers can log it as their own.
    pub fn ask(&mut self, verb: &str) -> Result<String, String> {
        self.stream
            .write_all(verb.as_bytes())
            .and_then(|()| self.stream.write_all(b"\n"))
            .and_then(|()| self.stream.flush())
            .map_err(|err| format!("control write failed: {err}"))?;
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .map_err(|err| format!("control read failed: {err}"))?;
        if read == 0 {
            return Err("the daemon closed the control connection".into());
        }
        let line = line.trim();
        // A bare `ok` is what the daemon says when there is nothing to
        // report; `ok:` with an empty tail means the same thing and is
        // accepted rather than argued with.
        if let Some(detail) = line.strip_prefix("ok:") {
            Ok(detail.trim().to_string())
        } else if line == "ok" {
            Ok(String::new())
        } else if let Some(why) = line.strip_prefix("error:") {
            Err(why.trim().to_string())
        } else {
            Err(format!("unintelligible reply: {line}"))
        }
    }

    /// The member's whole picture, typed. `ask("status")` still returns the
    /// raw line for an operator or a log.
    pub fn status(&mut self) -> Result<StatusView, String> {
        let reply = self.ask("status")?;
        parse_status(&reply).ok_or(format!("unintelligible status: {reply}"))
    }

    pub fn vdisks(&mut self) -> Result<Vec<(u64, u64)>, String> {
        let reply = self.ask("vdisks")?;
        reply
            .split_whitespace()
            .map(|pair| {
                let (id, size) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("malformed vdisk entry: {pair}"))?;
                Ok((
                    id.parse::<u64>()
                        .map_err(|_| format!("bad vdisk id: {id}"))?,
                    size.parse::<u64>()
                        .map_err(|_| format!("bad vdisk size: {size}"))?,
                ))
            })
            .collect()
    }

    pub fn create_vdisk(&mut self, vdisk: u64, size_bytes: u64, tier: u8) -> Result<(), String> {
        self.ask(&format!("vdisk-create {vdisk} {size_bytes} {tier}"))
            .map(|_| ())
    }

    /// Every brick of this member's set: which disk, which tier, and its
    /// space in bytes.
    pub fn brick_list(&mut self) -> Result<Vec<BrickView>, String> {
        let reply = self.ask("brick-list")?;
        reply
            .split_whitespace()
            .map(|record| {
                let mut view = BrickView {
                    path: String::new(),
                    uuid: String::new(),
                    tier: 0,
                    wal_holder: false,
                    usable_bytes: 0,
                    free_bytes: 0,
                    payload_bytes: 0,
                };
                for field in record.split(',') {
                    let (key, value) = field
                        .split_once('=')
                        .ok_or_else(|| format!("malformed brick field: {field}"))?;
                    let parse = |v: &str| {
                        v.parse::<u64>()
                            .map_err(|_| format!("bad brick number: {v}"))
                    };
                    match key {
                        "path" => view.path = value.to_string(),
                        "uuid" => view.uuid = value.to_string(),
                        "tier" => view.tier = parse(value)? as u8,
                        "wal" => view.wal_holder = parse(value)? != 0,
                        "usable" => view.usable_bytes = parse(value)?,
                        "free" => view.free_bytes = parse(value)?,
                        "used" => view.payload_bytes = parse(value)?,
                        _ => {}
                    }
                }
                if view.path.is_empty() || view.uuid.is_empty() {
                    return Err(format!("brick record missing identity: {record}"));
                }
                Ok(view)
            })
            .collect()
    }

    pub fn delete_vdisk(&mut self, vdisk: u64) -> Result<(), String> {
        self.ask(&format!("vdisk-delete {vdisk}")).map(|_| ())
    }

    /// Export a vdisk, returning the guest device path — the stable,
    /// identical-on-every-member path the compute domain records.
    pub fn export(&mut self, vdisk: u64, dev_id: u32) -> Result<String, String> {
        self.ask(&format!("export {vdisk} {dev_id}"))
    }

    pub fn unexport(&mut self, vdisk: u64) -> Result<(), String> {
        self.ask(&format!("unexport {vdisk}")).map(|_| ())
    }

    pub fn exports(&mut self) -> Result<Vec<(u64, String)>, String> {
        let reply = self.ask("exports")?;
        reply
            .split_whitespace()
            .map(|pair| {
                let (id, device) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("malformed export entry: {pair}"))?;
                Ok((
                    id.parse::<u64>()
                        .map_err(|_| format!("bad vdisk id: {id}"))?,
                    device.to_string(),
                ))
            })
            .collect()
    }

    /// Who may write a vdisk. `None` means no member has ever claimed it.
    pub fn lease(&mut self, vdisk: u64) -> Result<Option<LeaseView>, String> {
        let reply = self.ask(&format!("lease {vdisk}"))?;
        if reply == "unheld" {
            return Ok(None);
        }
        let mut view = LeaseView {
            holder: 0,
            era: 0,
            handing_to: None,
        };
        let mut saw_holder = false;
        for field in reply.split_whitespace() {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                "holder" => {
                    view.holder = value.parse().map_err(|_| format!("bad holder: {value}"))?;
                    saw_holder = true;
                }
                "era" => view.era = value.parse().map_err(|_| format!("bad era: {value}"))?,
                "handing" => {
                    view.handing_to =
                        Some(value.parse().map_err(|_| format!("bad handing: {value}"))?)
                }
                _ => {}
            }
        }
        if !saw_holder {
            return Err(format!("a lease with no holder: {reply}"));
        }
        Ok(Some(view))
    }

    /// Open the migration window toward `to`. Runs on the source, whose
    /// guest keeps running and keeps writing.
    pub fn snapshot(&mut self, vdisk: u64, snapshot: u64) -> Result<(), String> {
        self.ask(&format!("snapshot {vdisk} {snapshot}"))
            .map(|_| ())
    }

    /// Every snapshot, or only one vdisk's, as `(vdisk, snapshot, bytes)`.
    pub fn snapshots(&mut self, vdisk: Option<u64>) -> Result<Vec<(u64, u64, u64)>, String> {
        let reply = match vdisk {
            Some(vdisk) => self.ask(&format!("snapshots {vdisk}"))?,
            None => self.ask("snapshots")?,
        };
        if reply.is_empty() {
            return Ok(Vec::new());
        }
        reply
            .split_whitespace()
            .map(|entry| {
                let mut parts = entry.split(':');
                let parsed = (|| {
                    Some((
                        parts.next()?.parse().ok()?,
                        parts.next()?.parse().ok()?,
                        parts.next()?.parse().ok()?,
                    ))
                })();
                parsed.ok_or_else(|| format!("unintelligible snapshot: {entry}"))
            })
            .collect()
    }

    pub fn delete_snapshot(&mut self, vdisk: u64, snapshot: u64) -> Result<(), String> {
        self.ask(&format!("snapshot-delete {vdisk} {snapshot}"))
            .map(|_| ())
    }

    pub fn rollback(&mut self, vdisk: u64, snapshot: u64) -> Result<(), String> {
        self.ask(&format!("rollback {vdisk} {snapshot}"))
            .map(|_| ())
    }

    pub fn handover(&mut self, vdisk: u64, to: NodeId) -> Result<(), String> {
        self.ask(&format!("handover {vdisk} {to}")).map(|_| ())
    }

    /// Hand the pen to `to`. Runs on the source, once its guest has
    /// stopped writing — this is the instant the writer changes.
    pub fn relinquish(&mut self, vdisk: u64, to: NodeId) -> Result<(), String> {
        self.ask(&format!("relinquish {vdisk} {to}")).map(|_| ())
    }

    /// Ask whether the pen has arrived. Runs on the destination, and takes
    /// nothing: it refuses until the source has handed over, so a caller
    /// polls this rather than commanding it.
    pub fn accept(&mut self, vdisk: u64) -> Result<(), String> {
        self.ask(&format!("accept {vdisk}")).map(|_| ())
    }

    /// Close a window the migration did not use. Runs on the source.
    pub fn abort(&mut self, vdisk: u64) -> Result<(), String> {
        self.ask(&format!("abort {vdisk}")).map(|_| ())
    }

    pub fn fence_peer(&mut self) -> Result<(), String> {
        self.ask("fence-peer").map(|_| ())
    }

    /// The mesh verdict: the member and the era the verdict layer agreed
    /// from every survivor's reported `era_target`.
    pub fn fence_member(&mut self, node: u8, era: u64) -> Result<(), String> {
        self.ask(&format!("fence-peer {node} {era}")).map(|_| ())
    }

    pub fn reassign(&mut self, version: u64, members: &[u8]) -> Result<(), String> {
        let members: Vec<String> = members.iter().map(u8::to_string).collect();
        self.ask(&format!("reassign {version} {}", members.join(" ")))
            .map(|_| ())
    }

    /// `(pending version, blocks still owed)`; `None` when nothing is
    /// open. Asking also nudges the moves along.
    pub fn reassign_status(&mut self) -> Result<Option<(u64, usize)>, String> {
        let reply = self.ask("reassign-status")?;
        let mut pending = None;
        let mut owed = 0usize;
        for token in reply.split_whitespace() {
            match token.split_once('=') {
                Some(("pending", "none")) => return Ok(None),
                Some(("pending", value)) => {
                    pending = Some(
                        value
                            .parse()
                            .map_err(|_| format!("bad pending version: {value}"))?,
                    )
                }
                Some(("owed", value)) => {
                    owed = value.parse().map_err(|_| format!("bad owed: {value}"))?
                }
                _ => {}
            }
        }
        Ok(pending.map(|version| (version, owed)))
    }

    pub fn commit_reassign(&mut self) -> Result<(), String> {
        self.ask("reassign-commit").map(|_| ())
    }

    pub fn checkpoint(&mut self) -> Result<(), String> {
        self.ask("checkpoint").map(|_| ())
    }

    pub fn scrub(&mut self) -> Result<String, String> {
        self.ask("scrub")
    }

    pub fn content_hash(&mut self, vdisk: u64) -> Result<String, String> {
        self.ask(&format!("hash {vdisk}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon's replies without a daemon: one connection, one canned
    /// answer per line. Lets the client's parsing be tested for the shapes
    /// a real daemon produces *and* the shapes it never should.
    fn responder(replies: Vec<&'static str>) -> Client {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut out = stream.try_clone().unwrap();
            let reader = BufReader::new(stream);
            for (line, reply) in reader.lines().zip(replies) {
                if line.is_err() {
                    return;
                }
                if out
                    .write_all(reply.as_bytes())
                    .and_then(|()| out.write_all(b"\n"))
                    .is_err()
                {
                    return;
                }
            }
        });
        Client::connect(addr).unwrap()
    }

    #[test]
    fn the_client_reads_the_shapes_a_daemon_writes() {
        let mut client = responder(vec![
            "ok: 1=1073741824 2=268435456",
            "ok: holder=1 era=3 handing=0",
            "ok: holder=1 era=3",
            "ok: unheld",
            "ok: 2=/dev/ublkb7",
            "ok: /dev/ublkb4",
            "ok",
        ]);
        assert_eq!(
            client.vdisks().unwrap(),
            vec![(1, 1073741824), (2, 268435456)]
        );
        assert_eq!(
            client.lease(2).unwrap(),
            Some(LeaseView {
                holder: 1,
                era: 3,
                handing_to: Some(0)
            })
        );
        assert_eq!(
            client.lease(2).unwrap(),
            Some(LeaseView {
                holder: 1,
                era: 3,
                handing_to: None
            })
        );
        assert_eq!(client.lease(2).unwrap(), None);
        assert_eq!(
            client.exports().unwrap(),
            vec![(2, "/dev/ublkb7".to_string())]
        );
        assert_eq!(client.export(3, 4).unwrap(), "/dev/ublkb4");
        assert_eq!(client.checkpoint(), Ok(()));
    }

    #[test]
    fn an_empty_listing_is_empty_not_an_error() {
        // A bare `ok` is what this daemon actually says when a listing is
        // empty — the first shape below — and a client that read it as a
        // parse failure would report a healthy pool as broken. `ok:` with
        // an empty tail means the same and is tolerated.
        let mut client = responder(vec!["ok", "ok:"]);
        assert_eq!(client.vdisks().unwrap(), vec![]);
        assert_eq!(client.exports().unwrap(), vec![]);
    }

    /// A status line is the one reply a program reads in full, so it is
    /// parsed by key rather than position — and an empty pool answers with
    /// empty listings, not with missing ones.
    #[test]
    fn a_status_line_reads_back_as_the_daemon_meant_it() {
        let mut client = responder(vec![
            "ok: node=1 state=synced era=3 writes=open free=29/30 \
             usable=5111808 free_bytes=4915200 \
             tier0.usable=3145728 tier0.free=2949120 \
             tier1.usable=1966080 tier1.free=1966080 \
             vdisks=1:1073741824,2:536870912 leases=1:0@3,2:1@3->0 \
             sent=5 durable=5 applied=2",
            "ok: node=0 state=resyncing sync=target era=2 writes=held free=1/30 \
             usable=100 free_bytes=0 vdisks= leases= sent=0 durable=0 applied=0",
        ]);
        let status = client.status().unwrap();
        assert_eq!(status.node, 1);
        assert_eq!(status.state, ReplState::Synced);
        assert_eq!(status.era, 3);
        assert!(status.accepts_writes);
        assert_eq!((status.segments_free, status.segments_total), (29, 30));
        assert_eq!((status.usable_bytes, status.free_bytes), (5111808, 4915200));
        assert_eq!(
            status.tiers,
            vec![
                TierBytes {
                    tier: 0,
                    usable_bytes: 3145728,
                    free_bytes: 2949120
                },
                TierBytes {
                    tier: 1,
                    usable_bytes: 1966080,
                    free_bytes: 1966080
                },
            ]
        );
        assert_eq!(status.vdisks, vec![(1, 1073741824), (2, 536870912)]);
        assert_eq!(
            status.leases,
            vec![
                (
                    1,
                    LeaseView {
                        holder: 0,
                        era: 3,
                        handing_to: None
                    }
                ),
                (
                    2,
                    LeaseView {
                        holder: 1,
                        era: 3,
                        handing_to: Some(0)
                    }
                ),
            ]
        );
        assert_eq!(status.stream, (5, 5, 2));

        // A resync target: the direction rides its own key, and an empty
        // pool's listings are empty rather than absent.
        let target = client.status().unwrap();
        assert_eq!(target.state, ReplState::Resyncing { source: false });
        assert!(!target.accepts_writes, "a resync target refuses writes");
        assert_eq!(target.vdisks, vec![]);
        assert_eq!(target.leases, vec![]);
    }

    /// The formatter and the parser are one contract; a round trip through
    /// both is what keeps them from drifting apart in separate edits.
    #[test]
    fn every_state_survives_the_round_trip_including_the_one_with_a_direction() {
        for state in [
            ReplState::Suspended,
            ReplState::Synced,
            ReplState::Degraded,
            ReplState::Resyncing { source: true },
            ReplState::Resyncing { source: false },
        ] {
            let (word, sync) = state_words(state);
            let mut line = format!("node=0 state={word}");
            if let Some(sync) = sync {
                line.push_str(&format!(" sync={sync}"));
            }
            line.push_str(
                " era=1 writes=open free=1/2 usable=10 free_bytes=5 vdisks= leases= \
                 sent=0 durable=0 applied=0",
            );
            assert_eq!(
                parse_status(&line).map(|s| s.state),
                Some(state),
                "{line} did not survive"
            );
        }
    }

    #[test]
    fn a_status_line_that_says_nothing_useful_is_not_guessed_at() {
        // Defaulting a missing field is how an orchestrator concludes that a
        // healthy pool is an empty one, so every one of these is refused.
        for nonsense in [
            "",
            "ok",
            "node=0",
            // A resync with no direction: a target that refuses writes must
            // never read as a source that serves them.
            "node=0 state=resyncing era=1 writes=open free=1/2 vdisks= leases= \
             sent=0 durable=0 applied=0",
            // The old prose format, which is what this replaced.
            "node 0 state Synced era 1 writes open vdisks [] leases [] free 1/2",
            // Malformed pieces of an otherwise good line.
            "node=x state=synced era=1 writes=open free=1/2 vdisks= leases= \
             sent=0 durable=0 applied=0",
            "node=0 state=synced era=1 writes=maybe free=1/2 vdisks= leases= \
             sent=0 durable=0 applied=0",
            "node=0 state=synced era=1 writes=open free=1 vdisks= leases= \
             sent=0 durable=0 applied=0",
            "node=0 state=synced era=1 writes=open free=1/2 vdisks=1 leases= \
             sent=0 durable=0 applied=0",
            "node=0 state=synced era=1 writes=open free=1/2 vdisks= leases=1:0 \
             sent=0 durable=0 applied=0",
        ] {
            assert_eq!(parse_status(nonsense), None, "{nonsense} was parsed anyway");
        }
    }

    #[test]
    fn the_client_refuses_to_invent_meaning() {
        let mut client = responder(vec![
            "error: no vdisk 404 exists in this pool",
            "ok: 1=not-a-number",
            "ok: era=3",
            "ok: node=0 state=synced",
            "surprise",
        ]);
        assert_eq!(
            client.lease(404).unwrap_err(),
            "no vdisk 404 exists in this pool",
            "the daemon's reason should arrive intact"
        );
        assert!(client.vdisks().is_err());
        assert!(
            client.lease(1).is_err(),
            "a lease with no holder is not a lease"
        );
        assert!(
            client.status().is_err(),
            "half a status is not a status — it must not be defaulted out"
        );
        assert!(client.status().is_err(), "a non-reply is an error");
    }
}
