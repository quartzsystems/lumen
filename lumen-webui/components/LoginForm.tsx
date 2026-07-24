"use client";

import { useEffect, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import * as api from "@/lib/authClient";
import type { RealmInfo } from "@/lib/authClient";

/// Minimal inline eye icons (avoids an icon dependency for one control).
/// Ported verbatim from Quartz Command's login page.
function EyeIcon({ off }: { off?: boolean }) {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {off ? (
        <>
          <path d="M9.88 9.88a3 3 0 1 0 4.24 4.24" />
          <path d="M10.73 5.08A10.43 10.43 0 0 1 12 5c7 0 10 7 10 7a13.16 13.16 0 0 1-1.67 2.68" />
          <path d="M6.61 6.61A13.526 13.526 0 0 0 2 12s3 7 10 7a9.74 9.74 0 0 0 5.39-1.61" />
          <line x1="2" y1="2" x2="22" y2="22" />
        </>
      ) : (
        <>
          <path d="M2 12s3-7 10-7 10 7 10 7-3 7-10 7-10-7-10-7Z" />
          <circle cx="12" cy="12" r="3" />
        </>
      )}
    </svg>
  );
}

/// Chevron for the realm dropdown.
function ChevronIcon({ open }: { open?: boolean }) {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      style={{
        transform: open ? "rotate(180deg)" : undefined,
        transition: "transform var(--qz-dur-2) var(--qz-ease-out)",
      }}
    >
      <path d="m6 9 6 6 6-6" />
    </svg>
  );
}

/// Check mark on the selected realm option.
function CheckIcon() {
  return (
    <svg
      width="13"
      height="13"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M20 6 9 17l-5-5" />
    </svg>
  );
}

