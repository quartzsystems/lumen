import { apiFetch } from "@/lib/authClient";

// Typed view of /api/system/updates, modelled on lib/systemClient.ts: exported
// interfaces plus thin functions, no logic. Field names mirror lumen-update's
// serde output exactly, so the wire format is the only contract between the two.

/// Which of three things an update is, from the operator's point of view.
///
/// `platform` is the one that matters: the kernel and the modules built
/// against its ABI — ZFS and DRBD — which move as one set or not at all. The
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

/// The one-line summary of what is waiting. Written as a sentence because it
/// is the first thing on the page and a bare count answers the wrong question.
export const summarize = (view: UpdateView): string => {
  const ordinary = view.updates.length;
  const platform = view.counts.platform;
  if (ordinary === 0 && platform === 0) {
    return view.checked_at ? "This node is up to date." : "Nothing has been checked yet.";
  }
  const parts: string[] = [];
  if (ordinary > 0) parts.push(`${ordinary} update${ordinary === 1 ? "" : "s"}`);
  if (platform > 0) parts.push(`${platform} kernel and storage package${platform === 1 ? "" : "s"}`);
  const security = view.counts.security;
  const tail = security > 0 ? ` ${security} of them named in a security advisory.` : "";
  return `${parts.join(", and ")} waiting.${tail}`;
};
