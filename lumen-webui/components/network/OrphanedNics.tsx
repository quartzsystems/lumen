"use client";

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle } from "lucide-react";
import { Button } from "@/components/ui/Button";
import { SelectInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import {
  adoptNic,
  fetchNicPins,
  type PinReport,
  type UnclaimedNic,
} from "@/lib/networkClient";

/// The banner a replaced network card earns.
///
/// Lumen pins each adapter's name to its permanent address, and every
/// profile — a bond, the Core network, a cluster ring — is written against
/// that name. Swap the card and the name survives while its hardware does
/// not: NetworkManager reports "no suitable device found", the bond comes
/// up with no ports, and nothing on the page explains why.
///
/// So the page says it, and offers the one repair. It is a choice rather
/// than an automatic adoption because a name is a promise about which
/// *cable*: the appliance cannot tell which port of a new card carries the
/// network the old one did, and picking wrong moves storage replication
/// somewhere nobody asked for. The facts to choose by — link, speed,
/// driver — are on the row.
export function OrphanedNics({ onAdopted }: { onAdopted: () => Promise<void> }) {
  const [report, setReport] = useState<PinReport | null>(null);
  const [chosen, setChosen] = useState<Record<number, string>>({});
  const [busy, setBusy] = useState<number | null>(null);
  const [message, setMessage] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setReport(await fetchNicPins());
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      // A node whose pins cannot be read is not a node with a problem to
      // report here; the rest of the page still works.
      setReport(null);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  if (!report || report.orphaned.length === 0) return null;

  const label = (adapter: UnclaimedNic) =>
    `${adapter.device} — ${adapter.mac}${
      adapter.carrier
        ? `, link up${adapter.speed_mbps ? ` at ${adapter.speed_mbps} Mb/s` : ""}`
        : ", no link"
    }${adapter.driver ? ` (${adapter.driver})` : ""}`;

  const adopt = async (slot: number) => {
    const mac = chosen[slot] ?? report.unclaimed[0]?.mac;
    if (!mac) return;
    setBusy(slot);
    setMessage(null);
    try {
      const answer = await adoptNic(slot, mac);
      setMessage(
        answer.note ??
          `${answer.device} is now ${answer.adopted}. Profiles built on that name work again.`,
      );
      await load();
      await onAdopted();
    } catch (err) {
      setMessage(err instanceof Error ? err.message : "The adoption was refused.");
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="callout callout-warn">
      <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
      <div className="flex flex-col gap-3 flex-1 min-w-0">
        <div className="text-[13px] text-[var(--qz-fg-2)]">
          <strong>
            {report.orphaned.length === 1
              ? "A network name has lost its card"
              : "Network names have lost their cards"}
          </strong>{" "}
          — everything built on{" "}
          {report.orphaned.map((pin, index) => (
            <span key={pin.slot}>
              {index > 0 && ", "}
              <span className="qz-mono">nic{pin.slot}</span>
            </span>
          ))}{" "}
          is bound to hardware that is no longer in this node. Give the name to the card that
          replaced it, and the profiles above it work again — no rebuilding.
        </div>

        {report.unclaimed.length === 0 ? (
          <div className="text-[13px] text-[var(--qz-fg-3)]">
            No unclaimed adapter is present to adopt. Fit the replacement card, or remove the
            profiles that name these.
          </div>
        ) : (
          report.orphaned.map((pin) => (
            <div key={pin.slot} className="flex items-center gap-2 flex-wrap">
              <span className="qz-mono text-[13px] text-[var(--qz-fg-1)]" style={{ minWidth: 62 }}>
                nic{pin.slot}
              </span>
              <span className="text-[12px] text-[var(--qz-fg-4)]" style={{ minWidth: 190 }}>
                was {pin.mac}
                {pin.altname ? ` (${pin.altname})` : ""}
              </span>
              <span style={{ minWidth: 320, flex: 1 }}>
                <SelectInput
                  id={`adopt-${pin.slot}`}
                  mono
                  value={chosen[pin.slot] ?? report.unclaimed[0]?.mac ?? ""}
                  onChange={(value) => setChosen({ ...chosen, [pin.slot]: value })}
                >
                  {report.unclaimed.map((adapter) => (
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
                onClick={() => void adopt(pin.slot)}
              >
                {busy === pin.slot ? "Adopting…" : `Adopt as nic${pin.slot}`}
              </Button>
            </div>
          ))
        )}

        {message && <div className="text-[12px] text-[var(--qz-fg-3)]">{message}</div>}
      </div>
    </div>
  );
}
