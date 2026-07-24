"use client";

import { useMemo, useState } from "react";
import { ModalShell, ModalHeader } from "@/components/ui/Modal";
import { Switch } from "@/components/ui/Switch";
import {
  CheckList,
  CheckRow,
  ErrorText,
  Field,
  ModalFooter,
  SelectInput,
  TextInput,
} from "@/components/ui/formkit";
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
    <ModalShell onClose={onClose} maxWidth={560}>
      <ModalHeader
        title={editing ? `Edit ${editing.name}` : `Create ${TITLES[kind]}`}
        subtitle={
          editing
            ? "Changes are staged. Nothing reaches the node until you apply them."
            : "The new interface is staged. Nothing reaches the node until you apply it."
        }
        onClose={onClose}
      />

      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <Field label="Name" htmlFor="link-name" required={editing === null} error={errors.name}>
          <TextInput
            id="link-name"
            value={draft.name}
            mono
            // An interface cannot be renamed in place — the profile, the ports
            // pointing at it, and the running device all carry the name — so
            // editing shows it rather than offering it.
            readOnly={editing !== null}
            autoFocus={editing === null}
            invalid={!!errors.name}
            placeholder={kind === "bridge" ? "br1" : kind === "bond" ? "bond0" : "vlan100"}
            onChange={(v) => set("name", v)}
          />
        </Field>

        {kind === "ethernet" && editing && (
          <Field
            label="Alternative name"
            htmlFor="link-altname"
            hint={
              <>
                What the kernel called this adapter before Lumen pinned it to{" "}
                <span style={{ fontFamily: "var(--qz-font-mono)" }}>{editing.name}</span>.
              </>
            }
          >
            <TextInput
              id="link-altname"
              value={editing.altname ?? ""}
              mono
              readOnly
              placeholder="—"
              onChange={() => undefined}
            />
          </Field>
        )}

        {kind === "vlan" && (
          <>
            <Field label="Parent interface" htmlFor="vlan-parent" required error={errors.parent}>
              <SelectInput
                id="vlan-parent"
                value={draft.parent}
                mono
                invalid={!!errors.parent}
                onChange={(v) => set("parent", v)}
              >
                <option value="">Choose an interface…</option>
                {parentCandidates.map((link) => (
                  <option key={link.name} value={link.name}>
                    {link.name} ({link.kind})
                  </option>
                ))}
              </SelectInput>
            </Field>
            <Field label="VLAN id" htmlFor="vlan-id" required error={errors.vlan_id}>
              <TextInput
                id="vlan-id"
                value={draft.vlanId}
                mono
                inputMode="numeric"
                invalid={!!errors.vlan_id}
                placeholder="1–4094"
                onChange={(v) => set("vlanId", v)}
              />
            </Field>
          </>
        )}

        {kind === "bond" && (
          <Field label="Bond mode" htmlFor="bond-mode">
            <SelectInput
              id="bond-mode"
              value={draft.mode}
              mono
              onChange={(v) => set("mode", v as BondMode)}
            >
              <option value="active-backup">active-backup</option>
              <option value="802.3ad">802.3ad (LACP)</option>
              <option value="balance-xor">balance-xor</option>
            </SelectInput>
          </Field>
        )}

        {kind === "bridge" && (
          <label className="flex items-center gap-[10px] cursor-pointer select-none">
            <Switch on={draft.vlanAware} onChange={(v) => set("vlanAware", v)} />
            <span className="text-[13px] text-[var(--qz-fg-2)]">VLAN aware</span>
          </label>
        )}

        {showPorts && (
          <Field
            label={portLabel}
            error={errors.ports}
            // Only when the management link is genuinely missing from the list
            // above. Editing the management link itself excludes it for the
            // ordinary reason — nothing is a port of itself — and saying
            // otherwise there is just confusing.
            hint={
              managementLink && managementLink !== draft.name ? (
                <>
                  <span style={{ fontFamily: "var(--qz-font-mono)" }}>{managementLink}</span> carries
                  the management address and is not listed. Use &ldquo;Create management
                  bridge&rdquo; to move it safely.
                </>
              ) : undefined
            }
          >
            {portCandidates.length === 0 ? (
              <p className="text-[12px] text-[var(--qz-fg-4)] m-0">No interfaces are free to add.</p>
            ) : (
              <CheckList>
                {portCandidates.map((link) => (
                  <CheckRow
                    key={link.name}
                    checked={draft.ports.includes(link.name)}
                    onChange={() => togglePort(link.name)}
                  >
                    <span
                      className="text-[13px] text-[var(--qz-fg-2)]"
                      style={{ fontFamily: "var(--qz-font-mono)" }}
                    >
                      {link.name}
                    </span>
                    <span className="text-[12px] text-[var(--qz-fg-4)]">
                      {link.altname ?? link.kind}
                      {link.carrier ? "" : " · no carrier"}
                    </span>
                  </CheckRow>
                ))}
              </CheckList>
            )}
          </Field>
        )}

        {isPort ? (
          <p className="text-[12px] text-[var(--qz-fg-4)] m-0">
            <span style={{ fontFamily: "var(--qz-font-mono)" }}>{editing?.name}</span> is a port of{" "}
            <span style={{ fontFamily: "var(--qz-font-mono)" }}>{editing?.controller}</span>, so its
            address is configured on{" "}
            <span style={{ fontFamily: "var(--qz-font-mono)" }}>{editing?.controller}</span> instead.
          </p>
        ) : (
          <>
            <Field label="IPv4" htmlFor="link-ipmode">
              <SelectInput
                id="link-ipmode"
                value={draft.ipMode}
                onChange={(v) => set("ipMode", v as IpMode)}
              >
                <option value="none">No address</option>
                <option value="static">Static</option>
                <option value="dhcp">Automatic (DHCP)</option>
              </SelectInput>
            </Field>

            {draft.ipMode === "static" && (
              <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
                <Field label="IPv4/CIDR" htmlFor="link-cidr" required error={errors.ip}>
                  <TextInput
                    id="link-cidr"
                    value={draft.cidr}
                    mono
                    invalid={!!errors.ip}
                    placeholder="192.168.1.10/24"
                    onChange={(v) => set("cidr", v)}
                  />
                </Field>
                <Field label="Gateway (IPv4)" htmlFor="link-gateway" error={errors.gateway}>
                  <TextInput
                    id="link-gateway"
                    value={draft.gateway}
                    mono
                    invalid={!!errors.gateway}
                    placeholder="192.168.1.1"
                    onChange={(v) => set("gateway", v)}
                  />
                </Field>
              </div>
            )}
          </>
        )}

        {kind === "bond" && (
          <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
            <Field label="Link check (ms)" htmlFor="bond-miimon">
              <TextInput
                id="bond-miimon"
                value={draft.miimon}
                mono
                inputMode="numeric"
                placeholder="100"
                onChange={(v) => set("miimon", v)}
              />
            </Field>
            <Field label="Primary port" htmlFor="bond-primary" error={errors.primary}>
              <SelectInput
                id="bond-primary"
                value={draft.primary}
                mono
                invalid={!!errors.primary}
                onChange={(v) => set("primary", v)}
              >
                <option value="">None</option>
                {draft.ports.map((port) => (
                  <option key={port} value={port}>
                    {port}
                  </option>
                ))}
              </SelectInput>
            </Field>
            {draft.mode === "802.3ad" && (
              <>
                <Field label="LACP rate" htmlFor="bond-lacp">
                  <SelectInput
                    id="bond-lacp"
                    value={draft.lacpRate}
                    mono
                    onChange={(v) => set("lacpRate", v as Draft["lacpRate"])}
                  >
                    <option value="">Default</option>
                    <option value="slow">slow</option>
                    <option value="fast">fast</option>
                  </SelectInput>
                </Field>
                <Field label="Hash policy" htmlFor="bond-hash">
                  <SelectInput
                    id="bond-hash"
                    value={draft.hashPolicy}
                    mono
                    onChange={(v) => set("hashPolicy", v as Draft["hashPolicy"])}
                  >
                    <option value="">Default</option>
                    <option value="layer2">layer2</option>
                    <option value="layer2+3">layer2+3</option>
                    <option value="layer3+4">layer3+4</option>
                  </SelectInput>
                </Field>
              </>
            )}
          </div>
        )}

        {kind === "bridge" && (
          <div className="grid gap-4 items-end" style={{ gridTemplateColumns: "1fr 1fr" }}>
            <Field label="Forward delay (s)" htmlFor="bridge-delay">
              <TextInput
                id="bridge-delay"
                value={draft.forwardDelay}
                mono
                inputMode="numeric"
                placeholder="0"
                onChange={(v) => set("forwardDelay", v)}
              />
            </Field>
            <label className="flex items-center gap-[10px] cursor-pointer select-none pb-[9px]">
              <Switch on={draft.stp} onChange={(v) => set("stp", v)} />
              <span className="text-[13px] text-[var(--qz-fg-2)]">Spanning tree (STP)</span>
            </label>
          </div>
        )}

        {kind === "ethernet" && (
          <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
            <Field label="Link negotiation" htmlFor="nic-autoneg">
              <SelectInput
                id="nic-autoneg"
                value={draft.autoneg}
                onChange={(v) => set("autoneg", v as Draft["autoneg"])}
              >
                <option value="">Leave as configured</option>
                <option value="on">Automatic</option>
                <option value="off">Forced</option>
              </SelectInput>
            </Field>
            {draft.autoneg === "off" && (
              <>
                <Field label="Speed (Mb/s)" htmlFor="nic-speed">
                  <TextInput
                    id="nic-speed"
                    value={draft.speed}
                    mono
                    inputMode="numeric"
                    placeholder="1000"
                    onChange={(v) => set("speed", v)}
                  />
                </Field>
                <Field label="Duplex" htmlFor="nic-duplex">
                  <SelectInput
                    id="nic-duplex"
                    value={draft.duplex}
                    mono
                    onChange={(v) => set("duplex", v as Draft["duplex"])}
                  >
                    <option value="">Choose…</option>
                    <option value="full">full</option>
                    <option value="half">half</option>
                  </SelectInput>
                </Field>
              </>
            )}
          </div>
        )}

        <Field label="MTU" htmlFor="link-mtu" error={errors.mtu}>
          <TextInput
            id="link-mtu"
            value={draft.mtu}
            mono
            inputMode="numeric"
            invalid={!!errors.mtu}
            placeholder="1500"
            onChange={(v) => set("mtu", v)}
          />
        </Field>

        <Field label="Comment" htmlFor="link-comment" hint="Shown in the Description column.">
          <TextInput
            id="link-comment"
            value={draft.comment}
            placeholder="What this interface is for"
            onChange={(v) => set("comment", v)}
          />
        </Field>

        {errors.management && <ErrorText msg={errors.management} />}
        {errors.form && <ErrorText msg={errors.form} />}

        <ModalFooter
          onCancel={onClose}
          saving={saving}
          savingLabel="Staging…"
          submitLabel={editing ? "Stage changes" : "Stage interface"}
        />
      </form>
    </ModalShell>
  );
}

/// Which dialog a row opens. Physical NICs get the link-settings form; the
/// rest get the form they were created with.
export const dialogKindFor = (kind: LinkKind): DialogKind =>
  kind === "bridge" || kind === "bond" || kind === "vlan" ? kind : "ethernet";
