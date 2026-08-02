import { apiFetch } from "@/lib/authClient";

// Typed view of /api/system/updates, modelled on lib/systemClient.ts: exported
// interfaces plus thin functions, no logic. Field names mirror lumen-update's
// serde output exactly, so the wire format is the only contract between the two.

/// Which of three things an update is, from the operator's point of view.
///
/// `platform` is the one that matters: the kernel and the modules built
/// against its ABI — ZFS above all — which move as one set or not at all. The
/// console never offers them on the same button as everything else.
export type UpdateKind = "lumen" | "platform" | "other";

export interface Update {
  name: string;
  arch: string;
  /// The version that would be installed.
  version: string;
  /// What is installed now. Absent for a package that is not installed at all.
  installed?: string | null;
  repo: string;
  kind: UpdateKind;
  /// The advisory covering it, when the repository publishes advisory data.
  /// Absent means *nothing said so* — not "carries no security fix".
  advisory?: string | null;
  security: boolean;
}

/// The platform set, and whether the package manager can move it.
export interface PlatformPlan {
  updates: Update[];
  /// Whether they resolve as one transaction. When false the console must not
  /// offer the button — installing a kernel without its storage modules is
  /// what leaves a node unable to import its pool.
  resolves: boolean;
  /// The package manager's own words for why not.
  detail?: string | null;
}

export interface KernelState {
  running: string;
  newest?: string | null;
}

export interface RebootState {
  required: boolean;
  reason?: string | null;
  kernel: KernelState;
}

export interface UpdateCounts {
  lumen: number;
  other: number;
  security: number;
  platform: number;
}

export interface UpdateView {
  node: string;
  /// Seconds since the epoch, absent before the first check.
  checked_at?: number;
  /// Everything that is not the platform set.
  updates: Update[];
  platform: PlatformPlan;
  reboot: RebootState;
  counts: UpdateCounts;
  /// Why the last check failed. The rest of the page still renders.
  error?: string;
}

export type UpdatePhase = "running" | "complete" | "failed";

export interface UpdateProgress {
  node: string;
  /// `ordinary` or `platform` — which decision this transaction was.
  kind: string;
  phase: UpdatePhase;
  started_at: number;
  finished_at?: number;
  /// Who asked, as the `user@realm` principal.
  by: string;
  changed: string[];
  log?: string;
  error?: string;
  reboot?: RebootState;
}

const post = <T>(path: string, body?: unknown): Promise<T> =>
  apiFetch<T>(path, { method: "POST", body: JSON.stringify(body ?? {}) });

/// What was last found. Never asks the repositories, so this is safe to call
/// on every page load.
export const fetchUpdates = (): Promise<UpdateView> => apiFetch<UpdateView>("/system/updates");

/// Ask the repositories now. Takes as long as the network does.
export const checkUpdates = (): Promise<UpdateView> => post<UpdateView>("/system/updates/check");

/// Start installing the ordinary updates — everything except the kernel and
/// the modules built against it.
export const installUpdates = (): Promise<UpdateProgress> =>
  post<UpdateProgress>("/system/updates/apply", { platform: false });

/// Start installing the platform set. The acknowledgement is required by the
/// backend and is not defaulted here on purpose: the caller has to have shown
/// the operator what they are agreeing to.
export const installPlatform = (acknowledged: boolean): Promise<UpdateProgress> =>
  post<UpdateProgress>("/system/updates/apply", {
    platform: true,
    i_understand_the_kernel_moves: acknowledged,
  });

export const fetchUpdateProgress = (): Promise<UpdateProgress | null> =>
  apiFetch<UpdateProgress | null>("/system/updates/progress");

// --- the environment ---------------------------------------------------------

/// One member's answer, from one round trip: what it has waiting, and what it
/// is installing.
export interface NodeUpdates {
  view: UpdateView;
  progress?: UpdateProgress | null;
}

/// One member as the cluster table carries it.
///
/// `reachable: false` with an `error` is a member that is in the environment
/// and could not be asked — a different thing from a member with nothing
/// waiting, and during an environment-wide update it is the *expected* state
/// of whichever member is restarting its control plane.
export interface MemberUpdates {
  node: string;
  local: boolean;
  reachable: boolean;
  error?: string | null;
  updates?: NodeUpdates | null;
}

export interface ClusterCounts {
  members: number;
  reachable: number;
  members_with_updates: number;
  members_with_platform: number;
  updates: number;
  security: number;
  platform: number;
  restarts_required: number;
  platform_blocked: number;
}

export interface ClusterUpdateView {
  members: MemberUpdates[];
  counts: ClusterCounts;
}

export type StepState = "pending" | "running" | "done" | "failed" | "unwound";

/// Where one member is in its leg. An ordinary update only ever reaches
/// `installing`; a rolling one goes through all of them.
export type MemberStage =
  | "waiting"
  | "checking"
  | "draining"
  | "installing"
  | "restarting"
  | "rejoining"
  | "returning"
  | "finished";

/// A machine that would not move off a node being drained.
export interface Stranded {
  vmid: number;
  name: string;
  reason: string;
}

/// One member's leg of an environment-wide update.
export interface MemberStep {
  node: string;
  state: StepState;
  stage: MemberStage;
  detail?: string | null;
  changed: string[];
  restart_required: boolean;
  /// Machines that would not move off during the drain. Non-empty is why a
  /// rolling update stopped rather than restarting the node under them.
  stranded: Stranded[];
}

