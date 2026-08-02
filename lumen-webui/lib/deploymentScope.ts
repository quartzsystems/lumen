"use client";

import { useEffect, useState } from "react";
import { fetchEnvironment } from "@/lib/clusterClient";

/// How big this deployment is, for the console's own words.
///
/// One appliance and six of them are the same product, and most of this
/// console says so without needing to know which: "in this environment" is
/// true of a single node, and naming the node a thing is on is true of a
/// cluster. That is the preferred way to write a sentence here — a phrasing
/// that reads correctly either way needs no branch, and a branch is two
/// strings to keep in step forever.
///
/// This is for the places where the two genuinely differ: an empty state that
/// should invite a cluster to be created, a warning that only means something
/// when there is somewhere else to move work to, a column that is noise until
/// there is a second node to put in it.
export interface DeploymentScope {
  /// Until the first answer arrives, nothing is known — and `clustered` is
  /// false, which is the safe way round: a sentence about other members must
  /// not flash up on an appliance that has none.
  loading: boolean;
  /// More than one node in this environment.
  clustered: boolean;
  /// How many nodes there are, this one included.
  nodes: number;
  /// This node's own name, for the sentences that should say it rather than
  /// say "this node".
  node: string | null;
}

const UNKNOWN: DeploymentScope = { loading: true, clustered: false, nodes: 1, node: null };

/// Read once per console session and shared.
///
/// The shape of a deployment changes through workflows that reload the console
/// anyway — joining an environment replaces the session, creating a cluster
/// restarts the control plane — so re-reading it on every page open would be
/// one request per navigation for an answer that cannot have changed.
let cached: Promise<DeploymentScope> | null = null;

const read = async (): Promise<DeploymentScope> => {
  const environment = await fetchEnvironment();
  const clustered = environment.clusters.flatMap((cluster) => cluster.nodes);
  const all = [
    ...clustered.map((node) => ({ node: node.node, local: node.local })),
    ...environment.unassigned.map((node) => ({ node: node.node, local: node.local })),
  ];
  return {
    loading: false,
    clustered: all.length > 1,
    nodes: Math.max(all.length, 1),
    node: all.find((entry) => entry.local)?.node ?? null,
  };
};

export function useDeploymentScope(): DeploymentScope {
  const [scope, setScope] = useState<DeploymentScope>(UNKNOWN);

  useEffect(() => {
    let alive = true;
    cached ??= read();
    cached
      .then((answer) => {
        if (alive) setScope(answer);
      })
      .catch(() => {
        // Not knowing is not an error worth showing anywhere: every sentence
        // this feeds has a form that is true of one node, and that is what
        // stays on screen.
        cached = null;
      });
    return () => {
      alive = false;
    };
  }, []);

  return scope;
}
