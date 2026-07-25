// noVNC ships no TypeScript declarations, so this is the part of its API the
// console actually uses — written against docs/API.md in the package itself.
//
// Deliberately not the whole surface: a declaration file that claims more than
// is used is a second API to keep current, and everything here is checked
// against the real object at runtime by the viewer that uses it.
declare module "@novnc/novnc" {
  /// The events an `RFB` emits, and what each one carries.
  interface RFBEventMap {
    /// Handshaking finished; there is a screen.
    connect: CustomEvent<void>;
    /// The connection ended. `clean` is false when it ended unexpectedly,
    /// which is the difference between "you closed it" and "it dropped".
    disconnect: CustomEvent<{ clean: boolean }>;
    /// The server wants a password. Lumen's console socket has no
    /// authentication of its own — reaching the socket at all requires being
    /// root on the node — so this firing at all means something is wrong.
    credentialsrequired: CustomEvent<{ types: string[] }>;
    securityfailure: CustomEvent<{ status: number; reason?: string }>;
    desktopname: CustomEvent<{ name: string }>;
    clipboard: CustomEvent<{ text: string }>;
    bell: CustomEvent<void>;
    /// `RFB.capabilities` changed — `power` is the one worth knowing about.
    capabilities: CustomEvent<void>;
  }

  export default class RFB extends EventTarget {
    constructor(
      target: HTMLElement,
      urlOrChannel: string | WebSocket,
      options?: {
        credentials?: { username?: string; password?: string; target?: string };
        shared?: boolean;
        repeaterID?: string;
        wsProtocols?: string[];
      },
    );

    /// CSS background behind the remote screen.
    background: string;
    /// Scale the remote screen to fit its container.
    scaleViewport: boolean;
    /// Clip the remote screen to its container rather than showing scrollbars.
    clipViewport: boolean;
    /// Ask the guest to resize its screen to the container. Needs a guest that
    /// supports it; harmless when it does not.
    resizeSession: boolean;
    /// Send no input at all.
    viewOnly: boolean;
    focusOnClick: boolean;
    qualityLevel: number;
    compressionLevel: number;
    readonly capabilities: { power?: boolean };
    readonly clippingViewport: boolean;

    disconnect(): void;
    focus(): void;
    blur(): void;
    sendCtrlAltDel(): void;
    sendKey(keysym: number, code: string | null, down?: boolean): void;
    clipboardPasteFrom(text: string): void;
    machineShutdown(): void;
    machineReboot(): void;
    machineReset(): void;
    toDataURL(type?: string, quality?: number): string;

    addEventListener<K extends keyof RFBEventMap>(
      type: K,
      listener: (event: RFBEventMap[K]) => void,
      options?: boolean | AddEventListenerOptions,
    ): void;
    addEventListener(
      type: string,
      listener: EventListenerOrEventListenerObject,
      options?: boolean | AddEventListenerOptions,
    ): void;
  }
}