/// `ordinary` installs in place; `rolling` restarts each node as it goes.
export type RollKind = "ordinary" | "rolling";

export interface RollProgress {
  kind: RollKind;
  phase: UpdatePhase;
  by: string;
  started_at: number;
  finished_at?: number;
  error?: string;
  /// One per member, in the order they are visited — every peer, then the
  /// node coordinating. Named in full before anything is installed.
  steps: MemberStep[];
  /// What the update deliberately did not do. Set when a rolling update
  /// finishes with the coordinating node still needing a restart — a node
  /// cannot drive a rolling update through its own reboot.
  left_to_you?: string | null;
}

/// What every member has waiting. Never asks any member's repositories, so
/// this is safe on every page load.
export const fetchClusterUpdates = (): Promise<ClusterUpdateView> =>
  apiFetch<ClusterUpdateView>("/environment/updates");

/// Every member asks its repositories now, concurrently.
export const checkClusterUpdates = (): Promise<ClusterUpdateView> =>
  post<ClusterUpdateView>("/environment/updates/check");

/// Install the ordinary updates on every member, one at a time. Nothing is
/// drained and nothing restarts; the kernel is not moved by this.
export const installClusterUpdates = (): Promise<RollProgress> =>
  post<RollProgress>("/environment/updates/apply", {});

/// Take every member that needs a restart through maintenance, its kernel, and
/// a reboot — one at a time, waiting for each to rejoin before the next.
///
/// The acknowledgement is required by the backend and is not defaulted here on
/// purpose: the caller has to have shown the operator what they are agreeing
/// to.
export const rollClusterUpdates = (acknowledged: boolean): Promise<RollProgress> =>
  post<RollProgress>("/environment/updates/apply", {
    rolling: true,
    i_understand_each_node_restarts: acknowledged,
  });

/// What one member's stage reads as in the table, while a walk is running.
export const STAGE_LABEL: Record<MemberStage, string> = {
  waiting: "waiting",
  checking: "checking",
  draining: "moving machines off",
  installing: "installing",
  restarting: "restarting",
  rejoining: "coming back",
  returning: "back into service",
  finished: "finished",
};

/// The walk, running or finished.
///
/// `null` once it has updated the coordinating node, because installing Lumen
/// restarts the control plane holding this record. That is not a gap to paper
/// over: `fetchClusterUpdates` reads what every member actually ended up with,
/// which is the better answer anyway.
export const fetchRollProgress = (): Promise<RollProgress | null> =>
  apiFetch<RollProgress | null>("/environment/updates/progress");

/// One package waiting somewhere in the environment, and the nodes waiting on
/// it.
///
/// The same package at the same version on six nodes is six transactions, but
/// it is one thing to read — so it is one row carrying six node names, rather
/// than six rows saying the same sentence. The counts in [`ClusterCounts`] are
/// deliberately *not* built this way and should not be reconciled with this
/// table: those count the work, this lists what the work installs.
export interface PendingUpdate {
  /// Stable across refreshes, which is what lets the table keep a selection.
  key: string;
  /// One member's row, standing for all of them. Every field the table shows
  /// is part of the key below, so no node disagrees with it.
  update: Update;
  /// In the order the members came back, which is the order the table shows
  /// them in everywhere else.
  nodes: string[];
}

/// Roll every member's pending packages up into one list.
///
/// Keyed by what would be installed *and* what is installed now: a node two
/// versions behind is not the same fact as a node one behind, and merging them
/// would quietly hide the one that is further back.
export const collectPending = (
  members: MemberUpdates[],
  platform: boolean,
): PendingUpdate[] => {
  const byKey = new Map<string, PendingUpdate>();
  for (const member of members) {
    const view = member.updates?.view;
    if (!view) continue;
    for (const update of platform ? view.platform.updates : view.updates) {
      const key = `${update.name}.${update.arch}|${update.version}|${update.installed ?? ""}`;
      const seen = byKey.get(key);
      if (seen) seen.nodes.push(member.node);
      else byKey.set(key, { key, update, nodes: [member.node] });
    }
  }
  return [...byKey.values()].sort((a, b) => a.update.name.localeCompare(b.update.name));
};

// --- display helpers ---------------------------------------------------------

export const KIND_LABEL: Record<UpdateKind, string> = {
  lumen: "Lumen",
  platform: "Kernel & storage",
  other: "System",
};

export const KIND_TONE: Record<UpdateKind, "ok" | "info" | "warn" | "muted"> = {
  lumen: "info",
  platform: "warn",
  other: "muted",
};

/// How long ago a moment was, in the words a "last checked" line uses. The
/// node's clock is what produced the timestamp, so this is measured against
/// the reading rather than against the browser alone.
export const formatAgo = (epochSeconds: number, nowSeconds: number): string => {
  const seconds = Math.max(0, nowSeconds - epochSeconds);
  if (seconds < 90) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} minute${minutes === 1 ? "" : "s"} ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours} hour${hours === 1 ? "" : "s"} ago`;
  return `${Math.floor(hours / 24)} days ago`;
};

/// How long a transaction has been running, or took.
export const formatElapsed = (seconds: number): string => {
  if (seconds < 60) return `${seconds} s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes} m ${seconds % 60} s`;
};
