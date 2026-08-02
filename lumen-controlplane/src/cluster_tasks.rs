//! Every member's log, in one answer.
//!
//! [`crate::tasks`] is one node's history: what was asked of its machines, and
//! what happened to the node itself. This module is the same question asked of
//! every member at once, which is the question an operator actually has —
//! "what has been happening here" is about the appliance, and an appliance is
//! its nodes.
//!
//! ## The node is not in the record
//!
//! Each member's records say what happened, not where: a log a node keeps
//! about itself has no reason to repeat its own name on every line, and adding
//! one would be a field that could disagree with the node that answered. So
//! the name is attached here, from the member that gave the records, and the
//! console's Node column reads it from the wrapper.
//!
//! ## Concurrent, capped, and never merged here
//!
//! Read concurrently, like every other environment-wide read. The merge into
//! one ordering is deliberately the console's: each member's window is already
//! sorted newest-first, and interleaving them server-side would mean choosing
//! a global window size before knowing which member's history the operator is
//! about to filter down to.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::tasks::TaskRecord;
use crate::AppState;

/// One member's window onto its own log, or why it has none to show.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberTasks {
    pub node: String,
    pub local: bool,
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Newest first, as that member's own log hands them over.
    #[serde(default)]
    pub tasks: Vec<TaskRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentTasks {
    pub members: Vec<MemberTasks>,
}

/// Every member's most recent `limit` entries, asked for concurrently.
///
/// A node with no environment yet still answers — with itself, alone.
pub async fn environment(state: &Arc<AppState>, limit: usize) -> EnvironmentTasks {
    let local_name = state.cluster.node().to_string();
    let nodes = state.cluster.environment_nodes().unwrap_or_default();

    let members = if nodes.is_empty() {
        vec![local_member(state, &local_name, limit)]
    } else {
        let calls = nodes.iter().map(|node| {
            let state = state.clone();
            let local_name = local_name.clone();
            async move {
                if node.name == local_name {
                    return local_member(&state, &node.name, limit);
                }
                match state.peers.tasks(node, limit).await {
                    Ok(tasks) => MemberTasks {
                        node: node.name.clone(),
                        local: false,
                        reachable: true,
                        error: None,
                        tasks,
                    },
                    Err(err) => MemberTasks {
                        node: node.name.clone(),
                        local: false,
                        reachable: false,
                        error: Some(err.to_string()),
                        tasks: Vec::new(),
                    },
                }
            }
        });
        futures_util::future::join_all(calls).await
    };

    EnvironmentTasks { members }
}

fn local_member(state: &Arc<AppState>, node: &str, limit: usize) -> MemberTasks {
    MemberTasks {
        node: node.to_string(),
        local: true,
        reachable: true,
        error: None,
        tasks: state.tasks.recent(limit),
    }
}
