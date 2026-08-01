"use client";

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle } from "lucide-react";
import { Button } from "@/components/ui/Button";
import { SelectInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { shortNodeName } from "@/lib/nodeNames";
import {
  adoptNic,
  fetchNicPins,
  type PinReport,
  type UnclaimedNic,
} from "@/lib/networkClient";

/// One member the banner asks about. The pins are read on the member and an
/// adoption is forwarded to it, so the repair works wherever the card was
/// swapped — not only on the node serving the console.
export interface PinMember {
  node: string;
  local: boolean;
}

interface MemberReport {
  node: string;
  local: boolean;
  report: PinReport;
}

/// The banner a replaced network card earns.
///
/// Lumen pins each adapter's name to its permanent address, and every
/// profile — a bond, the Core network, a cluster ring — is written against
/// that name. Swap the card and the name survives while its hardware does
/// not: NetworkManager reports "no suitable device found", the bond comes
/// up with no ports, and nothing on the page explains why.
///
/// So the page says it, and offers the one repair — for every member the
/// environment can reach, because the node with the swapped card is rarely
/// the one whose console happens to be open. It is a choice rather than an
/// automatic adoption because a name is a promise about which *cable*: the
/// appliance cannot tell which port of a new card carries the network the
/// old one did, and picking wrong moves storage replication somewhere
/// nobody asked for. The facts to choose by — link, speed, driver — are on
/// the row.
export function OrphanedNics({
  members,
  onAdopted,
}: {
  members: PinMember[];
  onAdopted: () => Promise<void>;
}) {
  const [reports, setReports] = useState<MemberReport[]>([]);
  const [chosen, setChosen] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);

  // The member list arrives as a fresh array every render; keying the
  // effect on its contents keeps the reads from looping.
  const memberKey = members.map((m) => `${m.node}:${m.local}`).join(",");

  const load = useCallback(async () => {
    const wanted = memberKey
      .split(",")
      .filter(Boolean)
      .map((entry) => {
        const [node, local] = entry.split(":");
        return { node, local: local === "true" };
      });
    const settled = await Promise.allSettled(
      wanted.map((member) => fetchNicPins(member.local ? undefined : member.node)),
    );
    // A member whose pins cannot be read is not a member with a problem to
    // report here; the rest of the banner still works.
    setReports(
      wanted.flatMap((member, index) => {
        const answer = settled[index];
        return answer.status === "fulfilled"
          ? [{ ...member, report: answer.value }]
          : [];
      }),
    );
  }, [memberKey]);

  useEffect(() => {
    load().catch((err) => {
      if (err instanceof ApiError && err.status === 401) return;
      setReports([]);
    });
  }, [load]);

  const orphaned = reports.filter((entry) => entry.report.orphaned.length > 0);
  if (orphaned.length === 0) return null;
  const several = reports.length > 1;

  const label = (adapter: UnclaimedNic) =>
    `${adapter.device} — ${adapter.mac}${
      adapter.carrier
        ? `, link up${adapter.speed_mbps ? ` at ${adapter.speed_mbps} Mb/s` : ""}`
        : ", no link"
    }${adapter.driver ? ` (${adapter.driver})` : ""}`;

  const adopt = async (entry: MemberReport, slot: number) => {
    const key = `${entry.node}/${slot}`;
    const mac = chosen[key] ?? entry.report.unclaimed[0]?.mac;
    if (!mac) return;
    setBusy(key);
    setMessage(null);
    try {
      const answer = await adoptNic(slot, mac, entry.local ? undefined : entry.node);
      setMessage(
        answer.note ??
          `${answer.device} is now ${answer.adopted}${
            several ? ` on ${shortNodeName(entry.node)}` : ""
          }. Profiles built on that name work again.`,
      );
      await load();
      await onAdopted();
    } catch (err) {
      setMessage(err instanceof Error ? err.message : "The adoption was refused.");
    } finally {
      setBusy(null);
    }
  };

  const names = orphaned.flatMap((entry) =>
    entry.report.orphaned.map((pin) => ({ node: entry.node, slot: pin.slot })),
  );

  return (
    <div className="callout callout-warn">
      <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
      <div className="flex flex-col gap-3 flex-1 min-w-0">
        <div className="text-[13px] text-[var(--qz-fg-2)]">
          <strong>
            {names.length === 1
              ? "A network name has lost its card"
              : "Network names have lost their cards"}
          </strong>{" "}
          — everything built on{" "}
          {names.map((name, index) => (
            <span key={`${name.node}/${name.slot}`}>
              {index > 0 && ", "}
              <span className="qz-mono">nic{name.slot}</span>
              {several ? ` (${shortNodeName(name.node)})` : ""}
            </span>
          ))}{" "}
          is bound to hardware that is no longer in {several ? "that node" : "this node"}. Give
          the name to the card that replaced it, and the profiles above it work again — no
          rebuilding.
        </div>

        {orphaned.map((entry) =>
          entry.report.unclaimed.length === 0 ? (
            <div key={entry.node} className="text-[13px] text-[var(--qz-fg-3)]">
              {several ? `${shortNodeName(entry.node)}: no` : "No"} unclaimed adapter is present
              to adopt. Fit the replacement card, or remove the profiles that name these.
            </div>
          ) : (
            entry.report.orphaned.map((pin) => (
              <div
                key={`${entry.node}/${pin.slot}`}
                className="flex items-center gap-2 flex-wrap"
              >
                <span
                  className="qz-mono text-[13px] text-[var(--qz-fg-1)]"
                  style={{ minWidth: 62 }}
                >
                  nic{pin.slot}
                </span>
                <span className="text-[12px] text-[var(--qz-fg-4)]" style={{ minWidth: 190 }}>
                  {several ? `on ${shortNodeName(entry.node)}, ` : ""}was {pin.mac}
                  {pin.altname ? ` (${pin.altname})` : ""}
                </span>
                <span style={{ minWidth: 320, flex: 1 }}>
                  <SelectInput
                    id={`adopt-${entry.node}-${pin.slot}`}
                    mono
                    value={
                      chosen[`${entry.node}/${pin.slot}`] ??
                      entry.report.unclaimed[0]?.mac ??
                      ""
                    }
                    onChange={(value) =>
                      setChosen({ ...chosen, [`${entry.node}/${pin.slot}`]: value })
                    }
                  >
                    {entry.report.unclaimed.map((adapter) => (
                      <option key={adapter.mac} value={adapter.mac}>
                        {label(adapter)}
                      </option>
                    ))}
                  </SelectInput>
                </span>
                <Button
                  kind="secondary"
                  size="sm"
                  disabled={busy !== null}
                  onClick={() => void adopt(entry, pin.slot)}
                >
                  {busy === `${entry.node}/${pin.slot}`
                    ? "Adopting…"
                    : `Adopt as nic${pin.slot}`}
                </Button>
              </div>
            ))
          ),
        )}

        {message && <div className="text-[12px] text-[var(--qz-fg-3)]">{message}</div>}
      </div>
    </div>
  );
}
