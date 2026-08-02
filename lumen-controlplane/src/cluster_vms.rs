//! Every member's machines, and finding the one that has a given machine.
//!
//! `lumen_virt` answers for the hypervisor it is attached to, which is one
//! node's. No member can speak for another's domains — libvirt on node A has
//! never heard of node B's machines — so the environment's list is a fan-out
//! and not a lookup in a record somebody keeps. There is no such record, and
//! inventing one would mean a second, staler answer to a question the members
//! can already answer for themselves.
//!
//! ## Which node has machine 103
//!
//! The console does not have to say. A request that names no node is resolved
//! here — this node first, because it is free, then every member at once —
//! and only then relayed. That is what lets `POST /api/vms/103/start` mean the
//! same thing from every console in the environment, and what keeps a console
//! holding a stale row from starting the wrong thing: the row's node is a hint
//! the operator never sees, and the identifier is what the request is about.
//!
//! An explicit `node` in the body still wins, for the caller that knows.

use std::sync::Arc;

use lumen_cluster::EnvironmentNode;
use lumen_virt::service::NodeVms;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::AppState;

/// A member that could not be asked, and why.
///
/// Reported rather than dropped, for the reason every other environment-wide
/// read reports its silences: a list that is quietly short is a list an
/// operator will read as "that machine is gone".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SilentMember {
    pub node: String,
    pub error: String,
}

/// GET /api/vms across the environment.
///
/// `nodes` keeps the shape `lumen_virt::service::VmsResponse` has always had —
/// grouped by node, one group per member — so a console written against the
/// single-node answer reads the clustered one unchanged. What is new is that
/// there can be more than one group, and that a member may be missing from
/// them and named below instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentVms {
    pub nodes: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreachable: Vec<SilentMember>,
}

/// This node's own machines.
pub async fn local(state: &Arc<AppState>) -> Result<NodeVms, ApiError> {
    let mut answer = state.virt.list().await?;
    // The service answers in the grouped shape even for one node; the peer
    // surface wants the group itself.
    Ok(answer.nodes.pop().unwrap_or(NodeVms {
        node: state.cluster.node().to_string(),
        vms: Vec::new(),
    }))
}

/// Every member's machines, asked for concurrently.
///
/// A node with no environment yet still answers — with itself, alone.
pub async fn environment(state: &Arc<AppState>) -> EnvironmentVms {
    let local_name = state.cluster.node().to_string();
    let nodes = state.cluster.environment_nodes().unwrap_or_default();

    if nodes.is_empty() {
        return EnvironmentVms {
            nodes: local_group(state, &local_name).await.into_iter().collect(),
            unreachable: Vec::new(),
        };
    }

    let calls = nodes.iter().map(|node| {
        let state = state.clone();
        let local_name = local_name.clone();
        async move {
            if node.name == local_name {
                return match local_group(&state, &node.name).await {
                    Some(group) => Ok(group),
                    None => Err(SilentMember {
                        node: node.name.clone(),
                        error: "This node's hypervisor could not be read.".to_string(),
                    }),
                };
            }
            state.peers.vms(node).await.map_err(|err| SilentMember {
                node: node.name.clone(),
                error: err.to_string(),
            })
        }
    });

    let answers = futures_util::future::join_all(calls).await;
    let mut view = EnvironmentVms {
        nodes: Vec::new(),
        unreachable: Vec::new(),
    };
    for answer in answers {
        match answer {
            Ok(group) => view.nodes.push(group),
            Err(silent) => view.unreachable.push(silent),
        }
    }
    view
}

/// This node's group as JSON, or `None` when its own hypervisor would not
/// answer — which is a silence like any other and is reported as one.
async fn local_group(state: &Arc<AppState>, node: &str) -> Option<serde_json::Value> {
    match local(state).await {
        Ok(group) => serde_json::to_value(group).ok(),
        Err(err) => {
            tracing::warn!(%node, "this node's machines could not be listed: {err}");
            None
        }
    }
}

/// The member holding a machine, or `None` for one this node has itself —
/// which is also the answer for a machine nothing has, so the local domain
/// gets to produce its own "no machine with identifier 42".
///
/// This node is asked first and for free. Only a miss costs a round trip per
/// member, and only then in parallel.
pub async fn owner_of(state: &Arc<AppState>, vmid: u32) -> Option<EnvironmentNode> {
    if state.virt.get(vmid).await.is_ok() {
        return None;
    }
    let local_name = state.cluster.node().to_string();
    let nodes = state.cluster.environment_nodes().unwrap_or_default();

    let calls = nodes
        .iter()
        .filter(|node| node.name != local_name)
        .map(|node| {
            let state = state.clone();
            async move {
                match state.peers.vms(node).await {
                    Ok(group) => has_vmid(&group, vmid).then(|| node.clone()),
                    Err(err) => {
                        // A member that cannot be asked is not evidence the
                        // machine is not there — but it is not evidence that
                        // it is, either, and the local domain's "no such
                        // machine" is the honest end of that.
                        tracing::debug!(node = %node.name, "could not be asked for its machines: {err}");
                        None
                    }
                }
            }
        });

    futures_util::future::join_all(calls)
        .await
        .into_iter()
        .flatten()
        .next()
}

/// Whether a member's answer contains the machine, read out of the JSON it
/// relayed rather than out of a type — the group crosses the wire as the
/// member serialized it, untouched, which is the whole point of the relay.
fn has_vmid(group: &serde_json::Value, vmid: u32) -> bool {
    group
        .get("vms")
        .and_then(|vms| vms.as_array())
        .is_some_and(|vms| {
            vms.iter()
                .any(|vm| vm.get("vmid").and_then(serde_json::Value::as_u64) == Some(vmid as u64))
        })
}

/// The lowest free identifier **across the environment**.
///
/// Node-local uniqueness is not enough once a console can see every member's
/// machines: two nodes each allocating 101 would give an operator two machines
/// with one name in the table, and — worse — a migration that cannot land.
/// Advisory, like the node-local one it replaces: it is what the create dialog
/// shows before anything exists, and the service still allocates for real.
pub async fn next_vmid(state: &Arc<AppState>) -> Result<u32, ApiError> {
    use lumen_virt::{FIRST_VMID as FIRST, LAST_VMID as LAST};

    let mut taken: Vec<u32> = Vec::new();
    for group in environment(state).await.nodes {
        if let Some(vms) = group.get("vms").and_then(|vms| vms.as_array()) {
            taken.extend(
                vms.iter()
                    .filter_map(|vm| vm.get("vmid").and_then(serde_json::Value::as_u64))
                    .map(|vmid| vmid as u32),
            );
        }
    }
    taken.sort_unstable();
    (FIRST..=LAST)
        .find(|id| taken.binary_search(id).is_err())
        .ok_or_else(|| {
            ApiError::Conflict(format!(
                "Every machine identifier from {FIRST} to {LAST} is in use."
            ))
        })
}
