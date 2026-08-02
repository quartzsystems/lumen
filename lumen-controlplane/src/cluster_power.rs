//! Every member's power state, and restarting or shutting one down by name.
//!
//! The node-local feature in `src/api/system.rs` answers "when did *this* node
//! boot, what is it committed to, and commit it to something else". This
//! module is the other half: the same three questions asked of every member,
//! and answered for whichever one the operator named.
//!
//! ## Reading is a fan-out, acting is one node
//!
//! The read is concurrent — six members answering at once, the wall clock the
//! slowest one rather than the sum, exactly as [`crate::inventory`] and
//! [`crate::cluster_updates`] do. There is no walk here and there must not be
//! one: a restart is a single node going down at a moment the operator chose,
//! and an "every node" button on this page would be an environment-wide outage
//! behind one click. Taking a whole cluster through restarts one at a time is
//! what a rolling update already is, and it lives next door.
//!
//! ## The guards stay on the node they are about
//!
//! Whether a cluster can spare a member is a question about that member's
//! cluster, asked with that member's view of quorum — so
//! [`crate::api::system::guard_cluster_power`] runs *there*, on the node being
//! restarted, and not here on the node relaying the request. A coordinator's
//! belief about another node's quorum carries no weight, which is the same
//! rule the rolling update's peer restart already follows.
//!
//! What crosses the wire is the instruction and the operator's name for the
//! journal. The acknowledgement that overrides the quorum guard does not cross
//! it, for the reason `apply_updates` gives about the kernel: consent was
//! given to a console, and a peer route that accepted "yes, lose quorum" from
//! a body would be a second, quieter way to take a cluster down.

use std::sync::Arc;

use lumen_cluster::EnvironmentNode;
use lumen_sys::model::PowerAction;
use lumen_sys::service::PowerView;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::AppState;

/// One member as the Maintenance table carries it: its power state, or why it
/// has none to report.
///
/// `reachable: false` with an `error` is a member that is in the environment
/// and could not be asked — which, on this page, is also the expected state of
/// a member that is doing exactly what the operator told it to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberPower {
    pub node: String,
    pub local: bool,
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power: Option<PowerView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentPower {
    pub members: Vec<MemberPower>,
}

/// Every member's power state, asked for concurrently.
///
/// A node with no environment yet still answers — with itself, alone. The
/// console renders the same table either way, which is what lets the page
/// exist on a single appliance without a second code path.
pub async fn environment(state: &Arc<AppState>) -> EnvironmentPower {
    let local_name = state.cluster.node().to_string();
    let nodes = state.cluster.environment_nodes().unwrap_or_default();

    let members = if nodes.is_empty() {
        vec![local_member(state, &local_name).await]
    } else {
        let calls = nodes.iter().map(|node| {
            let state = state.clone();
            let local_name = local_name.clone();
            async move {
                if node.name == local_name {
                    return local_member(&state, &node.name).await;
                }
                match state.peers.power_state(node).await {
                    Ok(power) => MemberPower {
                        node: node.name.clone(),
                        local: false,
                        reachable: true,
                        error: None,
                        power: Some(power),
                    },
                    Err(err) => MemberPower {
                        node: node.name.clone(),
                        local: false,
                        reachable: false,
                        error: Some(err.to_string()),
                        power: None,
                    },
                }
            }
        });
        futures_util::future::join_all(calls).await
    };

    EnvironmentPower { members }
}

/// This node's own answer, from its own service. Never a socket: only this
/// process holds the system service, and a loopback round trip would add ways
/// to fail without adding anything true.
async fn local_member(state: &Arc<AppState>, node: &str) -> MemberPower {
    match state.sys.power().await {
        Ok(power) => MemberPower {
            node: node.to_string(),
            local: true,
            reachable: true,
            error: None,
            power: Some(power),
        },
        Err(err) => MemberPower {
            node: node.to_string(),
            local: true,
            reachable: false,
            error: Some(err.to_string()),
            power: None,
        },
    }
}

/// Restart or shut one member down, now or at a moment.
///
/// Answers the member's own power view for a schedule, and `None` for an
/// immediate one — there is nothing truthful to put in a body about a node
/// that is going down, and on the local node the connection carrying it is
/// about to go away.
pub async fn set(
    state: &Arc<AppState>,
    node: &str,
    action: PowerAction,
    at: Option<u64>,
    acknowledged: bool,
    by: &str,
) -> Result<Option<PowerView>, ApiError> {
    if is_local(state, node) {
        return crate::api::system::power_locally(state, action, at, acknowledged).await;
    }

    let member = member(state, node)?;
    Ok(state.peers.set_power(&member, action, at, by).await?)
}

/// Call off whatever one member has scheduled.
pub async fn cancel(state: &Arc<AppState>, node: &str) -> Result<PowerView, ApiError> {
    if is_local(state, node) {
        return crate::api::system::cancel_locally(state).await;
    }

    let member = member(state, node)?;
    Ok(state.peers.cancel_power(&member).await?)
}

fn is_local(state: &Arc<AppState>, node: &str) -> bool {
    state.cluster.node() == node
}

/// The member by name, or the refusal that names what this node can see.
///
/// A node that is in the environment record but switched off still resolves
/// here — the peer call is what discovers it is unreachable, and "could not be
/// reached" is a different sentence from "there is no such node", which is the
/// distinction an operator acting on the wrong name needs.
fn member(state: &Arc<AppState>, node: &str) -> Result<EnvironmentNode, ApiError> {
    state
        .cluster
        .environment_nodes()
        .unwrap_or_default()
        .into_iter()
        .find(|member| member.name == node)
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "There is no node called \"{node}\" in this environment."
            ))
        })
}
