"use client";

import { useMemo, useState } from "react";
import {
  createBond,
  createBridge,
  createVlan,
  updateBond,
  updateBridge,
  updateNic,
  updateVlan,
  validationErrorsOf,
  type BondMode,
  type Duplex,
  type IpConfig,
  type LinkKind,
  type LinkView,
  type PendingResponse,
} from "@/lib/networkClient";

export type DialogKind = "bridge" | "bond" | "vlan" | "ethernet";

/// How a link is addressed. The three arms of the backend's `IpConfig`, in the
/// words the form uses.
type IpMode = "none" | "dhcp" | "static";

/// Every field the four link dialogs between them collect. Kept as strings so
/// a half-typed number is a normal input state rather than NaN.
interface Draft {
  name: string;
  comment: string;
  ipMode: IpMode;
  cidr: string;
  gateway: string;
  ports: string[];
  vlanAware: boolean;
  stp: boolean;
  forwardDelay: string;
  mtu: string;
  mode: BondMode;
  miimon: string;
  lacpRate: "" | "slow" | "fast";
  hashPolicy: "" | "layer2" | "layer2+3" | "layer3+4";
  primary: string;
  parent: string;
  vlanId: string;
  autoneg: "" | "on" | "off";
  speed: string;
  duplex: "" | Duplex;
}

const emptyDraft = (): Draft => ({
  name: "",
  comment: "",
  ipMode: "none",
  cidr: "",
  gateway: "",
  ports: [],
  vlanAware: false,
  stp: false,
  forwardDelay: "",
  mtu: "",
  mode: "active-backup",
  miimon: "",
  lacpRate: "",
  hashPolicy: "",
  primary: "",
  parent: "",
  vlanId: "",
  autoneg: "",
  speed: "",
  duplex: "",
});

const draftFrom = (link: LinkView): Draft => ({
  ...emptyDraft(),
  name: link.name,
  comment: link.comment ?? "",
  ipMode: link.ip.mode === "disabled" ? "none" : link.ip.mode,
  cidr: link.ip.mode === "static" ? link.ip.cidr : "",
  gateway: link.ip.mode === "static" ? link.ip.gateway : "",
  ports: [...link.ports],
  vlanAware: link.vlan_aware,
  mtu: link.mtu?.toString() ?? "",
  mode: link.bond_mode ?? "active-backup",
  parent: link.parent ?? "",
  vlanId: link.vlan_id?.toString() ?? "",
});

const TITLES: Record<DialogKind, string> = {
  bridge: "Linux Bridge",
  bond: "Linux Bond",
  vlan: "Linux VLAN",
  ethernet: "Network Interface",
};

/// 1–15 characters, no whitespace, slash, or colon — the same rule the backend
/// enforces (lumen-net `valid_ifname`), checked here so the obvious mistakes
/// never become a round trip.
const validName = (name: string): boolean =>
  name.length > 0 && name.length <= 15 && !/[\s/:]/.test(name);

/// Dotted quad, each octet 0–255.
const validIpv4 = (value: string): boolean =>
  /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.test(value) &&
  value.split(".").every((octet) => Number(octet) <= 255);

/// "192.168.1.10/24" — an address and a prefix length in the same field, which
/// is how an operator reads one off a network diagram.
const validCidr = (value: string): boolean => {
  const [address, prefix, ...rest] = value.split("/");
  if (rest.length > 0 || prefix === undefined) return false;
  const length = Number(prefix);
  return (
    validIpv4(address) && /^\d{1,2}$/.test(prefix) && Number.isInteger(length) && length <= 32
  );
};

const numberOrUndefined = (text: string): number | undefined => {
  const trimmed = text.trim();
  if (trimmed === "") return undefined;
  const value = Number(trimmed);
  return Number.isFinite(value) ? value : undefined;
};

const ipConfigOf = (draft: Draft): IpConfig => {
  if (draft.ipMode === "dhcp") return { mode: "dhcp" };
  if (draft.ipMode === "static") {
    return { mode: "static", cidr: draft.cidr.trim(), gateway: draft.gateway.trim() };
  }
  return { mode: "disabled" };
};

