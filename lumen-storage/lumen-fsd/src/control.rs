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

use lumen_fs::{hash_block, NodeId};

use crate::daemon::Daemon;

/// How long a client waits for a reply. Generous: a `scrub` or a `gc` on a
/// large brick is legitimately slow, and a teardown waits on a kernel.
const REPLY_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// The server side.

/// Serve the control surface until the listener dies. One connection at a
/// time, and one verb at a time within it: these are administrative acts,
/// and serializing them means an operator and an orchestrator can never
/// interleave halfway through one.
pub fn serve(listener: TcpListener, daemon: &Daemon) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
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
                daemon
                    .guest()
                    .create_vdisk(vdisk, size)
                    .map(|()| format!("vdisk {vdisk} of {size} bytes"))
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
            "fence-peer" => daemon
                .fence_peer()
                .map(|()| "continuing alone under the verdict".to_string())
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
            "scrub" => daemon
                .scrub()
                .map(|report| {
                    format!(
                        "verified={} corrupt={} missing={}",
                        report.blocks_verified,
                        report.corrupt.len(),
                        report.missing.len()
                    )
                })
                .map_err(|err| err.to_string())?,
            "hash" => {
                let vdisk = number(1)?;
                vdisk_content_hash(daemon, vdisk)?
            }
            _ => {
                return Err(
                    "unknown command. node: status, fence-peer, checkpoint, gc, \
                            scrub. vdisks: vdisks, vdisk-create <id> <bytes>, \
                            vdisk-delete <id>, hash <id>. exports: export <id> <dev>, \
                            unexport <id>, exports. migration: lease <id>, \
                            handover <id> <to>, relinquish <id> <to>, \
                            accept <id>, abort <id>"
                        .into(),
                )
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

fn status_line(daemon: &Daemon) -> String {
    let s = daemon.status();
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
    format!(
        "node {} state {:?} era {} writes {} vdisks {:?} leases [{}] free {}/{} \
         stream sent={} durable={} applied={}",
        s.node,
        s.state,
        s.era,
        if s.accepts_writes { "open" } else { "held" },
        s.vdisks,
        leases.join(","),
        s.space.segments_free,
        s.space.segments_total,
        s.stream.0,
        s.stream.1,
        s.stream.2,
    )
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

    pub fn status(&mut self) -> Result<String, String> {
        self.ask("status")
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

    pub fn create_vdisk(&mut self, vdisk: u64, size_bytes: u64) -> Result<(), String> {
        self.ask(&format!("vdisk-create {vdisk} {size_bytes}"))
            .map(|_| ())
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
        let mut client = responder(vec!["ok", "ok:", "ok"]);
        assert_eq!(client.vdisks().unwrap(), vec![]);
        assert_eq!(client.exports().unwrap(), vec![]);
        assert_eq!(client.status().unwrap(), "");
    }

    #[test]
    fn the_client_refuses_to_invent_meaning() {
        let mut client = responder(vec![
            "error: no vdisk 404 exists in this pool",
            "ok: 1=not-a-number",
            "ok: era=3",
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
        assert!(client.status().is_err(), "a non-reply is an error");
    }
}
