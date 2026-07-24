"use client";

import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { AlertTriangle, ArrowRight } from "lucide-react";
import { PageHeader } from "@/components/PageHeader";
import { ApiError } from "@/lib/authClient";
import {
  fetchInterfaces,
  fetchPending,
  type LinkKind,
  type NodeInterfaces,
  type PendingResponse,
} from "@/lib/networkClient";

const POLL_MS = 5000;

const KINDS: { kind: LinkKind; label: string }[] = [
  { kind: "ethernet", label: "Adapters" },
  { kind: "bridge", label: "Bridges" },
  { kind: "bond", label: "Bonds" },
  { kind: "vlan", label: "VLANs" },
];

/// What the networking subsystem actually knows, per node. Deliberately no
/// topology diagram and no traffic charts: nothing behind this page produces
/// either yet, and a placeholder that looks like data is worse than a
/// sentence saying there is none.
export default function NetworkingOverviewPage() {
  const [nodes, setNodes] = useState<NodeInterfaces[] | null>(null);
  const [pending, setPending] = useState<PendingResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [interfaces, staged] = await Promise.all([fetchInterfaces(), fetchPending()]);
      setNodes(interfaces.nodes);
      setPending(staged);
      setError(null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the network configuration.");
    }
  }, []);

  useEffect(() => {
    void load();
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

  return (
    <div className="p-[28px_36px]">
      <PageHeader
        title="Networking Overview"
        description="What this appliance's networking looks like right now."
      />

      {error && (
        <div className="callout callout-crit mb-4">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
        </div>
      )}

      {nodes === null && !error && (
        <div className="surface px-6 py-12 text-center text-[13px] text-[var(--qz-fg-4)]">
          Reading the network configuration…
        </div>
      )}

      {nodes?.map((node) => {
        const management = node.interfaces.find((link) => link.management) ?? null;
        const uplinks = node.interfaces.filter((link) => link.kind === "ethernet");
        const withCarrier = uplinks.filter((link) => link.carrier).length;
        const stagedHere = pending?.changes.length ?? 0;

        return (
          <section key={node.node} className="mb-8">
            <h2 className="text-[13px] font-semibold text-[var(--qz-fg-2)] mb-2 qz-mono">
              {node.node}
            </h2>

            <div
              className="grid gap-4"
              style={{ gridTemplateColumns: "repeat(auto-fit, minmax(200px, 1fr))" }}
            >
              {KINDS.map(({ kind, label }) => (
                <Stat
                  key={kind}
                  label={label}
                  value={node.interfaces.filter((link) => link.kind === kind).length.toString()}
                />
              ))}
              <Stat label="Adapters with carrier" value={`${withCarrier} of ${uplinks.length}`} />
            </div>

            <div className="surface mt-4 p-5">
              <div className="kpi-label mb-3">Management</div>
              {management ? (
                <div className="flex flex-wrap items-center gap-x-8 gap-y-3">
                  <Fact label="Interface" value={management.name} />
                  <Fact
                    label="Address"
                    value={management.addresses.join(", ") || "none reported"}
                  />
                  <Fact label="Gateway" value={management.gateway ?? "—"} />
                  <Fact label="Hardware address" value={management.mac ?? "—"} />
                  <div className="flex items-center gap-2">
                    <span className={`badge ${management.kind === "bridge" ? "badge-ok" : "badge-warn"}`}>
                      {management.kind === "bridge" ? "bridged" : "not bridged"}
                    </span>
                    {management.kind !== "bridge" && (
                      <Link
                        href="/networking/interfaces"
                        className="text-[13px] text-[var(--qz-accent)] no-underline inline-flex items-center gap-1"
                      >
                        Virtual machines need a bridge — convert it
                        <ArrowRight size={13} />
                      </Link>
                    )}
                  </div>
                </div>
              ) : (
                <p className="text-[13px] text-[var(--qz-fg-4)] m-0">
                  No interface on this node reports a management address.
                </p>
              )}
            </div>

            <div className="surface mt-4 p-5 flex items-center gap-4">
              <div className="flex-1">
                <div className="kpi-label mb-1">Staged changes</div>
                <div className="text-[13px] text-[var(--qz-fg-3)]">
                  {stagedHere === 0
                    ? "Nothing is waiting to be applied."
                    : `${stagedHere} ${stagedHere === 1 ? "change is" : "changes are"} staged and not yet applied.`}
                </div>
              </div>
              <Link
                href="/networking/interfaces"
                className="text-[13px] text-[var(--qz-accent)] no-underline inline-flex items-center gap-1"
              >
                Interfaces
                <ArrowRight size={13} />
              </Link>
            </div>
          </section>
        );
      })}

      {/* Same shape and tone as StubPage — the sections that do not exist yet
          should read the way every other unimplemented section does. */}
      <div className="surface px-6 py-12 text-center">
        <div className="text-[14px] font-semibold text-[var(--qz-fg-2)]">Not implemented yet</div>
        <p className="text-[13px] text-[var(--qz-fg-4)] mt-2 mb-0">
          Fabrics, routing, and tunnels are placeholders — their summaries land here as those
          subsystems arrive.
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

function Fact({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <div className="kpi-label">{label}</div>
      <div
        className="text-[13px] text-[var(--qz-fg-1)] truncate mt-[3px]"
        style={{ fontFamily: "var(--qz-font-mono)" }}
        title={value}
      >
        {value}
      </div>
    </div>
  );
}
