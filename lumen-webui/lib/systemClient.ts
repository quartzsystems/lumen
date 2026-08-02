import { apiFetch } from "@/lib/authClient";

// Typed view of /api/system, modelled on lib/vmClient.ts: exported interfaces
// plus thin functions, no logic. Field names mirror lumen-sys's serde output
// exactly, so the wire format is the only contract between the two.

/// Whether an account can be signed in as, and why not when it cannot.
///
/// Three separate mechanisms say "no" on a Unix system and they are not
/// interchangeable — a locked password is undone by setting one, and a nologin
/// shell is not. The console offers the right remedy because the backend told
/// it which of the three this is.
export type LoginState = "enabled" | "locked" | "no_password" | "nologin" | "unknown";

export interface Action {
  allowed: boolean;
  reason?: string;
  requires_acknowledgement: boolean;
}

export interface UserActions {
  edit: Action;
  lock: Action;
  unlock: Action;
  delete: Action;
}

export interface LocalUser {
  name: string;
  uid: number;
  gid: number;
  full_name?: string | null;
  home: string;
  shell: string;
  groups: string[];
  /// In the admin group, and therefore able to administer this appliance.
  administrator: boolean;
  login: LoginState;
  /// Belongs to a package rather than to a person.
  system: boolean;
  password_changed_days?: number | null;
}

/// One row of the Authentication table. The account plus what may be done to
/// it — flattened by the backend, so the fields sit alongside each other.
export interface UserView extends LocalUser {
  /// This is the account the request was made by. Never lockable or removable.
  is_you: boolean;
  actions: UserActions;
}

export interface UsersResponse {
  node: string;
  users: UserView[];
  /// The login shells this node offers. Empty when /etc/shells could not be
  /// read, and then the console offers a free text field rather than an empty
  /// drop-down.
  shells: string[];
  /// The group that grants administrative rights, named by the backend rather
  /// than assumed here.
  admin_group: string;
}

export interface NewUser {
  name: string;
  password: string;
  full_name?: string;
  shell?: string;
  administrator?: boolean;
  create_home?: boolean;
}

export interface UserPatch {
  full_name?: string;
  shell?: string;
  administrator?: boolean;
  password?: string;
  locked?: boolean;
}

export interface DeleteUserResponse {
  name: string;
  removed_home?: string;
  kept_home?: string;
}

export type PowerAction = "reboot" | "power_off";

export interface ScheduledPower {
  action: PowerAction;
  /// Seconds since the epoch.
  at: number;
}

export interface PowerView {
  node: string;
  uptime_secs: number | null;
  /// The node's own clock, so a countdown runs against it rather than against
  /// a duration computed on the server and then aged in transit.
  now: number;
  scheduled: ScheduledPower | null;
  horizon_secs: number;
}

const post = <T>(path: string, body?: unknown): Promise<T> =>
  apiFetch<T>(path, { method: "POST", body: JSON.stringify(body ?? {}) });

const patch = <T>(path: string, body: unknown): Promise<T> =>
  apiFetch<T>(path, { method: "PATCH", body: JSON.stringify(body) });

const del = <T>(path: string, body?: unknown): Promise<T> =>
  apiFetch<T>(path, { method: "DELETE", body: JSON.stringify(body ?? {}) });

export const fetchUsers = (): Promise<UsersResponse> => apiFetch<UsersResponse>("/system/users");

export const createUser = (body: NewUser): Promise<UserView> =>
  post<UserView>("/system/users", body);

export const updateUser = (name: string, body: UserPatch): Promise<UserView> =>
  patch<UserView>(`/system/users/${encodeURIComponent(name)}`, body);

export const deleteUser = (
  name: string,
  removeHome: boolean,
  acknowledge: boolean,
): Promise<DeleteUserResponse> =>
  del<DeleteUserResponse>(`/system/users/${encodeURIComponent(name)}`, {
    remove_home: removeHome,
    i_understand_this_may_lose_data: acknowledge,
  });

export const fetchPower = (): Promise<PowerView> => apiFetch<PowerView>("/system/power");

/// Restart or shut down now. Answers 202 with no body — the connection is
/// about to go away, so there is nothing truthful to put in one.
export const powerNow = (action: PowerAction): Promise<void> =>
  post<void>("/system/power", { action });

export const powerAt = (action: PowerAction, at: number): Promise<PowerView> =>
  post<PowerView>("/system/power", { action, at });

export const cancelPower = (): Promise<PowerView> => del<PowerView>("/system/power");

// --- the environment ---------------------------------------------------------

/// One member as the Maintenance table carries it.
///
/// `reachable: false` with an `error` is a member that is in the environment
/// and could not be asked — which on this page is also the expected state of a
/// node doing exactly what the operator just told it to.
export interface MemberPower {
  node: string;
  local: boolean;
  reachable: boolean;
  error?: string | null;
  power?: PowerView | null;
}

export interface EnvironmentPower {
  members: MemberPower[];
}

/// Every member's uptime, clock, and schedule. One round trip per member and
/// nothing leaves the cluster, so this is safe on every page load.
export const fetchEnvironmentPower = (): Promise<EnvironmentPower> =>
  apiFetch<EnvironmentPower>("/environment/power");

/// Restart or shut one member down now.
///
/// Answers 202 with no body, whichever node it was: there is nothing truthful
/// to put in one about a node that is going down, and when the node is this
/// one the connection carrying it is about to go away.
export const powerNodeNow = (node: string, action: PowerAction): Promise<void> =>
  post<void>("/environment/power", { node, action });

/// The same, at a moment. Answers that member's own power view.
export const powerNodeAt = (
  node: string,
  action: PowerAction,
  at: number,
): Promise<PowerView> => post<PowerView>("/environment/power", { node, action, at });

/// Call off what one member has scheduled.
export const cancelNodePower = (node: string): Promise<PowerView> =>
  del<PowerView>("/environment/power", { node });

// --- display helpers ---------------------------------------------------------

/// How a login state reads, and what to do about it. The remedy is the point:
/// a locked account is unlocked, an account with no password is given one, and
/// a nologin account needs its shell changed — three different fixes.
export const LOGIN_LABEL: Record<LoginState, string> = {
  enabled: "enabled",
  locked: "locked",
  no_password: "no password",
  nologin: "no login shell",
  unknown: "unknown",
};

export const LOGIN_TONE: Record<LoginState, "ok" | "warn" | "crit" | "muted"> = {
  enabled: "ok",
  locked: "warn",
  no_password: "warn",
  nologin: "muted",
  unknown: "muted",
};

/// Uptime as the coarsest unit that is still true.
export const formatUptime = (seconds: number | null): string => {
  if (seconds === null) return "—";
  if (seconds < 60) return `${seconds} s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours} h ${minutes % 60} m`;
  return `${Math.floor(hours / 24)} d ${hours % 24} h`;
};

/// How long until a moment, in the words a countdown uses.
export const formatCountdown = (seconds: number): string => {
  if (seconds <= 0) return "any moment now";
  if (seconds < 60) return `in ${seconds} s`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `in ${minutes} minute${minutes === 1 ? "" : "s"}`;
  const hours = Math.floor(minutes / 60);
  return `in ${hours} h ${minutes % 60} m`;
};

/// A moment, in the browser's own timezone — which is the one the operator is
/// reading the screen in. The node's clock is what the countdown uses; this is
/// only for showing the wall time alongside it.
export const formatMoment = (epochSeconds: number): string =>
  new Date(epochSeconds * 1000).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });

/// A `datetime-local` input's value for a moment, in local wall time.
export const toLocalInputValue = (date: Date): string => {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}`;
};
