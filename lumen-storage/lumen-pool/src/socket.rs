//! The fleet over real sockets: one control connection per call, to each
//! member's daemon.
//!
//! Two choices worth stating, because both look like shortcuts and are not.
//!
//! **A connection per call.** These are administrative verbs — create a
//! disk, open a window, hand a pen over — not a data path, so the cost of a
//! TCP handshake is irrelevant beside the cost of holding connections that
//! can go stale while nobody is looking. A pooled connection would need
//! liveness checks, reconnection, and a story for a half-open socket
//! discovered mid-verb; a fresh connection has none of those and fails in
//! exactly one place.
//!
//! **`spawn_blocking` around every call.** [`Client`] is blocking by
//! design, because the control surface is a line protocol an operator can
//! type. Blocking a reactor thread is how an async service quietly stops
//! answering unrelated requests, so each call goes to the blocking pool
//! instead.
//!
//! The node ids the leases speak are *asked for*, not configured: a
//! member's daemon reports its own id in `status`, and duplicating that in
//! configuration is how the two come to disagree.

use std::net::SocketAddr;

use async_trait::async_trait;
use lumen_fsd::{Client, ReplState};

use crate::error::{PoolError, Result};
use crate::fleet::PoolFleet;
use crate::state::{LeaseSeen, MemberStatus, Replication};

/// Every member's control address, this node first.
pub struct SocketFleet {
    members: Vec<(String, SocketAddr)>,
}

impl SocketFleet {
    /// `members` is `(node name, control address)`, **this node first** —
    /// the order the seam reports as `ReplicatedDisk::members` and the one
    /// `here()` depends on.
    pub fn new(members: Vec<(String, SocketAddr)>) -> SocketFleet {
        SocketFleet { members }
    }

    fn addr(&self, member: &str) -> Result<SocketAddr> {
        self.members
            .iter()
            .find(|(name, _)| name == member)
            .map(|(_, addr)| *addr)
            .ok_or_else(|| PoolError::NotFound(format!("{member} is not a member of this pool")))
    }

    /// Run one verb against a member, off the reactor.
    ///
    /// A daemon that refuses is a `Conflict` — every refusal on that surface
    /// is a state the caller can act on (a pen held elsewhere, a window
    /// nobody opened, a name already taken). A daemon that cannot be reached
    /// is a `Backend`, because nothing was learned about the pool at all.
    async fn on<T, F>(&self, member: &str, verb: F) -> Result<T>
    where
        F: FnOnce(&mut Client) -> std::result::Result<T, String> + Send + 'static,
        T: Send + 'static,
    {
        let addr = self.addr(member)?;
        let member = member.to_string();
        tokio::task::spawn_blocking(move || {
            let mut client = Client::connect(addr).map_err(|err| {
                PoolError::Backend(format!("cannot reach {member}'s pool daemon: {err}"))
            })?;
            verb(&mut client).map_err(PoolError::Conflict)
        })
        .await
        .map_err(|err| PoolError::Backend(format!("the control call did not run: {err}")))?
    }
}

#[async_trait]
impl PoolFleet for SocketFleet {
    async fn members(&self) -> Result<Vec<String>> {
        Ok(self.members.iter().map(|(name, _)| name.clone()).collect())
    }

    async fn node_id(&self, member: &str) -> Result<u8> {
        Ok(self.status(member).await?.node)
    }

    async fn status(&self, member: &str) -> Result<MemberStatus> {
        // A status the client could not parse is a `Conflict` by `on`'s
        // mapping, but nothing was learned about the pool — so it is
        // reported as a backend failure like an unreachable daemon.
        let view = self
            .on(member, |client| client.status())
            .await
            .map_err(|err| match err {
                PoolError::Conflict(why) => PoolError::Backend(format!(
                    "{member}'s daemon did not answer for itself: {why}"
                )),
                other => other,
            })?;
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
        })
    }

    async fn vdisks(&self, member: &str) -> Result<Vec<(u64, u64)>> {
        self.on(member, |client| client.vdisks()).await
    }

    async fn create_vdisk(&self, member: &str, vdisk: u64, size_bytes: u64) -> Result<()> {
        self.on(member, move |client| client.create_vdisk(vdisk, size_bytes))
            .await
    }

    async fn delete_vdisk(&self, member: &str, vdisk: u64) -> Result<()> {
        self.on(member, move |client| client.delete_vdisk(vdisk))
            .await
    }

    async fn export(&self, member: &str, vdisk: u64) -> Result<String> {
        // The device id is the vdisk id: one allocation, and the path comes
        // out identical on every member without anyone coordinating.
        let dev_id = u32::try_from(vdisk).map_err(|_| {
            PoolError::Conflict(format!("vdisk {vdisk} is beyond the ublk device id range"))
        })?;
        self.on(member, move |client| client.export(vdisk, dev_id))
            .await
    }

    async fn unexport(&self, member: &str, vdisk: u64) -> Result<()> {
        self.on(member, move |client| client.unexport(vdisk)).await
    }

    async fn exports(&self, member: &str) -> Result<Vec<(u64, String)>> {
        self.on(member, |client| client.exports()).await
    }

    async fn lease(&self, member: &str, vdisk: u64) -> Result<Option<(u8, Option<u8>)>> {
        let view = self.on(member, move |client| client.lease(vdisk)).await?;
        Ok(view.map(|lease| (lease.holder, lease.handing_to)))
    }

    async fn handover(&self, member: &str, vdisk: u64, to: u8) -> Result<()> {
        self.on(member, move |client| client.handover(vdisk, to))
            .await
    }

    async fn relinquish(&self, member: &str, vdisk: u64, to: u8) -> Result<()> {
        self.on(member, move |client| client.relinquish(vdisk, to))
            .await
    }

    async fn abort(&self, member: &str, vdisk: u64) -> Result<()> {
        self.on(member, move |client| client.abort(vdisk)).await
    }

    async fn accept(&self, member: &str, vdisk: u64) -> Result<()> {
        self.on(member, move |client| client.accept(vdisk)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_outside_the_pool_is_named_rather_than_dialed() {
        let fleet = SocketFleet::new(vec![(
            "lumen01".to_string(),
            "127.0.0.1:7777".parse().unwrap(),
        )]);
        let err = fleet.addr("lumen09").unwrap_err();
        assert!(err.to_string().contains("lumen09"), "{err}");
    }
}
