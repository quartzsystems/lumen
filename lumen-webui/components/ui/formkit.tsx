"use client";

// Shared form primitives for the console's modals, ported from QuartzFire's
// formkit so a field in Lumen and a field in QuartzFire are the same control:
// the same padding, the same border that turns accent on focus, monospace for
// anything an operator reads off a network diagram.

export const inputCls = "w-full rounded-md px-3 py-[9px] text-[13px] text-[var(--qz-fg-1)] outline-none";
export const inputSt = { background: "var(--qz-input-bg)", border: "1px solid var(--qz-border)" } as const;
export const monoSt = { ...inputSt, fontFamily: "var(--qz-font-mono)" } as const;

/// The border a rejected value carries until it is fixed.
export const invalidSt = { ...inputSt, borderColor: "var(--qz-danger)" } as const;
export const monoInvalidSt = { ...monoSt, borderColor: "var(--qz-danger)" } as const;

export function focusBorder(e: React.FocusEvent<HTMLElement>) {
  (e.currentTarget as HTMLElement).style.borderColor = "var(--qz-accent)";
}
export function blurBorder(e: React.FocusEvent<HTMLElement>) {
  (e.currentTarget as HTMLElement).style.borderColor = "var(--qz-border)";
}

/// Label, control, and — when the control has been rejected — the reason,
/// under the input that caused it rather than in a banner at the top.
export function Field({
  label,
  hint,
  error,
  htmlFor,
  required,
  children,
}: {
  label: string;
  hint?: React.ReactNode;
  error?: string;
  htmlFor?: string;
  required?: boolean;
  children: React.ReactNode;
}) {
  return (
    <div>
      <label htmlFor={htmlFor} className="block text-[12px] text-[var(--qz-fg-3)] mb-[6px]">
        {label} {required && <span style={{ color: "var(--qz-danger)" }}>*</span>}
      </label>
      {children}
      {error && <ErrorText msg={error} className="mt-[5px]" />}
      {!error && hint && <p className="text-[11px] text-[var(--qz-fg-4)] m-0 mt-[5px]">{hint}</p>}
    </div>
  );
}

/// A plain text input wired to the shared styling. `mono` for addresses/names.
export function TextInput({
  id,
  value,
  onChange,
  placeholder,
  mono,
  invalid,
  readOnly,
  disabled,
  autoFocus,
  inputMode,
  type,
}: {
  id?: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  mono?: boolean;
  invalid?: boolean;
  readOnly?: boolean;
  disabled?: boolean;
  autoFocus?: boolean;
  inputMode?: "numeric" | "text";
  /// "password" masks the value — for secrets that are typed, not read.
  type?: "text" | "password";
}) {
  const base = mono ? monoSt : inputSt;
  // A value that is shown but cannot be changed should not behave like
  // somewhere to type: no tab stop, no text caret, and no accent border on
  // click. It stays a real input so the value can still be selected and
  // copied — an interface name is something operators paste elsewhere.
  const inert = !!readOnly || !!disabled;
  return (
    <input
      id={id}
      type={type}
      value={value}
      readOnly={readOnly}
      disabled={disabled}
      autoFocus={autoFocus}
      inputMode={inputMode}
      tabIndex={inert ? -1 : undefined}
      onChange={(e) => onChange(e.target.value)}
      placeholder={placeholder}
      className={`${inputCls} disabled:opacity-70 read-only:opacity-70 ${
        inert ? "cursor-default caret-transparent" : ""
      }`.trim()}
      style={invalid ? (mono ? monoInvalidSt : invalidSt) : base}
      onFocus={inert ? undefined : focusBorder}
      onBlur={(e) => {
        e.currentTarget.style.borderColor = invalid ? "var(--qz-danger)" : "var(--qz-border)";
      }}
    />
  );
}

/// A drop-down wired to the same styling as `TextInput`.
export function SelectInput({
  id,
  value,
  onChange,
  mono,
  invalid,
  children,
}: {
  id?: string;
  value: string;
  onChange: (v: string) => void;
  mono?: boolean;
  invalid?: boolean;
  children: React.ReactNode;
}) {
  const base = mono ? monoSt : inputSt;
  return (
    <select
      id={id}
      value={value}
      onChange={(e) => onChange(e.target.value)}
      className={`${inputCls} cursor-pointer`}
      style={invalid ? (mono ? monoInvalidSt : invalidSt) : base}
      onFocus={focusBorder}
      onBlur={(e) => {
        e.currentTarget.style.borderColor = invalid ? "var(--qz-danger)" : "var(--qz-border)";
      }}
    >
      {children}
    </select>
  );
}

/// A scrolling box of checkable rows — the ports a bridge or bond collects.
export function CheckList({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-[2px] rounded-md p-[6px] max-h-[170px] overflow-auto" style={inputSt}>
      {children}
    </div>
  );
}

export function CheckRow({
  checked,
  onChange,
  children,
}: {
  checked: boolean;
  onChange: () => void;
  children: React.ReactNode;
}) {
  return (
    <label className="flex items-center gap-[10px] px-2 py-[6px] rounded cursor-pointer select-none hover:bg-[color-mix(in_oklab,white_5%,transparent)] transition-colors">
      <input type="checkbox" checked={checked} onChange={onChange} style={{ accentColor: "var(--qz-accent)" }} />
      {children}
    </label>
  );
}

/// Modal footer with Cancel + submit.
export function ModalFooter({
  onCancel,
  saving,
  disabled = false,
  submitLabel,
  savingLabel = "Applying…",
  onSubmit,
}: {
  onCancel: () => void;
  saving: boolean;
  /** Blocks submit without claiming work is in flight (an unticked ack, say). */
  disabled?: boolean;
  submitLabel: string;
  savingLabel?: string;
  /** Omit inside a `<form>` — the submit button posts it. */
  onSubmit?: () => void;
}) {
  return (
    <div className="flex gap-2 justify-end mt-1">
      <button
        type="button"
        onClick={onCancel}
        disabled={saving}
        className="px-4 py-[9px] rounded-md text-[13px] font-medium cursor-pointer"
        style={{ background: "transparent", border: "1px solid var(--qz-border)", color: "var(--qz-fg-2)" }}
      >
        Cancel
      </button>
      <button
        type={onSubmit ? "button" : "submit"}
        onClick={onSubmit}
        disabled={saving || disabled}
        className="px-4 py-[9px] rounded-md text-[13px] font-semibold cursor-pointer border-0"
        style={{
          background: "var(--qz-accent)",
          color: "var(--qz-fg-on-accent)",
          opacity: saving || disabled ? 0.7 : 1,
          cursor: saving || disabled ? "not-allowed" : "pointer",
        }}
      >
        {saving ? savingLabel : submitLabel}
      </button>
    </div>
  );
}

export function ErrorText({ msg, className = "" }: { msg: string; className?: string }) {
  if (!msg) return null;
  return (
    <p className={`text-[12px] m-0 ${className}`} style={{ color: "var(--qz-danger)" }}>
      {msg}
    </p>
  );
}
