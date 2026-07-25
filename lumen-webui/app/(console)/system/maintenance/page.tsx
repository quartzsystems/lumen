"use client";

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, CalendarClock, Clock, Power, RotateCw, X } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { Button } from "@/components/ui/Button";
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
import { Fact, Facts, Mono, Panel } from "@/components/vm/VmBits";
import { useConsole } from "@/lib/ConsoleContext";
import { ApiError } from "@/lib/authClient";
import {
  cancelPower,
  fetchPower,
  formatCountdown,
  formatMoment,
  formatUptime,
  powerAt,
  powerNow,
  toLocalInputValue,
  type PowerAction,
  type PowerView,
} from "@/lib/systemClient";

/// Restarting and shutting down the node, now or later.
///
/// The schedule is **the node's own** — logind's, the same one `shutdown -r
/// +30` sets — rather than a timer this console keeps. That is what makes it
/// survive the control plane restarting, warn every signed-in session on its
/// own, and be cancellable with `shutdown -c` at the keyboard. It also means a
/// schedule somebody else set shows up here, which is the correct behaviour
/// and would be impossible with a timer of ours.
export default function MaintenancePage() {
  const { setToast } = useConsole();
  const [power, setPower] = useState<PowerView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState<PowerAction | null>(null);
  const [scheduling, setScheduling] = useState(false);
  // Ticks once a second, purely to re-render the countdown. The number itself
  // is computed from the node's clock, not from this.
  const [, setTick] = useState(0);
  /// The browser's clock when the node's was last read, so the countdown runs
  /// against the node's time and not against a browser that is a minute out.
  const [readAt, setReadAt] = useState(() => Math.floor(Date.now() / 1000));

  const refresh = useCallback(async () => {
    try {
      const view = await fetchPower();
      setReadAt(Math.floor(Date.now() / 1000));
      setPower(view);
      setError(null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read this node's power state.");
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    const timer = setInterval(() => setTick((n) => n + 1), 1000);
    return () => clearInterval(timer);
  }, []);

  const cancel = async () => {
    setBusy(true);
    try {
      setPower(await cancelPower());
      setToast("The scheduled restart was called off.");
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Could not cancel it.");
    } finally {
      setBusy(false);
    }
  };

  // How long until a moment, measured against the node's clock rather than the
  // browser's — a workstation a minute out must not show a countdown the node
  // disagrees with.
  const secondsUntil = (at: number) => {
    if (!power) return 0;
    const elapsed = Math.floor(Date.now() / 1000) - readAt;
    return Math.max(0, at - power.now - elapsed);
  };

  return (
    <Page>
      <PageHeader
        title="Maintenance"
        description="Restarting and shutting this node down, now or at a chosen moment."
      />
      <PageBody>
        <div className="flex flex-col gap-4">
          {error && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
            </div>
          )}

          {power?.scheduled && (
            <div className="callout callout-warn">
              <Clock size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="flex-1 min-w-0">
                <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
                  This node will {power.scheduled.action === "reboot" ? "restart" : "shut down"}{" "}
                  {formatCountdown(secondsUntil(power.scheduled.at))}
                </div>
                <div className="text-[13px] text-[var(--qz-fg-3)] mt-1">
                  At {formatMoment(power.scheduled.at)}. Every signed-in session is warned by the
                  node itself; <span className="qz-mono">shutdown -c</span> at the keyboard cancels
                  it too.
                </div>
              </div>
              <Button kind="secondary" icon={X} disabled={busy} onClick={cancel}>
                Cancel
              </Button>
            </div>
          )}

          <Panel title="This node">
            <Facts>
              <Fact label="Node">
                <Mono>{power?.node ?? "—"}</Mono>
              </Fact>
              <Fact label="Up for">{formatUptime(power?.uptime_secs ?? null)}</Fact>
              <Fact label="Scheduled">
                {power?.scheduled ? (
                  <>
                    {power.scheduled.action === "reboot" ? "Restart" : "Shut down"} at{" "}
                    {formatMoment(power.scheduled.at)}
                  </>
                ) : (
                  <span className="qz-dim">nothing</span>
                )}
              </Fact>
            </Facts>
          </Panel>

          <Panel title="Restart">
            <div className="flex flex-col gap-4">
              <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
                Every virtual machine on this node stops with it. Machines with{" "}
                <strong>Start on boot</strong> turned on come back by themselves; the rest have to be
                started again. This console is unreachable until the node is up.
              </p>
              <div className="flex items-center gap-2 flex-wrap">
                <Button kind="danger" icon={RotateCw} disabled={busy} onClick={() => setConfirming("reboot")}>
                  Restart now
                </Button>
                <Button
                  kind="secondary"
                  icon={CalendarClock}
                  disabled={busy}
                  onClick={() => setScheduling(true)}
                >
                  Schedule…
                </Button>
              </div>
            </div>
          </Panel>

          <Panel title="Shut down">
            <div className="flex flex-col gap-4">
              <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
                The node powers off and stays off. Bringing it back needs somebody at the machine, or
                out-of-band management if this node has any — there is nothing this console can do
                once it is off.
              </p>
              <Button kind="danger" icon={Power} disabled={busy} onClick={() => setConfirming("power_off")}>
                Shut down now
              </Button>
            </div>
          </Panel>
        </div>
      </PageBody>

      {confirming && (
        <ConfirmPowerDialog
          action={confirming}
          node={power?.node ?? "this node"}
          onClose={() => setConfirming(null)}
          onSent={(message) => {
            setConfirming(null);
            setToast(message);
          }}
        />
      )}

      {scheduling && (
        <ScheduleDialog
          horizonSecs={power?.horizon_secs ?? 7 * 86400}
          onClose={() => setScheduling(false)}
          onScheduled={(view) => {
            setScheduling(false);
            setPower(view);
            setToast("Scheduled.");
          }}
        />
      )}
    </Page>
  );
}

/// The confirmation in front of an immediate restart or shutdown.
///
/// There is no acknowledgement field on the request behind this, deliberately:
/// unlike stopping a machine, this is not something the console can undo *or*
/// report the result of — the answer is the connection dropping. The dialog is
/// where the confirmation belongs, and a second weaker copy of it in the
/// backend would only be a second thing to get out of step.
function ConfirmPowerDialog({
  action,
  node,
  onClose,
  onSent,
}: {
  action: PowerAction;
  node: string;
  onClose: () => void;
  onSent: (message: string) => void;
}) {
  const [typed, setTyped] = useState("");
  const [working, setWorking] = useState(false);
  const [error, setError] = useState("");

  const reboot = action === "reboot";
  // Typing the node's name is the friction that fits the action: a checkbox is
  // ticked without reading, and this cannot be done by accident on the wrong
  // node — which is the mistake that actually happens.
  const confirmed = typed.trim() === node;

  const run = async () => {
    setWorking(true);
    setError("");
    try {
      await powerNow(action);
      onSent(
        reboot
          ? `${node} is restarting. This console is unreachable until it is back.`
          : `${node} is shutting down. It has to be powered on at the machine.`,
      );
    } catch (err) {
      // The node may drop the connection before answering, and that is the
      // request succeeding rather than failing.
      const message = err instanceof Error ? err.message : "";
      if (err instanceof ApiError && err.status === 0) {
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
        subtitle={reboot ? "Everything on this node stops and comes back." : "Everything on this node stops."}
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <div className="callout callout-warn">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            {reboot
              ? "Every virtual machine on this node is shut down with it. Guests are asked to stop properly, and the node waits for them — a guest that ignores the request delays the restart rather than being cut off."
              : "Every virtual machine on this node is shut down with it, and the node stays off. Nothing in this console can bring it back."}
          </div>
        </div>
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
          submitLabel={reboot ? "Restart now" : "Shut down now"}
          onSubmit={run}
        />
      </div>
    </ModalShell>
  );
}

/// Schedule one for a moment in the future.
function ScheduleDialog({
  horizonSecs,
  onClose,
  onScheduled,
}: {
  horizonSecs: number;
  onClose: () => void;
  onScheduled: (view: PowerView) => void;
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
      onScheduled(await powerAt(action, Math.floor(moment.getTime() / 1000)));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not schedule it.");
      setWorking(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title="Schedule"
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
          There is one schedule and it belongs to the node, so setting another replaces this one —
          and one somebody set with <span className="qz-mono">shutdown</span> at the keyboard shows
          up on this page.
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
