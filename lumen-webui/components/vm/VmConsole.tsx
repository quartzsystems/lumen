"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  ClipboardPaste,
  ExternalLink,
  Eye,
  FileUp,
  Keyboard,
  Maximize2,
  Monitor,
  Minimize2,
  RotateCw,
  Scaling,
} from "lucide-react";
import type RFB from "@novnc/novnc";
import { Button } from "@/components/ui/Button";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import {
  ErrorText,
  Field,
  ModalFooter,
  TextInput,
  blurBorder,
  focusBorder,
  inputCls,
  monoSt,
} from "@/components/ui/formkit";
import { Panel } from "@/components/vm/VmBits";
import { LifecycleControls } from "@/components/vm/LifecycleControls";
import {
  consoleUrl,
  fetchConsole,
  formatBytes,
  MAX_GUEST_FILE_BYTES,
  pushFile,
  type ConsoleInfo,
  type VmView,
} from "@/lib/vmClient";

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

  // Nothing under the screen: the socket path, the protocol, and the node were
  // three facts an operator reads once and never again, and they were taking
  // the bottom third of the page from the only thing on it. The socket is still
  // named where it is worth naming — on the veil, when the console could not be
  // opened and the path is part of the diagnosis.
  return (
    // Keyed on the machine so opening a different one starts a new connection
    // rather than pointing the old viewer somewhere else.
    <ConsoleScreen key={vm.vmid} vmid={vm.vmid} name={vm.name} popout />
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
  /// The text of the fallback paste dialog, or `null` when it is closed. Only
  /// opened when the browser will not hand over its clipboard by itself.
  const [pasting, setPasting] = useState<string | null>(null);
  /// A file dragged onto the screen, waiting for somebody to say where it goes.
  const [dropped, setDropped] = useState<File | null>(null);
  /// Whether a drag is currently over the screen, for the overlay.
  const [dragging, setDragging] = useState(false);
  /// The last file that made it in, shown briefly in the toolbar.
  const [delivered, setDelivered] = useState<string | null>(null);

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
      // The guest copied something. Writing it into the browser's clipboard is
      // best-effort on purpose: the permission is the browser's to give, it is
      // refused outright when the document is not focused, and a console that
      // threw an error because somebody pressed Ctrl+C in a guest they were not
      // looking at would be worse than one that quietly did not.
      rfb.addEventListener("clipboard", (event) => {
        if (cancelled) return;
        const text = event.detail.text;
        if (typeof text !== "string" || text === "") return;
        void navigator.clipboard?.writeText(text).catch(() => {
          /* not focused, or not permitted — the guest still has it */
        });
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

  /// Put text on the guest's clipboard.
  ///
  /// This does not type it. It hands the string to the guest's clipboard agent,
  /// and the paste inside the guest is still the operator's own Ctrl+V — which
  /// is what makes it work in a terminal, in an installer's text field, and in
  /// anything else that knows how to paste.
  const sendClipboard = useCallback((text: string) => {
    if (text) rfbRef.current?.clipboardPasteFrom(text);
  }, []);

  const pasteFromBrowser = useCallback(async () => {
    // The Clipboard API is the good path, and it needs both a secure context
    // and the operator's permission. Everything that is not that — an insecure
    // origin in development, a browser without the API, a refused prompt —
    // falls back to asking for the text outright instead of failing silently.
    try {
      const text = await navigator.clipboard.readText();
      if (text) {
        sendClipboard(text);
        return;
      }
    } catch {
      /* fall through to the dialog */
    }
    setPasting("");
  }, [sendClipboard]);

  // A real paste, when the browser lets one through. noVNC swallows most key
  // presses to send them to the guest, so this fires only when the frame has
  // focus but the canvas does not — worth having, not worth relying on, which
  // is why the toolbar has a button as well.
  useEffect(() => {
    const onPaste = (event: ClipboardEvent) => {
      if (!frameRef.current?.contains(document.activeElement)) return;
      const text = event.clipboardData?.getData("text");
      if (text) {
        event.preventDefault();
        sendClipboard(text);
      }
    };
    document.addEventListener("paste", onPaste);
    return () => document.removeEventListener("paste", onPaste);
  }, [sendClipboard]);

  // A delivered-file note is worth seeing and not worth keeping.
  useEffect(() => {
    if (!delivered) return;
    const timer = setTimeout(() => setDelivered(null), 6000);
    return () => clearTimeout(timer);
  }, [delivered]);

  const connected = status.kind === "connected";

  return (
    <div
      className="qz-console"
      ref={frameRef}
      onDragOver={(event) => {
        // Only a file drag. Dragging selected text across the screen is not an
        // offer to copy anything into the guest.
        if (!event.dataTransfer.types.includes("Files")) return;
        event.preventDefault();
        event.dataTransfer.dropEffect = "copy";
        setDragging(true);
      }}
      onDragLeave={(event) => {
        // Moving between children fires dragleave too, so the frame is only
        // really left when what is being entered is outside it.
        if (frameRef.current?.contains(event.relatedTarget as Node | null)) return;
        setDragging(false);
      }}
      onDrop={(event) => {
        if (!event.dataTransfer.types.includes("Files")) return;
        event.preventDefault();
        setDragging(false);
        const file = event.dataTransfer.files?.[0];
        if (file) setDropped(file);
      }}
    >
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
        {delivered && (
          <span className="text-[12px] text-[var(--qz-fg-3)]">
            Copied <span className="qz-mono">{delivered}</span> into the guest
          </span>
        )}

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
          <span title="Put this browser's clipboard on the guest's, then paste inside the guest">
            <Button
              kind="secondary"
              size="sm"
              icon={ClipboardPaste}
              disabled={!connected || viewOnly}
              onClick={() => void pasteFromBrowser()}
            >
              Paste
            </Button>
          </span>
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

      {pasting !== null && (
        <PasteDialog
          text={pasting}
          onChange={setPasting}
          onClose={() => setPasting(null)}
          onSend={() => {
            sendClipboard(pasting);
            setPasting(null);
          }}
        />
      )}

      {dragging && (
        <div className="qz-console-drop">
          <FileUp size={26} className="text-[var(--qz-accent)]" />
          <span>Drop a file to copy it into {name}</span>
        </div>
      )}

      {dropped && (
        <PushFileDialog
          vmid={vmid}
          name={name}
          file={dropped}
          onClose={() => setDropped(null)}
          onPushed={(path) => {
            setDropped(null);
            setDelivered(path);
          }}
        />
      )}
    </div>
  );
}

/// Where a dropped file is going, and the copy itself.
///
/// The path is asked for rather than assumed. A guest is somebody else's
/// filesystem — the console has no idea what is on it, whether it is Linux at
/// all, or what would be overwritten — so the one thing this must not do is
/// pick a destination quietly and write over something.
function PushFileDialog({
  vmid,
  name,
  file,
  onClose,
  onPushed,
}: {
  vmid: number;
  name: string;
  file: File;
  onClose: () => void;
  onPushed: (path: string) => void;
}) {
  const [path, setPath] = useState(`/root/${file.name}`);
  const [working, setWorking] = useState(false);
  const [error, setError] = useState("");

  const tooBig = file.size > MAX_GUEST_FILE_BYTES;

  const send = async () => {
    setWorking(true);
    setError("");
    try {
      const result = await pushFile(vmid, path.trim(), file);
      onPushed(result.path);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not copy the file into the guest.");
      setWorking(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Copy into ${name}`}
        subtitle="The guest's own agent writes this, so the machine has to be running."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <div className="qz-facts">
          <dt>File</dt>
          <dd className="qz-mono">{file.name}</dd>
          <dt>Size</dt>
          <dd>
            {formatBytes(file.size)}
            {tooBig && (
              <span style={{ color: "var(--qz-danger)" }}>
                {" "}
                — more than the {formatBytes(MAX_GUEST_FILE_BYTES)} a guest agent will take
              </span>
            )}
          </dd>
        </div>

        <Field
          label="Where it goes, inside the guest"
          htmlFor="push-path"
          hint="A full path. Anything already there is replaced."
        >
          <TextInput id="push-path" value={path} onChange={setPath} mono autoFocus />
        </Field>

        <p className="text-[12px] text-[var(--qz-fg-4)] m-0">
          This goes over the guest agent, not over the console — so it needs{" "}
          <span className="qz-mono">qemu-guest-agent</span> running inside the machine, and the
          file lands with whatever ownership that agent has. It is meant for a script, a key, or a
          configuration file; anything large belongs on a disk or on installation media.
        </p>

        <ErrorText msg={error} />
        <ModalFooter
          onCancel={onClose}
          saving={working}
          disabled={!path.trim().startsWith("/") || tooBig}
          savingLabel="Copying…"
          submitLabel="Copy in"
          onSubmit={send}
        />
      </div>
    </ModalShell>
  );
}

/// Where text goes when the browser will not hand over its clipboard.
///
/// Not a lesser path to apologise for: an insecure origin in development, a
/// browser without the Clipboard API, and an operator who said no to the prompt
/// all end up here, and typing into a box is a thing that always works.
function PasteDialog({
  text,
  onChange,
  onClose,
  onSend,
}: {
  text: string;
  onChange: (value: string) => void;
  onClose: () => void;
  onSend: () => void;
}) {
  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title="Paste into the guest"
        subtitle="This goes onto the guest's clipboard. Paste it inside the guest as usual."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <textarea
          value={text}
          autoFocus
          rows={6}
          onChange={(event) => onChange(event.target.value)}
          className={inputCls}
          style={{ ...monoSt, resize: "vertical", lineHeight: 1.5 }}
          onFocus={focusBorder}
          onBlur={blurBorder}
          placeholder="Text to put on the guest's clipboard"
        />
        <p className="text-[12px] text-[var(--qz-fg-4)] m-0">
          The guest needs <span className="qz-mono">spice-vdagent</span> running for this to reach
          it. A machine without one takes the text and does nothing with it.
        </p>
        <ModalFooter
          onCancel={onClose}
          saving={false}
          disabled={!text}
          savingLabel="Sending…"
          submitLabel="Send"
          onSubmit={onSend}
        />
      </div>
    </ModalShell>
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
