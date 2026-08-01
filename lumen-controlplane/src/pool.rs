//! The LumenFS pool on this node, if there is one — and how the control
//! plane decides.
//!
//! The decision is a read, not a record: the pool daemon's own drop-in,
//! `/etc/lumen/fsd.conf`, is the one place that says whether this node
//! carries a pool, and its presence *is* the answer. Membership is not in
//! the file and does not belong there — a pool spans its cluster, so the
//! members are the cluster's members, which the environment record already
//! names. See `lumen_pool::config` for why there is deliberately no second
//! source of truth to fall out of step with the first.
//!
//! Three shapes come out of the read, and the middle one exists on purpose:
//! a drop-in that is present but unusable is a **broken deployment**, not a
//! standalone appliance, and folding it into "no pool" would hide exactly
//! the situation an operator most needs shown.

use std::net::SocketAddr;
use std::sync::Arc;

use lumen_cluster::ClusterService;
use lumen_pool::{PeeredFleet, PoolConfig, PoolPeers, PoolService};

/// Whether this node carries a pool, resolved once at startup.
///
/// Startup rather than per request because the answer changes only when a
/// pool is created or destroyed, and both of those workflows (phase 4's
/// drive wizard) restart what they reconfigure. Cluster membership is also
/// read once here; reshaping a pool when its cluster grows is phase 5's
/// slice reassignment, which is where a live re-read belongs.
pub enum PoolPresence {
    /// No drop-in: a node with no pool — the standalone appliance among
    /// them. The common case, and not an error anywhere.
    Absent,
    /// The drop-in exists but no pool can be assembled from it. The console
    /// shows this sentence rather than an empty page, because "nothing to
    /// show" and "your deployment is broken" must not look alike.
    Broken(String),
    /// The pool, callable.
    Present {
        /// The whole pool: the observed view, the snapshot verbs, and the
        /// compute seam.
        service: Arc<PoolService>,
        /// This node's daemon's control surface — loopback — for serving
        /// peer verbs against the one daemon this machine may speak to.
        control: SocketAddr,
        /// The brick paths the drop-in serves from, as written there — what
        /// [`PoolPresence::brick_clearance`] hands the storage domain so
        /// its wipe guard can tell this pool's bricks from leftovers.
        bricks: Vec<String>,
    },
}

impl PoolPresence {
    /// Read the drop-in and, if it names a pool, assemble the service over
    /// it. `peers` is the authenticated channel to the other members'
    /// control planes; the local daemon is reached over loopback and never
    /// through it.
    pub fn assemble(cluster: &ClusterService, peers: Arc<dyn PoolPeers>) -> PoolPresence {
        let config = match PoolConfig::load() {
            Ok(Some(config)) => config,
            Ok(None) => return PoolPresence::Absent,
            Err(err) => return PoolPresence::Broken(err.to_string()),
        };
        PoolPresence::from_config(config, cluster, peers)
    }

    /// The half after the file read, separated so tests can hand in a
    /// config without owning `/etc`.
    pub fn from_config(
        config: PoolConfig,
        cluster: &ClusterService,
        peers: Arc<dyn PoolPeers>,
    ) -> PoolPresence {
        // A pool exists only inside a cluster: the drop-in names this
        // node's brick, and the environment record names everyone else's.
        let record =
            match cluster.environment_record() {
                Ok(Some(record)) => record,
                Ok(None) => return PoolPresence::Broken(
                    "a pool daemon is configured, but this node has not joined an environment — \
                     the pool's members are its cluster's members, and there is no cluster to ask"
                        .into(),
                ),
                Err(err) => {
                    return PoolPresence::Broken(format!(
                    "a pool daemon is configured, but the environment record is unreadable: {err}"
                ))
                }
            };
        let local = cluster.node();
        let Some(assignment) = record
            .nodes
            .iter()
            .find(|n| n.name == local)
            .and_then(|n| n.cluster.clone())
        else {
            return PoolPresence::Broken(format!(
                "a pool daemon is configured, but {local} is not assigned to any cluster"
            ));
        };
        let members: Vec<String> = record
            .nodes
            .iter()
            .filter(|n| n.cluster.as_deref() == Some(assignment.as_str()))
            .map(|n| n.name.clone())
            .collect();
        let fleet = PeeredFleet::new(local, config.control, &members, peers);
        PoolPresence::Present {
            // The pool is named for its cluster: the storage page and the
            // machine page then call the same thing by the same name.
            service: Arc::new(PoolService::new(Arc::new(fleet), &assignment)),
            control: config.control,
            bricks: config
                .bricks
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        }
    }

    /// The service, when there is one — the shape the API handlers match on.
    pub fn service(&self) -> Option<&Arc<PoolService>> {
        match self {
            PoolPresence::Present { service, .. } => Some(service),
            _ => None,
        }
    }

    /// What the storage domain's wipe guard needs to know from here: no
    /// pool means every brick found on a disk is a leftover; a broken
    /// drop-in means nothing can be told; a serving pool names its bricks
    /// so everything else clears.
    pub fn brick_clearance(&self) -> lumen_zfs::BrickClearance {
        match self {
            PoolPresence::Absent => lumen_zfs::BrickClearance::NoPool,
            PoolPresence::Broken(_) => lumen_zfs::BrickClearance::Unknown,
            PoolPresence::Present { bricks, .. } => {
                lumen_zfs::BrickClearance::PoolBricks(bricks.clone())
            }
        }
    }
}
