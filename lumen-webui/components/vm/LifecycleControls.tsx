"use client";

import { useState } from "react";
import { AlertTriangle, ChevronDown, Play, Power, RotateCcw, RotateCw, Square } from "lucide-react";
import { Button } from "@/components/ui/Button";
import { ModalShell, ModalHeader } from "@/components/ui/Modal";
import { ModalFooter } from "@/components/ui/formkit";
import type { Action, VmView } from "@/lib/vmClient";
import { rebootVm, resetVm, shutdownVm, startVm, stopVm } from "@/lib/vmClient";

export type Verb = "start" | "shutdown" | "stop" | "reboot" | "reset";

interface VerbSpec {
  label: string;
  icon: typeof Play;
  /// What the dialog says this does, when it needs one.
  consequence?: string;
  run: (vmid: number, acknowledge: boolean) => Promise<VmView>;
}

/// What each control does, and what it costs. The wording is the point: an
/// operator about to pull the power on a machine should be told that is what
/// it is, in those words.
const VERBS: Record<Verb, VerbSpec> = {
  start: { label: "Start", icon: Play, run: (vmid) => startVm(vmid) },
  shutdown: {
    label: "Shut down",
    icon: Power,
    run: (vmid) => shutdownVm(vmid),
  },
  stop: {
    label: "Stop",
    icon: Square,
    consequence:
      "This is the equivalent of pulling the power out. The guest gets no warning and anything it " +
      "had not written to disk is lost. Use “Shut down” to ask it to stop properly.",
    run: (vmid, acknowledge) => stopVm(vmid, acknowledge),
  },
  reboot: { label: "Reboot", icon: RotateCw, run: (vmid) => rebootVm(vmid) },
  reset: {
    label: "Reset",
    icon: RotateCcw,
    consequence:
      "This is the reset button: the machine restarts immediately without the guest being asked, " +
      "so anything it had not written to disk is lost.",
    run: (vmid, acknowledge) => resetVm(vmid, acknowledge),
  },
};

/// The five lifecycle verbs, with the backend's own answer about each.
///
/// A control that is off says why — the reason comes from `vm.actions`, not
/// from a rule repeated here, so the console and the node can never disagree
/// about what is possible.
export function LifecycleControls({
  vm,
  busy,
  compact = false,
  menu = true,
  onDone,
}: {
  vm: VmView;
  busy: boolean;
  /// Icon-only, for a table row.
  compact?: boolean;
  /// Compact only: whether the remaining verbs hide behind a drop-down. Off in
  /// the machine table, where one obvious control is the whole point of the
  /// cell and everything else is a click away on the machine's own page.
  menu?: boolean;
  onDone: (message: string) => void;
}) {
  const [confirming, setConfirming] = useState<Verb | null>(null);
  const [working, setWorking] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);

  const run = async (verb: Verb, acknowledge: boolean) => {
    setWorking(true);
    try {
      await VERBS[verb].run(vm.vmid, acknowledge);
      setConfirming(null);
      onDone(`${VERBS[verb].label} sent to ${vm.name}.`);
    } catch (err) {
      onDone(err instanceof Error ? err.message : "Something went wrong.");
    } finally {
      setWorking(false);
    }
  };

  const press = (verb: Verb) => {
    setMenuOpen(false);
    if (vm.actions[verb].requires_acknowledgement) {
      setConfirming(verb);
    } else {
      void run(verb, false);
    }
  };

  const control = (verb: Verb, action: Action) => {
    const { label, icon: Icon } = VERBS[verb];
    const disabled = busy || working || !action.allowed;
    // A disabled button carries no tooltip of its own, so the reason goes on a
    // wrapper — the same trick the Interfaces page uses.
    return (
      <span key={verb} title={action.allowed ? undefined : action.reason}>
        <Button
          kind={verb === "start" ? "primary" : "secondary"}
          size={compact ? "sm" : "md"}
          icon={Icon}
          disabled={disabled}
          onClick={() => press(verb)}
        >
          {compact ? undefined : label}
        </Button>
      </span>
    );
  };

  // In a row there is space for the one obvious control plus a menu; on the
  // detail page every verb gets a button of its own.
  const primary: Verb = vm.actions.start.allowed ? "start" : "shutdown";
  const rest: Verb[] = (["start", "shutdown", "stop", "reboot", "reset"] as Verb[]).filter(
    (verb) => verb !== primary,
  );

  return (
    <>
      {compact ? (
        <div className="flex items-center gap-1 justify-end relative">
          {control(primary, vm.actions[primary])}
          {menu && (
            <Button
              kind="ghost"
              size="sm"
              iconRight={ChevronDown}
              disabled={busy || working}
              onClick={() => setMenuOpen((open) => !open)}
            />
          )}
          {menuOpen && (
            <>
              <div className="fixed inset-0 z-10" onClick={() => setMenuOpen(false)} />
              <div className="menu">
                {rest.map((verb) => {
                  const action = vm.actions[verb];
                  const { label, icon: Icon } = VERBS[verb];
                  return (
                    <button
                      key={verb}
                      type="button"
                      className="menu-item"
                      disabled={!action.allowed}
                      title={action.allowed ? undefined : action.reason}
                      onClick={() => press(verb)}
                    >
                      <Icon size={15} /> {label}
                    </button>
                  );
                })}
              </div>
            </>
          )}
        </div>
      ) : (
        <div className="flex items-center gap-2 flex-wrap">
          {(["start", "shutdown", "reboot", "stop", "reset"] as Verb[]).map((verb) =>
            control(verb, vm.actions[verb]),
          )}
        </div>
      )}

      {confirming && (
        <ConfirmVerbDialog
          vm={vm}
          verb={confirming}
          busy={working}
          onCancel={() => setConfirming(null)}
          onConfirm={() => run(confirming, true)}
        />
      )}
    </>
  );
}

/// Says plainly what the risky verbs do, and collects the acknowledgement the
/// backend demands before it will do them.
function ConfirmVerbDialog({
  vm,
  verb,
  busy,
  onCancel,
  onConfirm,
}: {
  vm: VmView;
  verb: Verb;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const [acked, setAcked] = useState(false);
  const spec = VERBS[verb];

  return (
    <ModalShell onClose={onCancel}>
      <ModalHeader
        title={`${spec.label} ${vm.name}?`}
        subtitle={`Machine ${vm.vmid} on ${vm.node}.`}
        onClose={onCancel}
      />
      <div className="flex flex-col gap-4">
        <div className="callout callout-warn">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{spec.consequence}</div>
        </div>
        <label className="flex items-center gap-[10px] cursor-pointer select-none">
          <input
            type="checkbox"
            checked={acked}
            onChange={(e) => setAcked(e.target.checked)}
            style={{ accentColor: "var(--qz-accent)" }}
          />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            I understand this may lose data.
          </span>
        </label>
        <ModalFooter
          onCancel={onCancel}
          saving={busy}
          disabled={!acked}
          savingLabel="Working…"
          submitLabel={spec.label}
          onSubmit={onConfirm}
        />
      </div>
    </ModalShell>
  );
}
