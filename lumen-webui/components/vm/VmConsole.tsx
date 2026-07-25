"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  ExternalLink,
  Eye,
  Keyboard,
  Maximize2,
  Monitor,
  Minimize2,
  RotateCw,
  Scaling,
} from "lucide-react";
import type RFB from "@novnc/novnc";
import { Button } from "@/components/ui/Button";
import { Fact, Facts, Mono, Panel } from "@/components/vm/VmBits";
import { LifecycleControls } from "@/components/vm/LifecycleControls";
import { consoleUrl, fetchConsole, type ConsoleInfo, type VmView } from "@/lib/vmClient";

/// Where the console viewer is in its life.
///
/// `closed` keeps the last screen underneath it rather than clearing the
/// canvas: a guest that just crashed drew something worth reading, and the
/// last frame is often the whole of the diagnosis.
type Status =
  | { kind: "opening" }
  | { kind: "connected" }
  | { kind: "closed"; message: string; blame: "expected" | "unexpected" };

/// The Console section of a machine's detail page.
///
/// A machine that is not running has no console — the hypervisor listens on
/// the socket only while the guest exists — so this shows the backend's own
/// reason and the control that fixes it, rather than a viewer that opens and
/// immediately goes grey.
export function VmConsole({
  vm,
  busy,
  onAction,
}: {
  vm: VmView;
  busy: boolean;
  onAction: (message: string) => void;
}) {
  if (!vm.actions.console.allowed) {
    return (
      <Panel title="Console">
        <div className="flex flex-col gap-4">
          <div className="callout callout-warn">
            <Monitor size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              {vm.actions.console.reason ?? "This machine has no console."}
            </div>
          </div>
          <LifecycleControls vm={vm} busy={busy} onDone={onAction} />
        </div>
      </Panel>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      {/* Keyed on the machine so opening a different one starts a new
          connection rather than pointing the old viewer somewhere else. */}
      <ConsoleScreen key={vm.vmid} vmid={vm.vmid} name={vm.name} popout />
      <Panel title="Connection">
        <Facts>
          <Fact label="Protocol">VNC</Fact>
          <Fact label="Socket">
            <Mono>{vm.vnc_socket}</Mono>
          </Fact>
          <Fact label="Node">
            <Mono>{vm.node}</Mono>
          </Fact>
        </Facts>
        <p className="text-[12px] text-[var(--qz-fg-4)] mt-4 mb-0">
          The stream is the hypervisor&rsquo;s own, carried over this
          console&rsquo;s connection and not interpreted on the way through. It
          is reachable only with the session you are already signed in with.
        </p>
      </Panel>
    </div>
  );
}

/// The viewer: a toolbar and the guest's screen.
///
/// Separate from the section above so the pop-out window renders exactly the
/// same thing, and so the section stays a decision about *whether* there is a
/// screen while this stays the screen itself.
export function ConsoleScreen({
  vmid,
  name,
  popout = false,
}: {
  vmid: number;
  name: string;
  /// Offer the detached-window control. The detached window itself does not.
  popout?: boolean;
}) {
  const frameRef = useRef<HTMLDivElement>(null);
  const screenRef = useRef<HTMLDivElement>(null);
  const rfbRef = useRef<RFB | null>(null);

  const [status, setStatus] = useState<Status>({ kind: "opening" });
  const [info, setInfo] = useState<ConsoleInfo | null>(null);
  const [desktop, setDesktop] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [fit, setFit] = useState(true);
  const [viewOnly, setViewOnly] = useState(false);
  const [fullscreen, setFullscreen] = useState(false);

  useEffect(() => {
    let cancelled = false;
    let rfb: RFB | null = null;
    setStatus({ kind: "opening" });
    setDesktop(null);

    // noVNC is imported here rather than at the top of the file for two
    // reasons: it touches `window` as soon as it is evaluated, which the
    // static export's prerender pass does not have, and it is the largest
    // thing in the console — a page nobody opens should not pay for it.
    const open = async () => {
      // Ask first. A WebSocket that fails can only answer with a close code,
      // and "1006" is not something anyone can act on; this turns a stopped
      // machine or an expired session into the sentence it actually is.
      let target: ConsoleInfo;
      try {
        target = await fetchConsole(vmid);
      } catch (err) {
        if (cancelled) return;
        setStatus({
          kind: "closed",
          message: err instanceof Error ? err.message : "Could not open the console.",
          blame: "unexpected",
        });
        return;
      }
      if (cancelled) return;
      setInfo(target);

      const { default: RFBClass } = await import("@novnc/novnc");
      if (cancelled || !screenRef.current) return;

      rfb = new RFBClass(screenRef.current, consoleUrl(target.websocket));
      rfbRef.current = rfb;
      rfb.background = "#000";
      rfb.scaleViewport = true;
      rfb.clipViewport = true;
      // Deliberately off: asking the guest to match the browser window means a
      // resolution that changes when somebody drags a corner, and an installer
      // half-way through a screen redraw does not enjoy that.
      rfb.resizeSession = false;

      rfb.addEventListener("connect", () => {
        if (!cancelled) setStatus({ kind: "connected" });
      });
      rfb.addEventListener("disconnect", (event) => {
        if (cancelled) return;
        setStatus(
          event.detail.clean
            ? { kind: "closed", message: "The console was closed.", blame: "expected" }
            : {
                kind: "closed",
                message: `The connection to ${name} ended. The machine may have stopped.`,
                blame: "unexpected",
              },
        );
      });
      rfb.addEventListener("desktopname", (event) => {
        if (!cancelled) setDesktop(event.detail.name);
      });
      // Lumen's console socket has no password of its own — reaching it at all
      // means being root on the node — so being asked for one means the
      // machine is pointed at something other than its own socket.
      rfb.addEventListener("credentialsrequired", () => {
        if (cancelled) return;
        rfb?.disconnect();
        setStatus({
          kind: "closed",
          message: "This console asked for a password, which Lumen does not set. Check the machine's graphics device.",
          blame: "unexpected",
        });
      });
      rfb.addEventListener("securityfailure", (event) => {
        if (cancelled) return;
        setStatus({
          kind: "closed",
          message: event.detail.reason ?? "The console refused the connection.",
          blame: "unexpected",
        });
      });
    };

    void open();

    return () => {
      cancelled = true;
      // `disconnect` on an already-dead connection throws in some states, and
      // there is nothing useful to do about it during teardown.
      try {
        rfb?.disconnect();
      } catch {
        /* already gone */
      }
      rfbRef.current = null;
      if (screenRef.current) screenRef.current.replaceChildren();
    };
  }, [vmid, name, attempt]);

  // Toolbar switches act on the live connection, which is a plain object
  // rather than React state — so they are applied here instead of at
  // construction, and stay applied across a reconnect because the effect above
  // sets its own defaults and these run after it.
  useEffect(() => {
    if (rfbRef.current) rfbRef.current.scaleViewport = fit;
  }, [fit, status]);

  useEffect(() => {
    if (rfbRef.current) rfbRef.current.viewOnly = viewOnly;
  }, [viewOnly, status]);

  // The browser owns full screen, and it can be left with Escape without the
  // page hearing about it any other way.
  useEffect(() => {
    const onChange = () => setFullscreen(document.fullscreenElement === frameRef.current);
    document.addEventListener("fullscreenchange", onChange);
    return () => document.removeEventListener("fullscreenchange", onChange);
  }, []);

  const toggleFullscreen = useCallback(() => {
    if (document.fullscreenElement) void document.exitFullscreen();
    else void frameRef.current?.requestFullscreen();
  }, []);

  const connected = status.kind === "connected";

  return (
    <div className="qz-console" ref={frameRef}>
      <div className="qz-console-bar">
        <span className="qz-console-status">
          <span
            className={`state-dot state-dot-${
              connected ? "ok" : status.kind === "opening" ? "warn" : "muted"
            }`}
            aria-hidden
          />
          {status.kind === "opening"
            ? "Connecting…"
            : connected
              ? (desktop ?? name)
              : "Not connected"}
        </span>

        <div className="ml-auto flex items-center gap-2">
          <Button
            kind="secondary"
            size="sm"
            icon={Keyboard}
            disabled={!connected || viewOnly}
            onClick={() => rfbRef.current?.sendCtrlAltDel()}
          >
            Ctrl+Alt+Del
          </Button>
          <Toggle
            icon={Scaling}
            on={fit}
            label={fit ? "Showing the whole screen" : "Showing the screen at full size"}
            onClick={() => setFit((on) => !on)}
          />
          <Toggle
            icon={Eye}
            on={viewOnly}
            label={viewOnly ? "Watching only — input is not sent" : "Input is sent to the guest"}
            onClick={() => setViewOnly((on) => !on)}
          />
          <Button
            kind="secondary"
            size="sm"
            icon={fullscreen ? Minimize2 : Maximize2}
            onClick={toggleFullscreen}
          >
            {fullscreen ? "Exit" : "Full screen"}
          </Button>
          {popout && !fullscreen && (
            <Button
              kind="secondary"
              size="sm"
              icon={ExternalLink}
              onClick={() =>
                window.open(
                  `/console/?vm=${vmid}`,
                  `lumen-console-${vmid}`,
                  "width=1100,height=800,menubar=no,toolbar=no",
                )
              }
            >
              Detach
            </Button>
          )}
          <Button
            kind={connected ? "secondary" : "primary"}
            size="sm"
            icon={RotateCw}
            onClick={() => setAttempt((n) => n + 1)}
          >
            {connected ? "Reconnect" : "Connect"}
          </Button>
        </div>
      </div>

      <div className="qz-console-screen" ref={screenRef} onClick={() => rfbRef.current?.focus()}>
        {status.kind !== "connected" && (
          <div className="qz-console-veil">
            {status.kind === "opening" ? (
              <span>Opening the console of {name}…</span>
            ) : (
              <>
                {status.blame === "unexpected" && (
                  <AlertTriangle size={20} className="text-[var(--qz-warn)]" />
                )}
                <span className="max-w-[46ch]">{status.message}</span>
                {info && (
                  <span className="text-[11px] text-[var(--qz-fg-4)] qz-mono">{info.socket}</span>
                )}
                <Button kind="primary" size="sm" icon={RotateCw} onClick={() => setAttempt((n) => n + 1)}>
                  Try again
                </Button>
              </>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

/// A toolbar switch that shows its state, for the two settings that are on or
/// off rather than an action.
function Toggle({
  icon: Icon,
  on,
  label,
  onClick,
}: {
  icon: typeof Eye;
  on: boolean;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={label}
      aria-label={label}
      aria-pressed={on}
      className="w-8 h-8 rounded-md grid place-items-center cursor-pointer transition-all duration-[120ms]"
      style={{
        background: on ? "var(--qz-accent-soft)" : "transparent",
        color: on ? "var(--qz-accent)" : "var(--qz-fg-3)",
        border: `1px solid ${on ? "color-mix(in oklab, var(--qz-accent) 30%, transparent)" : "transparent"}`,
      }}
    >
      <Icon size={16} />
    </button>
  );
}
