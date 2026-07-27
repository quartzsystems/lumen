//! The HA manager: restart machines whose node is confirmed lost.
//!
//! One rule above all others, and it is the reason this module is small:
//! **never act on "unreachable" alone.** A member that is `unclean` is a
//! member fencing has not finished with — it may still be running, still
//! writing — and acting then is how split-brain stories start. A member
//! that is offline and *clean* — fenced successfully, confirmed dead by the
//! operator's break-glass, or cleanly shut down — is a member Pacemaker has
//! vouched for, and only then do its HA machines restart elsewhere.
//!
//! The restart inventory is the replicated definitions (docs/storage.md):
//! each carries its home node and the full domain document, kept current at
//! define time precisely because libvirt on a dead node cannot be asked.
//! Which survivor acts is decided without coordination: the lowest-named
//! survivor holding a replica of every disk. Every eligible node computes
//! the same answer from the same gossiped record — and if two ever raced
//! anyway, DRBD's refusal of a second writer is the backstop.
//!
//! No automatic failback, deliberately: a machine that restarted here stays
//! here, visible as "away from home", until an operator migrates it back.
//! Failback that fires on its own turns one failure into two moves.

use crate::AppState;
use lumen_drbd::VmVolumes;

/// One pass: find lost-and-clean members, restart their HA machines here if
/// this node is the elected survivor. Called on an interval from `main`;
/// takes the whole state because it spans three domains and the task log.
pub async fn sweep(state: &AppState) {
    let Ok(Some(membership)) = state.cluster.environment_record() else {
        return;
    };
    let node = state.cluster.node().to_string();
    let Some(home_cluster) = membership.node(&node).and_then(|n| n.cluster.clone()) else {
        return;
    };
    let Ok(view) = state.cluster.cluster(&home_cluster).await else {
        return;
    };
    // A view with an error is a cluster this node could not ask; a
    // non-quorate view is a partition this node may be on the wrong side
    // of. Neither is a place to start machines from.
    if view.error.is_some() || !view.quorum.quorate {
        return;
    }

    // Lost and clean: offline, and not waiting on a fence. Unclean waits —
    // that is the fence-confirmed rule, not an optimization.
    let lost: Vec<String> = view
        .nodes
        .iter()
        .filter(|n| !n.online && !n.unclean)
        .map(|n| n.node.clone())
        .collect();
    if lost.is_empty() {
        return;
    }
    let survivors: Vec<String> = view
        .nodes
        .iter()
        .filter(|n| n.online)
        .map(|n| n.node.clone())
        .collect();

    let Ok(definitions) = state.cluster.stored_definitions() else {
        return;
    };
    for definition in definitions {
        if !lost.contains(&definition.node) {
            continue;
        }
        let Ok(parsed) = lumen_virt::domain_xml::parse(&definition.xml) else {
            tracing::warn!(
                vmid = definition.vmid,
                "a stored definition does not parse; it cannot be restarted"
            );
            continue;
        };
        if !parsed.config.ha {
            continue;
        }
        let vmid = parsed.config.vmid;

        // Already adopted — by this node on an earlier pass, or the home
        // came back before anyone acted.
        if state.virt.definition(vmid).await.is_ok() {
            continue;
        }

        // Where can it run? Only where every disk has a replica. A machine
        // with a local disk — or any disk this environment cannot resolve —
        // is not restartable, and the log says so once per sweep.
        let devices: Vec<String> = parsed
            .config
            .disks
            .iter()
            .map(|d| d.source.clone())
            .collect();
        if devices.is_empty() {
            continue;
        }
        let members = match state.drbd.common_members(&devices).await {
            Ok(members) => members,
            Err(err) => {
                tracing::warn!(vmid, "an HA machine cannot restart anywhere: {err}");
                continue;
            }
        };
        let mut eligible: Vec<&String> = survivors.iter().filter(|s| members.contains(s)).collect();
        eligible.sort();
        let Some(elected) = eligible.first() else {
            tracing::warn!(
                vmid,
                "no survivor holds a replica of every disk; the machine waits"
            );
            continue;
        };
        if **elected != node {
            continue;
        }

        let result = state.virt.adopt(&definition.xml).await;
        state.tasks.record(
            vmid,
            "ha-restart",
            format!("Restarted here after {} was lost", definition.node),
            "ha@lumen".to_string(),
            result.as_ref().err().map(|err| err.to_string()),
        );
        match result {
            Ok(_) => {
                tracing::warn!(vmid, from = %definition.node, "HA restarted a machine on this node");
                // Re-home the definition: this node is where the machine
                // lives now, and the record must say so before the next
                // failure.
                if let Ok(xml) = state.virt.definition(vmid).await {
                    if let Err(err) = state.cluster.replicate_definition(vmid, &xml).await {
                        tracing::warn!(vmid, "the adopted definition did not replicate: {err}");
                    }
                }
            }
            Err(err) => {
                tracing::error!(vmid, from = %definition.node, "the HA restart failed: {err}");
            }
        }
    }
}
