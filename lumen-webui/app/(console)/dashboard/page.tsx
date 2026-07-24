"use client";

import { useEffect, useState } from "react";
import { PageHeader } from "@/components/PageHeader";
import { getCurrentUser } from "@/lib/authClient";
import type { AuthUserInfo } from "@/lib/authClient";
import { fetchVersion } from "@/lib/system";

/// Console landing page. Hypervisor tiles (guests, hosts, pools, alerts) land
/// here as those subsystems arrive; for now it reports what the control plane
/// already knows: who is signed in and which build is running.
export default function DashboardPage() {
  const [user, setUser] = useState<AuthUserInfo | null>(null);
  const [version, setVersion] = useState<string | null>(null);

  useEffect(() => {
    setUser(getCurrentUser());
    fetchVersion()
      .then((v) => setVersion(v.version))
      .catch(() => setVersion(null));
  }, []);

  return (
    <div className="p-[28px_36px]">
      <PageHeader title="Dashboard" description="Appliance status at a glance." />

      <div className="grid gap-4" style={{ gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))" }}>
        <Stat label="Signed in as" value={user ? `${user.username}@${user.realm}` : "—"} />
        <Stat label="Control plane" value={version ?? "unavailable"} />
        <Stat label="Guests" value="—" />
      </div>

      <div className="surface mt-6 px-6 py-12 text-center">
        <div className="text-[14px] font-semibold text-[var(--qz-fg-2)]">
          Hypervisor telemetry not wired up yet
        </div>
        <p className="text-[13px] text-[var(--qz-fg-4)] mt-2 mb-0">
          Guest, host, and storage tiles land here as those subsystems arrive.
        </p>
      </div>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="kpi-card">
      <div className="kpi-label">{label}</div>
      <div
        className="mt-[6px] text-[20px] font-semibold leading-none text-[var(--qz-fg-1)] truncate"
        style={{ fontFamily: "var(--qz-font-mono)" }}
        title={value}
      >
        {value}
      </div>
    </div>
  );
}
