"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle, ClipboardPaste, Copy } from "lucide-react";
import { Button } from "@/components/ui/Button";

/// A login session on the node, drawn in the browser.
///
/// The transport is the machine console's: same origin, same cookie, a
/// WebSocket carrying bytes. What differs is that this end is a terminal
/// emulator rather than a framebuffer — keystrokes out as binary frames,
/// output in, and one structured message for the size, because a terminal
/// has a size and the browser is the only one who knows it.
///
/// **Copy and paste.** A terminal is where an operator pastes a command
/// and copies an error message, so neither is left to chance:
/// - Selecting text copies it, the way a terminal has always behaved.
/// - Ctrl+Shift+C copies, Ctrl+Shift+V pastes — the terminal bindings,
///   with the plain Ctrl+C left alone because it is an interrupt.
/// - Right-click pastes.
/// - Buttons do both, for a browser whose clipboard permission has not
///   been granted to keystrokes.
///
/// xterm.js and its stylesheet are loaded on the client only: the module
/// touches `window` at import time, and this console is a static export.
export function NodeConsole({ node, local }: { node: string; local: boolean }) {
  const holder = useRef<HTMLDivElement | null>(null);
  const terminal = useRef<import("@xterm/xterm").Terminal | null>(null);
  const socket = useRef<WebSocket | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);

  const copy = useCallback(async () => {
    const selection = terminal.current?.getSelection();
    if (selection) await navigator.clipboard.writeText(selection).catch(() => {});
  }, []);

  const paste = useCallback(async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (text) socket.current?.send(new TextEncoder().encode(text));
    } catch {
      setError(
        "The browser would not give the page its clipboard. Use Ctrl+Shift+V, or allow clipboard access for this site.",
      );
    }
  }, []);

  useEffect(() => {
    if (!local) return;
    let disposed = false;
    let cleanup = () => {};

    void (async () => {
      const [{ Terminal }, { FitAddon }] = await Promise.all([
        import("@xterm/xterm"),
        import("@xterm/addon-fit"),
      ]);
      // The stylesheet ships with the package; importing it here keeps it
      // out of every page that is not this one.
      await import("@xterm/xterm/css/xterm.css");
      if (disposed || !holder.current) return;

      const term = new Terminal({
        // The console's own palette, so the terminal is part of the page
        // rather than a white box sitting in it.
        theme: {
          background: "#0b0f14",
          foreground: "#d7dee8",
          cursor: "#3ddc97",
          selectionBackground: "#2a3d52",
        },
        fontFamily:
          "var(--qz-font-mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace)",
        fontSize: 13,
        cursorBlink: true,
        // Enough history that a long build's output is still there.
        scrollback: 10000,
        // The browser owns the selection; the shell never sees a mouse.
        macOptionIsMeta: true,
      });
      const fit = new FitAddon();
      term.loadAddon(fit);
      term.open(holder.current);
      fit.fit();
      terminal.current = term;

      const url = new URL(
        `/api/environment/nodes/${encodeURIComponent(node)}/shell/ws`,
        window.location.href,
      );
      url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
      const ws = new WebSocket(url);
      ws.binaryType = "arraybuffer";
      socket.current = ws;

      const sendSize = () => {
        fit.fit();
        if (ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ cols: term.cols, rows: term.rows }));
        }
      };

      ws.onopen = () => {
        setConnected(true);
        setError(null);
        sendSize();
        term.focus();
      };
      ws.onmessage = (event) => {
        term.write(new Uint8Array(event.data as ArrayBuffer));
      };
      ws.onclose = () => {
        setConnected(false);
        term.write("\r\n\x1b[2m— the session ended —\x1b[0m\r\n");
      };
      ws.onerror = () => {
        setError(
          "The shell connection failed. A node shell is offered to administrators of this node; the journal says why it was refused.",
        );
      };

      term.onData((data) => {
        if (ws.readyState === WebSocket.OPEN) {
          ws.send(new TextEncoder().encode(data));
        }
      });

      // Selecting copies, the way a terminal always has.
      term.onSelectionChange(() => {
        const selection = term.getSelection();
        if (selection) void navigator.clipboard.writeText(selection).catch(() => {});
      });

      // The terminal bindings. Ctrl+C without shift stays an interrupt —
      // taking it for copy is how a terminal loses its most important key.
      term.attachCustomKeyEventHandler((event) => {
        if (event.type !== "keydown" || !event.ctrlKey || !event.shiftKey) return true;
        if (event.key === "C" || event.key === "c") {
          void copy();
          return false;
        }
        if (event.key === "V" || event.key === "v") {
          void paste();
          return false;
        }
        return true;
      });

      const observer = new ResizeObserver(() => sendSize());
      observer.observe(holder.current);
      window.addEventListener("resize", sendSize);

      cleanup = () => {
        observer.disconnect();
        window.removeEventListener("resize", sendSize);
        ws.close();
        term.dispose();
        terminal.current = null;
        socket.current = null;
      };
    })();

    return () => {
      disposed = true;
      cleanup();
    };
  }, [node, local, copy, paste]);

  if (!local) {
    return (
      <div className="callout callout-warn">
        <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
        <div className="flex-1 text-[13px] text-[var(--qz-fg-2)]">
          A shell runs on the node it belongs to, and this console serves another one.{" "}
          <a
            href={`https://${node}:8443/infrastructure/nodes?node=${encodeURIComponent(node)}&section=console`}
            className="text-[var(--qz-accent)]"
          >
            Open {node}&apos;s own console
          </a>{" "}
          to reach it.
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-center gap-2">
        <span className={`badge badge-${connected ? "ok" : "muted"}`}>
          {connected ? "Connected" : "Not connected"}
        </span>
        <span className="text-[12px] text-[var(--qz-fg-4)]">
          Selecting copies. Ctrl+Shift+C and Ctrl+Shift+V, or the buttons.
        </span>
        <span className="ml-auto flex items-center gap-2">
          <Button kind="secondary" size="sm" icon={Copy} onClick={() => void copy()}>
            Copy
          </Button>
          <Button kind="secondary" size="sm" icon={ClipboardPaste} onClick={() => void paste()}>
            Paste
          </Button>
        </span>
      </div>
      {error && (
        <div className="callout callout-crit">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
        </div>
      )}
      <div
        ref={holder}
        onContextMenu={(event) => {
          event.preventDefault();
          void paste();
        }}
        className="surface"
        style={{ height: "min(70vh, 640px)", padding: "8px", overflow: "hidden" }}
      />
    </div>
  );
}