/// Create or edit one link.
///
/// All four kinds share one form: name, addressing, MTU, and a comment are in
/// the same place every time, with only the middle — ports, VLAN id, bond
/// options — changing. Server-side validation errors come back with the field
/// they belong to, so they land under the input that caused them instead of in
/// a banner at the top.
export function LinkDialog({
  kind,
  editing,
  links,
  managementLink,
  onClose,
  onSaved,
}: {
  kind: DialogKind;
  /// The row being edited, or null when creating.
  editing: LinkView | null;
  /// Every link on the node, for the port and parent pickers.
  links: LinkView[];
  managementLink: string | null;
  onClose: () => void;
  onSaved: (pending: PendingResponse) => void;
}) {
  const [draft, setDraft] = useState<Draft>(() => (editing ? draftFrom(editing) : emptyDraft()));
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);
  const set = <K extends keyof Draft>(key: K, value: Draft[K]) =>
    setDraft((d) => ({ ...d, [key]: value }));

  // A candidate port is any link that is free, or already ours. The
  // management link is deliberately absent: enslaving it without moving the
  // address is the one change that takes the node off the network, so it has
  // its own guarded operation (the banner on the Interfaces page).
  const portCandidates = useMemo(
    () =>
      links.filter(
        (link) =>
          link.name !== draft.name &&
          link.name !== managementLink &&
          link.kind !== "other" &&
          link.change !== "deleted" &&
          (link.controller === null || link.controller === editing?.name),
      ),
    [links, draft.name, managementLink, editing],
  );

  const parentCandidates = useMemo(
    () => links.filter((link) => link.kind !== "vlan" && link.kind !== "other"),
    [links],
  );

  const validate = (): boolean => {
    const found: Record<string, string> = {};
    if (!editing && !validName(draft.name)) {
      found.name = "Use up to 15 characters, with no spaces, slashes, or colons.";
    }
    if (kind === "vlan") {
      const id = numberOrUndefined(draft.vlanId);
      if (id === undefined || id < 1 || id > 4094) found.vlan_id = "Use a VLAN id from 1 to 4094.";
      if (!draft.parent) found.parent = "Choose the interface this VLAN rides on.";
    }
    if (kind === "bond" && draft.ports.length === 0) {
      found.ports = "A bond needs at least one port.";
    }
    if (draft.ipMode === "static") {
      if (!validCidr(draft.cidr.trim())) {
        found.ip = "Use an address and prefix length, like 192.168.1.10/24.";
      }
      // A gateway is optional — a storage or migration network has none — but
      // a malformed one is a mistake, not a choice.
      if (draft.gateway.trim() !== "" && !validIpv4(draft.gateway.trim())) {
        found.gateway = "Use a plain IPv4 address, like 192.168.1.1.";
      }
    }
    if (draft.mtu.trim() !== "") {
      const mtu = numberOrUndefined(draft.mtu);
      if (mtu === undefined || mtu < 576 || mtu > 65536) found.mtu = "Use an MTU from 576 to 65536.";
    }
    setErrors(found);
    return Object.keys(found).length === 0;
  };

  const submit = async () => {
    if (!validate()) return;
    setSaving(true);
    try {
      const mtu = numberOrUndefined(draft.mtu);
      const common = { ip: ipConfigOf(draft), comment: draft.comment.trim(), mtu };
      let pending: PendingResponse;
      if (kind === "bridge") {
        const body = {
          ...common,
          ports: draft.ports,
          stp: draft.stp,
          forward_delay: numberOrUndefined(draft.forwardDelay),
          vlan_filtering: draft.vlanAware,
        };
        pending = editing
          ? await updateBridge(editing.name, body)
          : await createBridge({ name: draft.name, ...body });
      } else if (kind === "bond") {
        const body = {
          ...common,
          mode: draft.mode,
          ports: draft.ports,
          miimon: numberOrUndefined(draft.miimon),
          lacp_rate: draft.lacpRate || undefined,
          xmit_hash_policy: draft.hashPolicy || undefined,
          primary: draft.primary || undefined,
        };
        pending = editing
          ? await updateBond(editing.name, body)
          : await createBond({ name: draft.name, ...body });
      } else if (kind === "vlan") {
        const body = {
          ...common,
          parent: draft.parent,
          vlan_id: numberOrUndefined(draft.vlanId) ?? 0,
        };
        pending = editing
          ? await updateVlan(editing.name, body)
          : await createVlan({ name: draft.name, ...body });
      } else {
        // A physical NIC is configured, never created.
        pending = await updateNic(editing!.name, {
          ...common,
          autoneg: draft.autoneg === "" ? undefined : draft.autoneg === "on",
          speed: draft.autoneg === "off" ? numberOrUndefined(draft.speed) : undefined,
          duplex: draft.autoneg === "off" && draft.duplex ? draft.duplex : undefined,
        });
      }
      onSaved(pending);
    } catch (err) {
      const detail = validationErrorsOf(err);
      if (detail.length > 0) {
        // Pin each rejection to the input that caused it, using the codes the
        // backend guarantees are stable.
        const found: Record<string, string> = {};
        for (const item of detail) {
          found[item.field ?? "form"] = item.message;
        }
        setErrors(found);
      } else {
        setErrors({ form: err instanceof Error ? err.message : "Something went wrong." });
      }
    } finally {
      setSaving(false);
    }
  };

  const togglePort = (name: string) =>
    setDraft((d) => ({
      ...d,
      ports: d.ports.includes(name) ? d.ports.filter((p) => p !== name) : [...d.ports, name],
    }));

  const showPorts = kind === "bridge" || kind === "bond";
  const portLabel = kind === "bridge" ? "Bridge ports" : "Bond ports";
  // A port hands its addressing to its controller, so offering the fields
  // would be offering a setting the box discards.
  const isPort = editing?.controller != null;

  return (
    <div className="dialog-scrim" role="dialog" aria-modal="true">
      <div className="dialog">
        <h2 className="dialog-title">
          {editing ? `Edit ${editing.name}` : `Create ${TITLES[kind]}`}
        </h2>
        <p className="dialog-subtitle">
          {editing
            ? "Changes are staged. Nothing reaches the node until you apply them."
            : "The new interface is staged. Nothing reaches the node until you apply it."}
        </p>

        <div className="flex flex-col gap-4 mt-5">
          <div className="field">
            <label className="field-label" htmlFor="link-name">
              Name
            </label>
            <input
              id="link-name"
              className={`input${errors.name ? " input-invalid" : ""}`}
              value={draft.name}
              // An interface cannot be renamed in place — the profile, the
              // ports pointing at it, and the running device all carry the
              // name — so editing shows it rather than offering it.
              readOnly={editing !== null}
              autoFocus={editing === null}
              placeholder={kind === "bridge" ? "br1" : kind === "bond" ? "bond0" : "vlan100"}
              onChange={(e) => set("name", e.target.value)}
            />
            {errors.name && <span className="field-error">{errors.name}</span>}
          </div>

          {kind === "ethernet" && editing && (
            <div className="field">
              <label className="field-label" htmlFor="link-altname">
                Alternative name
              </label>
              <input
                id="link-altname"
                className="input"
                value={editing.altname ?? ""}
                placeholder="—"
                readOnly
              />
              <span className="field-hint">
                What the kernel called this adapter before Lumen pinned it to{" "}
                <span className="qz-mono">{editing.name}</span>.
              </span>
            </div>
          )}

          {kind === "vlan" && (
            <>
              <div className="field">
                <label className="field-label" htmlFor="vlan-parent">
                  Parent interface
                </label>
                <select
                  id="vlan-parent"
                  className={`select${errors.parent ? " select-invalid" : ""}`}
                  value={draft.parent}
                  onChange={(e) => set("parent", e.target.value)}
                >
                  <option value="">Choose an interface…</option>
                  {parentCandidates.map((link) => (
                    <option key={link.name} value={link.name}>
                      {link.name} ({link.kind})
                    </option>
                  ))}
                </select>
                {errors.parent && <span className="field-error">{errors.parent}</span>}
              </div>
              <div className="field">
                <label className="field-label" htmlFor="vlan-id">
                  VLAN id
                </label>
                <input
                  id="vlan-id"
                  className={`input${errors.vlan_id ? " input-invalid" : ""}`}
                  inputMode="numeric"
                  value={draft.vlanId}
                  placeholder="1–4094"
                  onChange={(e) => set("vlanId", e.target.value)}
                />
                {errors.vlan_id && <span className="field-error">{errors.vlan_id}</span>}
              </div>
            </>
          )}

          {kind === "bond" && (
            <div className="field">
              <label className="field-label" htmlFor="bond-mode">
                Bond mode
              </label>
              <select
                id="bond-mode"
                className="select"
                value={draft.mode}
                onChange={(e) => set("mode", e.target.value as BondMode)}
              >
                <option value="active-backup">active-backup</option>
                <option value="802.3ad">802.3ad (LACP)</option>
                <option value="balance-xor">balance-xor</option>
              </select>
            </div>
          )}

          {kind === "bridge" && (
            <label className="checkbox-row">
              <input
                type="checkbox"
                checked={draft.vlanAware}
                onChange={(e) => set("vlanAware", e.target.checked)}
              />
              VLAN aware
            </label>
          )}

          {showPorts && (
            <div className="field">
              <span className="field-label">{portLabel}</span>
              <div className="port-list">
                {portCandidates.length === 0 && (
                  <span className="field-hint">No interfaces are free to add.</span>
                )}
                {portCandidates.map((link) => (
                  <label key={link.name} className="checkbox-row">
                    <input
                      type="checkbox"
                      checked={draft.ports.includes(link.name)}
                      onChange={() => togglePort(link.name)}
                    />
                    <span className="qz-mono">{link.name}</span>
                    <span className="qz-dim text-[12px]">
                      {link.altname ?? link.kind}
                      {link.carrier ? "" : " · no carrier"}
                    </span>
                  </label>
                ))}
              </div>
              {/* Only when the management link is genuinely missing from the
                  list above. Editing the management link itself excludes it
                  for the ordinary reason — nothing is a port of itself — and
                  saying otherwise there is just confusing. */}
              {managementLink && managementLink !== draft.name && (
                <span className="field-hint">
                  <span className="qz-mono">{managementLink}</span> carries the management address
                  and is not listed. Use &ldquo;Create management bridge&rdquo; to move it safely.
                </span>
              )}
              {errors.ports && <span className="field-error">{errors.ports}</span>}
            </div>
          )}

          {isPort ? (
            <span className="field-hint">
              <span className="qz-mono">{editing?.name}</span> is a port of{" "}
              <span className="qz-mono">{editing?.controller}</span>, so its address is configured
              on <span className="qz-mono">{editing?.controller}</span> instead.
            </span>
          ) : (
            <>
              <div className="field">
                <label className="field-label" htmlFor="link-ipmode">
                  IPv4
                </label>
                <select
                  id="link-ipmode"
                  className="select"
                  value={draft.ipMode}
                  onChange={(e) => set("ipMode", e.target.value as IpMode)}
                >
                  <option value="none">No address</option>
                  <option value="static">Static</option>
                  <option value="dhcp">Automatic (DHCP)</option>
                </select>
              </div>

              {draft.ipMode === "static" && (
                <div className="grid grid-cols-2 gap-3">
                  <div className="field">
                    <label className="field-label" htmlFor="link-cidr">
                      IPv4/CIDR
                    </label>
                    <input
                      id="link-cidr"
                      className={`input${errors.ip ? " input-invalid" : ""}`}
                      value={draft.cidr}
                      placeholder="192.168.1.10/24"
                      onChange={(e) => set("cidr", e.target.value)}
                    />
                    {errors.ip && <span className="field-error">{errors.ip}</span>}
                  </div>
                  <div className="field">
                    <label className="field-label" htmlFor="link-gateway">
                      Gateway (IPv4)
                    </label>
                    <input
                      id="link-gateway"
                      className={`input${errors.gateway ? " input-invalid" : ""}`}
                      value={draft.gateway}
                      placeholder="192.168.1.1"
                      onChange={(e) => set("gateway", e.target.value)}
                    />
                    {errors.gateway && <span className="field-error">{errors.gateway}</span>}
                  </div>
                </div>
              )}
            </>
          )}

          {kind === "bond" && (
            <div className="grid grid-cols-2 gap-3">
              <div className="field">
                <label className="field-label" htmlFor="bond-miimon">
                  Link check (ms)
                </label>
                <input
                  id="bond-miimon"
                  className="input"
                  inputMode="numeric"
                  value={draft.miimon}
                  placeholder="100"
                  onChange={(e) => set("miimon", e.target.value)}
                />
              </div>
              <div className="field">
                <label className="field-label" htmlFor="bond-primary">
                  Primary port
                </label>
                <select
                  id="bond-primary"
                  className={`select${errors.primary ? " select-invalid" : ""}`}
                  value={draft.primary}
                  onChange={(e) => set("primary", e.target.value)}
                >
                  <option value="">None</option>
                  {draft.ports.map((port) => (
                    <option key={port} value={port}>
                      {port}
                    </option>
                  ))}
                </select>
                {errors.primary && <span className="field-error">{errors.primary}</span>}
              </div>
              {draft.mode === "802.3ad" && (
                <>
                  <div className="field">
                    <label className="field-label" htmlFor="bond-lacp">
                      LACP rate
                    </label>
                    <select
                      id="bond-lacp"
                      className="select"
                      value={draft.lacpRate}
                      onChange={(e) => set("lacpRate", e.target.value as Draft["lacpRate"])}
                    >
                      <option value="">Default</option>
                      <option value="slow">slow</option>
                      <option value="fast">fast</option>
                    </select>
                  </div>
                  <div className="field">
                    <label className="field-label" htmlFor="bond-hash">
                      Hash policy
                    </label>
                    <select
                      id="bond-hash"
                      className="select"
                      value={draft.hashPolicy}
                      onChange={(e) => set("hashPolicy", e.target.value as Draft["hashPolicy"])}
                    >
                      <option value="">Default</option>
                      <option value="layer2">layer2</option>
                      <option value="layer2+3">layer2+3</option>
                      <option value="layer3+4">layer3+4</option>
                    </select>
                  </div>
                </>
              )}
            </div>
          )}

          {kind === "bridge" && (
            <div className="grid grid-cols-2 gap-3">
              <label className="checkbox-row mt-5">
                <input
                  type="checkbox"
                  checked={draft.stp}
                  onChange={(e) => set("stp", e.target.checked)}
                />
                Spanning tree (STP)
              </label>
              <div className="field">
                <label className="field-label" htmlFor="bridge-delay">
                  Forward delay (s)
                </label>
                <input
                  id="bridge-delay"
                  className="input"
                  inputMode="numeric"
                  value={draft.forwardDelay}
                  placeholder="0"
                  onChange={(e) => set("forwardDelay", e.target.value)}
                />
              </div>
            </div>
          )}

          {kind === "ethernet" && (
            <div className="grid grid-cols-2 gap-3">
              <div className="field">
                <label className="field-label" htmlFor="nic-autoneg">
                  Link negotiation
                </label>
                <select
                  id="nic-autoneg"
                  className="select"
                  value={draft.autoneg}
                  onChange={(e) => set("autoneg", e.target.value as Draft["autoneg"])}
                >
                  <option value="">Leave as configured</option>
                  <option value="on">Automatic</option>
                  <option value="off">Forced</option>
                </select>
              </div>
              {draft.autoneg === "off" && (
                <>
                  <div className="field">
                    <label className="field-label" htmlFor="nic-speed">
                      Speed (Mb/s)
                    </label>
                    <input
                      id="nic-speed"
                      className="input"
                      inputMode="numeric"
                      value={draft.speed}
                      placeholder="1000"
                      onChange={(e) => set("speed", e.target.value)}
                    />
                  </div>
                  <div className="field">
                    <label className="field-label" htmlFor="nic-duplex">
                      Duplex
                    </label>
                    <select
                      id="nic-duplex"
                      className="select"
                      value={draft.duplex}
                      onChange={(e) => set("duplex", e.target.value as Draft["duplex"])}
                    >
                      <option value="">Choose…</option>
                      <option value="full">full</option>
                      <option value="half">half</option>
                    </select>
                  </div>
                </>
              )}
            </div>
          )}

          <div className="field">
            <label className="field-label" htmlFor="link-mtu">
              MTU
            </label>
            <input
              id="link-mtu"
              className={`input${errors.mtu ? " input-invalid" : ""}`}
              inputMode="numeric"
              value={draft.mtu}
              placeholder="1500"
              onChange={(e) => set("mtu", e.target.value)}
            />
            {errors.mtu && <span className="field-error">{errors.mtu}</span>}
          </div>

          <div className="field">
            <label className="field-label" htmlFor="link-comment">
              Comment
            </label>
            <input
              id="link-comment"
              className="input"
              value={draft.comment}
              placeholder="What this interface is for"
              onChange={(e) => set("comment", e.target.value)}
            />
            <span className="field-hint">Shown in the Description column.</span>
          </div>

          {errors.management && <span className="field-error">{errors.management}</span>}
          {errors.form && <span className="field-error">{errors.form}</span>}
        </div>

        <div className="dialog-actions">
          <button type="button" className="btn" onClick={onClose} disabled={saving}>
            Cancel
          </button>
          <button type="button" className="btn btn-primary" onClick={submit} disabled={saving}>
            {editing ? "Stage changes" : "Stage interface"}
          </button>
        </div>
      </div>
    </div>
  );
}

/// Which dialog a row opens. Physical NICs get the link-settings form; the
/// rest get the form they were created with.
export const dialogKindFor = (kind: LinkKind): DialogKind =>
  kind === "bridge" || kind === "bond" || kind === "vlan" ? kind : "ethernet";
