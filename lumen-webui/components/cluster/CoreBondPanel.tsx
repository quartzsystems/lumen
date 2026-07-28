"use client";

import { useState } from "react";
import { CheckList, CheckRow, ErrorText, Field, SelectInput, TextInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { bondNodeNics, type BondNodeRequest, type PreflightLink } from "@/lib/clusterClient";

/// The create wizard's shortcut to a Core seat that survives a cable.
///
/// It does not invent a second way to own a link: the request lands in the
/// target node's own networking domain, so the bond that comes out is an
/// ordinary one — it shows up in that node's Networking page, is edited and
/// deleted there, and a cluster teardown leaves it alone. This panel only
/// saves the operator a trip to each node's console before the wizard.
///
/// Only unaddressed ethernet links are offered as ports. A bond takes its
/// ports over entirely, so enslaving the link the console answers on would
/// cut the operator off mid-wizard — the node refuses that too, but it should
/// not be offered in the first place.

const MODES: { value: BondNodeRequest["mode"]; label: string; hint: string }[] = [
  {
    value: "active-backup",
    label: "Active/backup",
    hint: "One port carries traffic, the other waits. Needs nothing from the switch.",
  },
  {
    value: "802.3ad",
    label: "LACP (802.3ad)",
    hint: "Both ports carry traffic. The switch must have a matching port channel.",
  },
  {
    value: "balance-xor",
    label: "Balance XOR",
    hint: "Both ports carry traffic, hashed. Needs static aggregation on the switch.",
  },
];

/// A free bond name on this node: bond0, else bond1, and so on.
const proposeName = (links: PreflightLink[]): string => {
  const taken = new Set(links.map((link) => link.name));
  for (let n = 0; n < 16; n += 1) {
    if (!taken.has(`bond${n}`)) return `bond${n}`;
  }
  return "bond0";
};

export function CoreBondPanel({
  node,
  links,
  onBuilt,
}: {
  node: string;
  /// Every link the node reported, so the name proposal avoids all of them.
  links: PreflightLink[];
  /// The bond exists now. The caller re-preflights and seats Core on it.
  onBuilt: (name: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [mode, setMode] = useState<BondNodeRequest["mode"]>("active-backup");
  const [ports, setPorts] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const candidates = links.filter(
    (link) =>
      link.kind === "ethernet" &&
      link.addresses.length === 0 &&
      // A link answers to one controller: a NIC already in a bridge or an
      // earlier bond cannot be enslaved twice.
      !link.controller,
  );

  const reveal = () => {
    setName(proposeName(links));
    setPorts([]);
    setError("");
    setOpen(true);
  };

  const toggle = (port: string) =>
    setPorts((current) =>
      current.includes(port) ? current.filter((p) => p !== port) : [...current, port],
    );

  const build = async () => {
    setBusy(true);
    setError("");
    try {
      await bondNodeNics(node, { name: name.trim(), mode, ports, miimon: 100 });
      setOpen(false);
      onBuilt(name.trim());
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The bond could not be built.");
    } finally {
      setBusy(false);
    }
  };

  if (!open) {
    return (
      <button
        type="button"
        onClick={reveal}
        disabled={candidates.length < 2}
        className="self-start text-[12px] underline cursor-pointer bg-transparent border-0 p-0"
        style={{
          color: candidates.length < 2 ? "var(--qz-fg-4)" : "var(--qz-accent)",
          cursor: candidates.length < 2 ? "not-allowed" : "pointer",
        }}
      >
        {candidates.length < 2
          ? "Bond NICs for Core — needs two free NICs on this node"
          : "Bond NICs for Core…"}
      </button>
    );
  }

  return (
    <div className="surface p-3 flex flex-col gap-3">
      <div className="text-[12px] text-[var(--qz-fg-3)]">
        Built on {node} through its own networking, before the cluster exists. It becomes an
        ordinary link there — edited and deleted from that node&rsquo;s Networking page, and left
        alone by a cluster teardown.
      </div>
      <div className="grid grid-cols-2 gap-3">
        <Field label="Bond name" required>
          <TextInput mono value={name} onChange={setName} />
        </Field>
        <Field label="Mode" hint={MODES.find((m) => m.value === mode)?.hint}>
          <SelectInput value={mode} onChange={(v) => setMode(v as BondNodeRequest["mode"])}>
            {MODES.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
          </SelectInput>
        </Field>
      </div>
      <Field
        label="Ports"
        required
        hint="Two or more. Only NICs with no addressing — a bond takes its ports over entirely."
      >
        <CheckList>
          {candidates.map((link) => (
            <CheckRow key={link.name} checked={ports.includes(link.name)} onChange={() => toggle(link.name)}>
              <span className="qz-mono text-[13px] text-[var(--qz-fg-2)]">
                {link.name}
                {link.carrier ? "" : " — no carrier"}
              </span>
            </CheckRow>
          ))}
        </CheckList>
      </Field>
      <ErrorText msg={error} />
      <div className="flex gap-2 justify-end">
        <button
          type="button"
          onClick={() => setOpen(false)}
          disabled={busy}
          className="px-3 py-[7px] rounded-md text-[12px] cursor-pointer"
          style={{
            background: "transparent",
            border: "1px solid var(--qz-border)",
            color: "var(--qz-fg-2)",
          }}
        >
          Cancel
        </button>
        <button
          type="button"
          onClick={build}
          disabled={busy || ports.length < 2 || name.trim() === ""}
          className="px-3 py-[7px] rounded-md text-[12px] font-semibold cursor-pointer border-0"
          style={{
            background: "var(--qz-accent)",
            color: "var(--qz-fg-on-accent)",
            opacity: busy || ports.length < 2 || name.trim() === "" ? 0.7 : 1,
          }}
        >
          {busy ? "Building…" : "Build bond"}
        </button>
      </div>
    </div>
  );
}