/// Custom realm dropdown. A native <select>'s popup takes the OS widget
/// theme and can't be styled to the Quartz dark tokens, so the listbox is
/// drawn by hand: same field styling as the text inputs, options on a
/// raised dark surface. Keyboard: arrows move, Enter/Space select, Escape
/// closes.
function RealmSelect({
  realms,
  value,
  onChange,
}: {
  realms: RealmInfo[];
  value: string;
  onChange: (id: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const rootRef = useRef<HTMLDivElement>(null);
  const selected = realms.find((r) => r.id === value);

  // Close when clicking anywhere outside the control.
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (!rootRef.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [open]);

  const openList = () => {
    setActive(Math.max(0, realms.findIndex((r) => r.id === value)));
    setOpen(true);
  };

  const choose = (id: string) => {
    onChange(id);
    setOpen(false);
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (!open) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp" || e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        openList();
      }
      return;
    }
    if (e.key === "Escape") {
      setOpen(false);
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((i) => Math.min(i + 1, realms.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      if (realms[active]) choose(realms[active].id);
    } else if (e.key === "Tab") {
      setOpen(false);
    }
  };

  return (
    <div className="relative" ref={rootRef}>
      <button
        type="button"
        onClick={() => (open ? setOpen(false) : openList())}
        onKeyDown={handleKeyDown}
        aria-haspopup="listbox"
        aria-expanded={open}
        className="w-full rounded-md px-3 py-[10px] text-[13px] text-left text-[var(--qz-fg-1)] outline-none transition-colors cursor-pointer flex items-center justify-between"
        style={{
          background: "var(--qz-input-bg)",
          border: `1px solid ${open ? "var(--qz-accent)" : "var(--qz-border)"}`,
        }}
        onFocus={(e) => (e.currentTarget.style.borderColor = "var(--qz-accent)")}
        onBlur={(e) => {
          if (!open) e.currentTarget.style.borderColor = "var(--qz-border)";
        }}
      >
        <span>{selected?.name ?? value}</span>
        <span className="text-[var(--qz-fg-4)]">
          <ChevronIcon open={open} />
        </span>
      </button>

      {open && (
        <ul
          role="listbox"
          aria-label="Realm"
          className="absolute left-0 right-0 z-10 mt-1 m-0 p-1 list-none rounded-md overflow-hidden"
          style={{
            background: "var(--qz-surface-raised)",
            border: "1px solid var(--qz-border-strong)",
            boxShadow: "var(--qz-shadow-2)",
          }}
        >
          {realms.map((r, i) => (
            <li
              key={r.id}
              role="option"
              aria-selected={r.id === value}
              onPointerEnter={() => setActive(i)}
              onClick={() => choose(r.id)}
              className="rounded px-2 py-[8px] text-[13px] cursor-pointer flex items-center justify-between transition-colors"
              style={{
                background: i === active ? "var(--qz-accent-soft)" : "transparent",
                color: r.id === value ? "var(--qz-fg-1)" : "var(--qz-fg-2)",
              }}
            >
              <span>{r.name}</span>
              {r.id === value && (
                <span style={{ color: "var(--qz-accent)" }}>
                  <CheckIcon />
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/// The Quartz Command sign-in card with Lumen branding: username instead of
/// email (the built-in realm authenticates OS accounts) plus a Realm
/// dropdown. Credentials are verified server-side by the selected realm,
/// which sets an httpOnly session cookie.
export function LoginForm() {
  const router = useRouter();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [realms, setRealms] = useState<RealmInfo[]>([]);
  const [realm, setRealm] = useState("lumen");
  const [showPassword, setShowPassword] = useState(false);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  // Already signed in? Skip straight to the console.
  useEffect(() => {
    api
      .fetchMe()
      .then(() => router.replace("/"))
      .catch(() => {});
  }, [router]);

  useEffect(() => {
    api.fetchRealms().then((list) => {
      setRealms(list);
      const preselected = list.find((r) => r.default) ?? list[0];
      if (preselected) setRealm(preselected.id);
    });
  }, []);

  const handleSubmit = async (e: React.FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    if (!username || !password) {
      setError("Username and password are required.");
      return;
    }
    setError("");
    setLoading(true);
    try {
      await api.login(username, password, realm);
      router.push("/");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not reach the server.");
    } finally {
      setLoading(false);
    }
  };

  const inputBase =
    "w-full rounded-md px-3 py-[10px] text-[13px] text-[var(--qz-fg-1)] outline-none transition-colors";

  return (
    <div
      className="min-h-screen flex flex-col items-center justify-center"
      style={{ background: "var(--qz-bg)" }}
    >
      <div
        className="w-full max-w-[360px] rounded-xl p-8 flex flex-col gap-6"
        style={{
          background: "var(--qz-ink-0)",
          border: "1px solid var(--qz-border)",
          boxShadow: "var(--qz-shadow-3)",
        }}
      >
        {/* Logo + heading */}
        <div className="flex items-center justify-center gap-3">
          <img src="/logo-mark.png" alt="Lumen" className="w-9 h-9" />
          <h1
            className="text-[22px] font-bold text-[var(--qz-fg-1)] m-0"
            style={{ letterSpacing: "-0.02em" }}
          >
            Lumen
          </h1>
        </div>

        {/* Form */}
        <form onSubmit={handleSubmit} className="flex flex-col gap-3">
          <div>
            <label className="block text-[12px] text-[var(--qz-fg-3)] mb-[6px]">Username</label>
            <input
              type="text"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              placeholder="Enter your username"
              autoComplete="username"
              autoCapitalize="none"
              spellCheck={false}
              className={inputBase}
              style={{
                background: "var(--qz-input-bg)",
                border: "1px solid var(--qz-border)",
              }}
              onFocus={(e) => (e.currentTarget.style.borderColor = "var(--qz-accent)")}
              onBlur={(e) => (e.currentTarget.style.borderColor = "var(--qz-border)")}
            />
          </div>

          <div>
            <label className="block text-[12px] text-[var(--qz-fg-3)] mb-[6px]">Password</label>
            <div className="relative">
              <input
                type={showPassword ? "text" : "password"}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="Enter your password"
                autoComplete="current-password"
                className={`${inputBase} pr-10`}
                style={{
                  background: "var(--qz-input-bg)",
                  border: "1px solid var(--qz-border)",
                }}
                onFocus={(e) => (e.currentTarget.style.borderColor = "var(--qz-accent)")}
                onBlur={(e) => (e.currentTarget.style.borderColor = "var(--qz-border)")}
              />
              <button
                type="button"
                onClick={() => setShowPassword((v) => !v)}
                aria-label={showPassword ? "Hide password" : "Show password"}
                className="absolute right-3 top-1/2 -translate-y-1/2 text-[var(--qz-fg-4)] hover:text-[var(--qz-fg-2)] transition-colors cursor-pointer bg-transparent border-0 p-0"
              >
                <EyeIcon off={showPassword} />
              </button>
            </div>
          </div>

          <div>
            <label className="block text-[12px] text-[var(--qz-fg-3)] mb-[6px]">Realm</label>
            <RealmSelect realms={realms} value={realm} onChange={setRealm} />
          </div>

          {error && (
            <p className="text-[12px] m-0" style={{ color: "var(--qz-danger)" }}>
              {error}
            </p>
          )}

          <button
            type="submit"
            disabled={loading}
            className="w-full rounded-md py-[10px] text-[13.5px] font-semibold transition-opacity cursor-pointer border-0 mt-1"
            style={{
              background: "var(--qz-accent)",
              color: "var(--qz-fg-on-accent)",
              opacity: loading ? 0.7 : 1,
            }}
          >
            {loading ? "Signing in…" : "Sign in"}
          </button>
        </form>
      </div>
    </div>
  );
}
