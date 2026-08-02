"use client";

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, CalendarClock, Clock, Power, RotateCw, X } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { Button, IconButton } from "@/components/ui/Button";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import {
  ErrorText,
  Field,
  ModalFooter,
  SelectInput,
  blurBorder,
  focusBorder,
  inputCls,
  monoSt,
} from "@/components/ui/formkit";
import { DataTable, type Column } from "@/components/console/DataTable";
import { Panel } from "@/components/vm/VmBits";
import { useConsole } from "@/lib/ConsoleContext";
import { ApiError } from "@/lib/authClient";
import {
  cancelNodePower,
  fetchEnvironmentPower,
  formatCountdown,
  formatMoment,
  formatUptime,
  powerNodeAt,
  powerNodeNow,
  toLocalInputValue,
  type MemberPower,
  type PowerAction,
} from "@/lib/systemClient";

/// Restarting and shutting down the nodes in this environment, now or later.
///
/// Written from the environment down, like the Updates page beside it: an
/// operator restarting a node is not necessarily sitting at that node's
/// console, and making them find its address first was work the console could
/// do for them. Every member is in the table, and the buttons act on the node
/// in the row.
///
/// The schedule is **the node's own** — logind's, the same one `shutdown -r
/// +30` sets — rather than a timer this console keeps. That is what makes it
/// survive the control plane restarting, warn every signed-in session on its
/// own, and be cancellable with `shutdown -c` at the keyboard. It also means a
/// schedule somebody else set shows up here, which is the correct behaviour
/// and would be impossible with a timer of ours.
///
/// There is deliberately no button that acts on every node at once. A restart
/// is one node going down at a moment somebody chose; taking a whole cluster
/// through restarts one at a time is a rolling update, and that lives on the
/// Updates page with the drain and the quorum guard it needs.
export default function MaintenancePage() {
  const { setToast } = useConsole();
  const [members, setMembers] = useState<MemberPower[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState<{ node: string; action: PowerAction } | null>(null);
  const [scheduling, setScheduling] = useState<MemberPower | null>(null);
  // Ticks once a second, purely to re-render the countdowns. The numbers
  // themselves are computed from each node's own clock, not from this.
  const [, setTick] = useState(0);
  /// The browser's clock when the nodes were last read, so a countdown runs
  /// against the node's time and not against a browser that is a minute out.
  const [readAt, setReadAt] = useState(() => Math.floor(Date.now() / 1000));

  const refresh = useCallback(async () => {
    try {
      const view = await fetchEnvironmentPower();
      setReadAt(Math.floor(Date.now() / 1000));
      setMembers(view.members);
      setError(null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the nodes' power state.");
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    const timer = setInterval(() => setTick((n) => n + 1), 1000);
    return () => clearInterval(timer);
  }, []);

  const cancel = async (node: string) => {
    setBusy(true);
    try {
      await cancelNodePower(node);
      await refresh();
      setToast(`${node}'s schedule was called off.`);
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Could not cancel it.");
    } finally {
      setBusy(false);
    }
  };

  // How long until a moment, measured against the node's own clock rather than
  // the browser's — a workstation a minute out must not show a countdown the
  // node disagrees with.
  const secondsUntil = (member: MemberPower, at: number) => {
    if (!member.power) return 0;
    const elapsed = Math.floor(Date.now() / 1000) - readAt;
    return Math.max(0, at - member.power.now - elapsed);
  };

  const scheduled = members.filter((member) => member.power?.scheduled);

  return (
    <Page>
      <PageHeader
        title="Maintenance"
        description="Restarting and shutting down the nodes in this environment, now or at a chosen moment."
      />
      <PageBody>
        <div className="flex flex-col gap-4">
          {error && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
            </div>
          )}

          {/* One per node, rather than one summary line: a countdown is only
              useful attached to the node it is about, and an operator who has
              scheduled two of them needs to see both. */}
          {scheduled.map((member) => (
            <div className="callout callout-warn" key={member.node}>
              <Clock size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="flex-1 min-w-0">
                <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
                  {member.node} will{" "}
                  {member.power!.scheduled!.action === "reboot" ? "restart" : "shut down"}{" "}
                  {formatCountdown(secondsUntil(member, member.power!.scheduled!.at))}
                </div>
                <div className="text-[13px] text-[var(--qz-fg-3)] mt-1">
                  At {formatMoment(member.power!.scheduled!.at)}. Every signed-in session is warned
                  by the node itself; <span className="qz-mono">shutdown -c</span> at its keyboard
                  cancels it too.
                </div>
              </div>
              <Button
                kind="secondary"
                icon={X}
                disabled={busy}
                onClick={() => void cancel(member.node)}
              >
                Cancel
              </Button>
            </div>
          ))}

          {/* One card, because restarting and shutting down are one decision
              made in one place — two cards read as two unrelated features and
              put the more destructive of the pair further down the page. Each
              still keeps its own sentence: the difference between them is the
              whole point. */}
          <Panel title="Power Options">
            <div className="flex flex-col gap-5">
              <div className="flex flex-col gap-3">
                <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">Restart</div>
                <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
                  Every virtual machine on the node stops with it. Machines with{" "}
                  <strong>Start on boot</strong> turned on come back by themselves; the rest have to
                  be started again. A node whose cluster would lose quorum without it refuses —
                  put it into maintenance first, from the cluster it belongs to, which moves its
                  machines off and tells the other members to expect the absence.
                </p>
              </div>

              <div style={{ borderTop: "1px solid var(--qz-border)" }} />

              <div className="flex flex-col gap-3">
                <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">Shut Down</div>
                <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
                  The node powers off and stays off. Bringing it back needs somebody at the machine,
                  or out-of-band management if that node has any — there is nothing this console can
                  do once it is off.
                </p>
              </div>

              <div style={{ borderTop: "1px solid var(--qz-border)" }} />

              <div className="flex flex-col gap-3">
                <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
                  Both act on the node in the row. Restarting or shutting down{" "}
                  <strong>this node</strong> — the one marked below — takes this console with it
                  until it is back; any other node is taken down while you keep watching.
                </p>
                <NodesTable
                  members={members}
                  busy={busy}
                  onRestart={(node) => setConfirming({ node, action: "reboot" })}
                  onShutDown={(node) => setConfirming({ node, action: "power_off" })}
                  onSchedule={(member) => setScheduling(member)}
                  onRefresh={() => void refresh()}
                />
              </div>
            </div>
          </Panel>
        </div>
      </PageBody>

      {confirming && (
        <ConfirmPowerDialog
          action={confirming.action}
          node={confirming.node}
          local={members.find((member) => member.node === confirming.node)?.local ?? false}
          onClose={() => setConfirming(null)}
          onSent={(message) => {
            setConfirming(null);
            setToast(message);
            void refresh();
          }}
        />
      )}

      {scheduling && (
        <ScheduleDialog
          node={scheduling.node}
          horizonSecs={scheduling.power?.horizon_secs ?? 7 * 86400}
          onClose={() => setScheduling(null)}
          onScheduled={() => {
            setScheduling(null);
            void refresh();
            setToast("Scheduled.");
          }}
        />
      )}
    </Page>
  );
}

/// Every member, and what may be done to it.
///
/// A member that could not be asked keeps its row and loses its buttons: there
/// is nothing to act on, and an empty row is the honest way to say a node is
/// not answering — which, on this page, is often the last thing the operator
/// asked for.
function NodesTable({
  members,
  busy,
  onRestart,
  onShutDown,
  onSchedule,
  onRefresh,
}: {
  members: MemberPower[];
  busy: boolean;
  onRestart: (node: string) => void;
  onShutDown: (node: string) => void;
  onSchedule: (member: MemberPower) => void;
  onRefresh: () => void;
}) {
  const columns: Column<MemberPower>[] = [
    {
      key: "node",
      header: "Node",
      value: (row) => row.node,
      mono: true,
      width: 220,
      render: (row) => (
        <span className="inline-flex items-center gap-2 min-w-0">
          <span className="qz-mono truncate">{row.node}</span>
          {row.local && <span className="badge badge-muted flex-shrink-0">this node</span>}
        </span>
      ),
    },
    {
      key: "state",
      header: "State",
      value: (row) => (row.reachable ? "answering" : "not answering"),
      width: 150,
      render: (row) =>
        row.reachable ? (
          <span className="badge badge-ok">answering</span>
        ) : (
          <span className="badge badge-crit" title={row.error ?? undefined}>
            not answering
          </span>
        ),
    },
    {
      key: "uptime",
      header: "Up for",
      value: (row) => row.power?.uptime_secs ?? -1,
      width: 140,
      render: (row) =>
        row.power?.uptime_secs != null ? (
          <span>{formatUptime(row.power.uptime_secs)}</span>
        ) : (
          <span className="qz-dim">—</span>
        ),
    },
    {
      key: "scheduled",
      header: "Scheduled",
      value: (row) => row.power?.scheduled?.at ?? 0,
      width: 260,
      render: (row) => {
        const at = row.power?.scheduled;
        if (!at) return <span className="qz-dim">nothing</span>;
        return (
          <span className="badge badge-warn inline-flex items-center gap-1">
            {at.action === "reboot" ? <RotateCw size={11} /> : <Power size={11} />}
            {at.action === "reboot" ? "Restart" : "Shut down"} at {formatMoment(at.at)}
          </span>
        );
      },
    },
  ];

  return (
    <DataTable
      rows={members}
      columns={columns}
      rowId={(row) => row.node}
      searchPlaceholder="Search nodes…"
      emptyMessage="No nodes answered."
      onRefresh={onRefresh}
      storageKey="maintenance-nodes"
      actionsWidth={120}
      actions={(row) =>
        row.reachable && !busy ? (
          <div className="inline-flex items-center gap-1">
            <IconButton
              icon={RotateCw}
              label={`Restart ${row.node}`}
              onClick={() => onRestart(row.node)}
            />
            <IconButton
              icon={Power}
              label={`Shut down ${row.node}`}
              onClick={() => onShutDown(row.node)}
            />
            <IconButton
              icon={CalendarClock}
              label={`Schedule a restart or shutdown for ${row.node}`}
              onClick={() => onSchedule(row)}
            />
          </div>
        ) : (
          <span className="qz-dim">—</span>
        )
      }
    />
  );
}

/// The confirmation in front of an immediate restart or shutdown.
///
/// There is no acknowledgement field on the request behind this, deliberately:
/// unlike stopping a machine, this is not something the console can undo *or*
/// report the result of — for the node serving this page, the answer is the
/// connection dropping. The dialog is where the confirmation belongs, and a
/// second weaker copy of it in the backend would only be a second thing to get
/// out of step.
function ConfirmPowerDialog({
  action,
  node,
  local,
  onClose,
  onSent,
}: {
  action: PowerAction;
  node: string;
  local: boolean;
  onClose: () => void;
  onSent: (message: string) => void;
}) {
  const [typed, setTyped] = useState("");
  const [working, setWorking] = useState(false);
  const [error, setError] = useState("");

  const reboot = action === "reboot";
  // Typing the node's name is the friction that fits the action: a checkbox is
  // ticked without reading, and this cannot be done by accident on the wrong
  // node — which is the mistake that actually happens, and which a table of
  // every node in the environment makes easier to make.
  const confirmed = typed.trim() === node;

  const run = async () => {
    setWorking(true);
    setError("");
    try {
      await powerNodeNow(node, action);
      onSent(
        reboot
          ? local
            ? `${node} is restarting. This console is unreachable until it is back.`
            : `${node} is restarting.`
          : `${node} is shutting down. It has to be powered on at the machine.`,
      );
    } catch (err) {
      // This node may drop the connection before answering, and that is the
      // request succeeding rather than failing. Only for this node: a peer
      // that does not answer is a peer that may not have been reached at all.
      const message = err instanceof Error ? err.message : "";
      if (local && err instanceof ApiError && err.status === 0) {
        onSent(reboot ? `${node} is restarting.` : `${node} is shutting down.`);
        return;
      }
      setError(message || `Could not ${reboot ? "restart" : "shut down"} ${node}.`);
      setWorking(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={reboot ? `Restart ${node}?` : `Shut down ${node}?`}
        subtitle={
          reboot ? "Everything on that node stops and comes back." : "Everything on that node stops."
        }
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <div className="callout callout-warn">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            {reboot
              ? `Every virtual machine on ${node} is shut down with it. Guests are asked to stop properly, and the node waits for them — a guest that ignores the request delays the restart rather than being cut off.`
              : `Every virtual machine on ${node} is shut down with it, and the node stays off. Nothing in this console can bring it back.`}
          </div>
        </div>
        {local && (
          <div className="callout callout-info">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-info)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              This is the node serving this page, so the console goes away with it.{" "}
              {reboot
                ? "Reload once it is back."
                : "Another member's console is where you will see it is off."}
            </div>
          </div>
        )}
        <Field label={`Type ${node} to confirm`} htmlFor="confirm-node">
          <input
            id="confirm-node"
            value={typed}
            autoFocus
            autoComplete="off"
            onChange={(e) => setTyped(e.target.value)}
            className={inputCls}
            style={monoSt}
            onFocus={focusBorder}
            onBlur={blurBorder}
            placeholder={node}
          />
        </Field>
        <ErrorText msg={error} />
        <ModalFooter
          onCancel={onClose}
          saving={working}
          disabled={!confirmed}
          savingLabel="Sending…"
          submitLabel={reboot ? "Restart Now" : "Shut Down Now"}
          onSubmit={run}
        />
      </div>
    </ModalShell>
  );
}

/// Schedule one for a moment in the future, on one node.
function ScheduleDialog({
  node,
  horizonSecs,
  onClose,
  onScheduled,
}: {
  node: string;
  horizonSecs: number;
  onClose: () => void;
  onScheduled: () => void;
}) {
  const [action, setAction] = useState<PowerAction>("reboot");
  // An hour from now, on a whole minute — far enough away to be a schedule
  // rather than a slow "now".
  const [when, setWhen] = useState(() => toLocalInputValue(new Date(Date.now() + 60 * 60 * 1000)));
  const [working, setWorking] = useState(false);
  const [error, setError] = useState("");

  const run = async () => {
    const moment = new Date(when);
    if (Number.isNaN(moment.getTime())) {
      setError("Pick a date and time.");
      return;
    }
    setWorking(true);
    setError("");
    try {
      // Seconds since the epoch: the browser's timezone is the operator's, and
      // an epoch second means the same thing on both sides of the wire.
      await powerNodeAt(node, action, Math.floor(moment.getTime() / 1000));
      onScheduled();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not schedule it.");
      setWorking(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Schedule for ${node}`}
        subtitle="The node holds this itself, so it survives the console restarting."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <Field label="What" htmlFor="schedule-action">
          <SelectInput
            id="schedule-action"
            value={action}
            onChange={(value) => setAction(value as PowerAction)}
          >
            <option value="reboot">Restart</option>
            <option value="power_off">Shut down</option>
          </SelectInput>
        </Field>
        <Field
          label="When"
          htmlFor="schedule-when"
          hint={`In your own timezone, up to ${Math.round(horizonSecs / 86400)} days ahead.`}
        >
          <input
            id="schedule-when"
            type="datetime-local"
            value={when}
            min={toLocalInputValue(new Date())}
            onChange={(e) => setWhen(e.target.value)}
            className={inputCls}
            style={monoSt}
            onFocus={focusBorder}
            onBlur={blurBorder}
          />
        </Field>
        <p className="text-[12px] text-[var(--qz-fg-4)] m-0">
          There is one schedule per node and it belongs to the node, so setting another replaces{" "}
          {node}&apos;s — and one somebody set with <span className="qz-mono">shutdown</span> at its
          keyboard shows up on this page.
        </p>
        <ErrorText msg={error} />
        <ModalFooter
          onCancel={onClose}
          saving={working}
          savingLabel="Scheduling…"
          submitLabel="Schedule"
          onSubmit={run}
        />
      </div>
    </ModalShell>
  );
}
