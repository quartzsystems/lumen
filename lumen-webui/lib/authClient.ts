// Session client for the Lumen console, ported from Quartz Command's model.
// Auth is an httpOnly cookie set by lumen-controlplane after a realm verifies
// the credentials; the browser calls /api on its OWN origin (in production the
// controlplane serves both; in dev Next.js proxies /api to it), so the cookie
// is first-party and sent automatically. JS can't read the ticket; the only
// client-side session state is a non-sensitive cached user (for display). The
// server is the real enforcement — every protected /api route requires a
// valid session.

/// Error carrying the HTTP status so callers can tell an invalid session (401)
/// apart from a backend that is down/restarting (5xx / network failure = 0).
/// Only a 401 means "sign in again".
export class ApiError extends Error {
  status: number;
  /// The decoded error body, when the response had one. Endpoints that reject
  /// a whole document (networking validation) return the per-field detail
  /// alongside the standard `error` string; without this the caller could
  /// only show the joined sentence.
  body?: unknown;
  constructor(message: string, status: number, body?: unknown) {
    super(message);
    this.status = status;
    this.body = body;
  }
}

/// The signed-in principal — `user@realm` split into parts.
export interface AuthUserInfo {
  username: string;
  realm: string;
}

/// An authentication realm as the login page's dropdown shows it.
export interface RealmInfo {
  id: string;
  name: string;
  kind: string;
  default: boolean;
}

const USER_KEY = "lumen-user";
const LOGIN_PATH = "/login";

const setUser = (user: AuthUserInfo): void => {
  if (typeof window === "undefined") return;
  localStorage.setItem(USER_KEY, JSON.stringify(user));
};

export const getCurrentUser = (): AuthUserInfo | null => {
  if (typeof window === "undefined") return null;
  try {
    const raw = localStorage.getItem(USER_KEY);
    return raw ? (JSON.parse(raw) as AuthUserInfo) : null;
  } catch {
    return null;
  }
};

export const clearSession = (): void => {
  if (typeof window === "undefined") return;
  localStorage.removeItem(USER_KEY);
};

const redirectToLogin = (): void => {
  if (typeof window === "undefined") return;
  // The export builds with trailingSlash, so the browser sees "/login/".
  const path = window.location.pathname.replace(/\/+$/, "") || "/";
  if (path !== LOGIN_PATH) {
    window.location.href = LOGIN_PATH;
  }
};

export const login = async (
  username: string,
  password: string,
  realm: string,
): Promise<AuthUserInfo> => {
  const res = await fetch("/api/auth/login", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ username, password, realm }),
  });
  if (!res.ok) {
    let message = "Invalid username or password.";
    if (res.status !== 401) {
      try {
        const body = await res.json();
        if (body?.error) message = body.error;
      } catch {}
    }
    throw new Error(message);
  }
  // Session is set via httpOnly cookie; the body is the user (for display).
  const user = (await res.json()) as AuthUserInfo;
  setUser(user);
  return user;
};

export const apiFetch = async <T>(path: string, init?: RequestInit): Promise<T> => {
  let res: Response;
  try {
    res = await fetch(`/api${path}`, {
      ...init,
      credentials: "include",
      headers: { "Content-Type": "application/json", ...init?.headers },
    });
  } catch {
    throw new ApiError("Could not reach the server.", 0);
  }

  if (res.status === 401) {
    clearSession();
    redirectToLogin();
    throw new ApiError("Session expired. Please sign in again.", 401);
  }

  if (!res.ok) {
    let message = `Request failed (${res.status})`;
    let payload: unknown;
    try {
      const body = await res.json();
      payload = body;
      if (body?.error) message = body.error;
    } catch {}
    throw new ApiError(message, res.status, payload);
  }

  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
};

/// Confirm the session with the backend (cookie is invisible to JS) and
/// refresh the cached user. Rejects when there is no valid session.
export const fetchMe = async (): Promise<AuthUserInfo> => {
  const user = await apiFetch<AuthUserInfo>("/auth/me");
  setUser(user);
  return user;
};

export const logout = async (): Promise<void> => {
  try {
    await fetch("/api/auth/logout", { method: "POST", credentials: "include" });
  } catch {}
  clearSession();
};

/// The configured realms for the login dropdown. Public (pre-auth) endpoint;
/// falls back to the built-in realm when the backend is unreachable so the
/// form still renders.
export const fetchRealms = async (): Promise<RealmInfo[]> => {
  try {
    const res = await fetch("/api/auth/realms", { credentials: "include" });
    if (res.ok) {
      const realms = (await res.json()) as RealmInfo[];
      if (realms.length > 0) return realms;
    }
  } catch {}
  return [{ id: "lumen", name: "Lumen", kind: "builtin", default: true }];
};
