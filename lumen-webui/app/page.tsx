"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import * as api from "@/lib/authClient";
import type { AuthUserInfo } from "@/lib/authClient";

/// Console landing — a placeholder until the hypervisor views land. Confirms
/// the session with the backend and bounces unauthenticated visitors to
/// /login (the cookie is httpOnly, so the server is the only judge).
export default function ConsolePage() {
  const router = useRouter();
  const [user, setUser] = useState<AuthUserInfo | null>(null);

  useEffect(() => {
    api
      .fetchMe()
      .then(setUser)
      .catch(() => router.replace("/login"));
  }, [router]);

  if (!user) return null;

  const handleLogout = async () => {
    await api.logout();
    router.replace("/login");
  };

  return (
    <div
      className="min-h-screen flex flex-col items-center justify-center gap-4"
      style={{ background: "var(--qz-bg)" }}
    >
      <div
        className="w-full max-w-[480px] rounded-xl p-8 flex flex-col gap-4"
        style={{
          background: "var(--qz-ink-0)",
          border: "1px solid var(--qz-border)",
          boxShadow: "var(--qz-shadow-3)",
        }}
      >
        <div className="flex items-center gap-3">
          <img src="/logo-mark.png" alt="Lumen" className="w-9 h-9" />
          <h1
            className="text-[22px] font-bold text-[var(--qz-fg-1)] m-0"
            style={{ letterSpacing: "-0.02em" }}
          >
            Lumen
          </h1>
        </div>
        <p className="text-[13px] m-0 text-[var(--qz-fg-2)]">
          Signed in as{" "}
          <span className="font-semibold text-[var(--qz-fg-1)]">
            {user.username}@{user.realm}
          </span>
          . The hypervisor console lands here.
        </p>
        <button
          onClick={handleLogout}
          className="self-start rounded-md px-4 py-[8px] text-[13px] font-semibold transition-colors cursor-pointer"
          style={{
            background: "var(--qz-input-bg)",
            border: "1px solid var(--qz-border)",
            color: "var(--qz-fg-1)",
          }}
        >
          Sign out
        </button>
      </div>
    </div>
  );
}
