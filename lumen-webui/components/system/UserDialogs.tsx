"use client";

import { useState } from "react";
import { AlertTriangle } from "lucide-react";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import {
  ErrorText,
  Field,
  ModalFooter,
  SelectInput,
  TextInput,
  blurBorder,
  focusBorder,
  inputCls,
  inputSt,
} from "@/components/ui/formkit";
import { Switch } from "@/components/ui/Switch";
import { validationErrorsOf } from "@/lib/vmClient";
import {
  createUser,
  deleteUser,
  updateUser,
  type DeleteUserResponse,
  type UserView,
} from "@/lib/systemClient";

/// The per-field problems a rejected request came back with, keyed by field.
///
/// The backend answers with a code, the field it belongs to, and a sentence;
/// the sentence is rendered under the input that caused it rather than in a
/// banner at the top, which is the whole reason the field is on the wire.
type FieldErrors = Record<string, string>;

const fieldErrorsOf = (err: unknown): FieldErrors => {
  const out: FieldErrors = {};
  for (const problem of validationErrorsOf(err)) {
    if (problem.field && !out[problem.field]) out[problem.field] = problem.message;
  }
  return out;
};

/// Anything the backend rejected that was not about a particular field.
const generalErrorOf = (err: unknown, fields: FieldErrors): string => {
  if (Object.keys(fields).length > 0) return "";
  return err instanceof Error ? err.message : "Something went wrong.";
};

/// A password field with a reveal control.
///
/// Revealing matters more than it looks on an appliance: an operator setting a
/// password for somebody else has to be able to read back what they typed, and
/// the alternative is a confirm field that catches typing mistakes and nothing
/// else.
function PasswordInput({
  id,
  value,
  onChange,
  placeholder,
  autoFocus,
}: {
  id?: string;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  autoFocus?: boolean;
}) {
  const [shown, setShown] = useState(false);
  return (
    <div className="flex gap-2">
      <input
        id={id}
        type={shown ? "text" : "password"}
        value={value}
        autoFocus={autoFocus}
        autoComplete="new-password"
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        className={inputCls}
        style={inputSt}
        onFocus={focusBorder}
        onBlur={blurBorder}
      />
      <button
        type="button"
        onClick={() => setShown((on) => !on)}
        className="px-3 rounded-md text-[12px] cursor-pointer flex-shrink-0"
        style={{
          background: "transparent",
          border: "1px solid var(--qz-border)",
          color: "var(--qz-fg-3)",
        }}
      >
        {shown ? "Hide" : "Show"}
      </button>
    </div>
  );
}

/// Create a local account.
export function CreateUserDialog({
  shells,
  adminGroup,
  onClose,
  onCreated,
}: {
  shells: string[];
  adminGroup: string;
  onClose: () => void;
  onCreated: (user: UserView) => void;
}) {
  const [name, setName] = useState("");
  const [fullName, setFullName] = useState("");
  const [password, setPassword] = useState("");
  // A node whose /etc/shells could not be read offers no list, and then this
  // is a free text field rather than an empty drop-down.
  const [shell, setShell] = useState(shells.includes("/bin/bash") ? "/bin/bash" : (shells[0] ?? ""));
  const [administrator, setAdministrator] = useState(false);
  const [createHome, setCreateHome] = useState(true);
  const [saving, setSaving] = useState(false);
  const [fields, setFields] = useState<FieldErrors>({});
  const [general, setGeneral] = useState("");

  const submit = async () => {
    setSaving(true);
    setFields({});
    setGeneral("");
    try {
      const user = await createUser({
        name: name.trim(),
        password,
        full_name: fullName.trim() || undefined,
        shell: shell || undefined,
        administrator,
        create_home: createHome,
      });
      onCreated(user);
    } catch (err) {
      const problems = fieldErrorsOf(err);
      setFields(problems);
      setGeneral(generalErrorOf(err, problems));
      setSaving(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title="Create account"
        subtitle="A local account on this node, which is also an account for this console."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <Field label="Username" htmlFor="user-name" required error={fields.name}>
          <TextInput
            id="user-name"
            value={name}
            onChange={setName}
            mono
            autoFocus
            invalid={!!fields.name}
            placeholder="alice"
          />
        </Field>

        <Field label="Full name" htmlFor="user-full-name" hint="Shown in the table. Optional.">
          <TextInput
            id="user-full-name"
            value={fullName}
            onChange={setFullName}
            placeholder="Alice Kowalski"
          />
        </Field>

        <Field
          label="Password"
          htmlFor="user-password"
          required
          error={fields.password}
          hint="This node's own password rules apply on top; if it refuses, it says why."
        >
          <PasswordInput id="user-password" value={password} onChange={setPassword} />
        </Field>

        <Field label="Login shell" htmlFor="user-shell" error={fields.shell}>
          {shells.length > 0 ? (
            <SelectInput id="user-shell" value={shell} onChange={setShell} mono invalid={!!fields.shell}>
              {shells.map((option) => (
                <option key={option} value={option}>
                  {option}
                </option>
              ))}
            </SelectInput>
          ) : (
            <TextInput
              id="user-shell"
              value={shell}
              onChange={setShell}
              mono
              invalid={!!fields.shell}
              placeholder="/bin/bash"
            />
          )}
        </Field>

        <label className="flex items-center gap-[10px] cursor-pointer select-none">
          <Switch on={administrator} onChange={setAdministrator} />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            Administrator{" "}
            <span className="qz-dim">
              — in <span className="qz-mono">{adminGroup}</span>, and able to administer this
              appliance
            </span>
          </span>
        </label>

        <label className="flex items-center gap-[10px] cursor-pointer select-none">
          <Switch on={createHome} onChange={setCreateHome} />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            Create a home directory{" "}
            <span className="qz-dim">— an account without one cannot hold an SSH key</span>
          </span>
        </label>

        <ErrorText msg={general} />
        <ModalFooter
          onCancel={onClose}
          saving={saving}
          disabled={!name.trim() || !password}
          savingLabel="Creating…"
          submitLabel="Create account"
          onSubmit={submit}
        />
      </div>
    </ModalShell>
  );
}

/// Change one. Every field is optional — an empty password box leaves the
/// password alone rather than clearing it, which is the only safe reading.
export function EditUserDialog({
  user,
  shells,
  adminGroup,
  onClose,
  onSaved,
}: {
  user: UserView;
  shells: string[];
  adminGroup: string;
  onClose: () => void;
  onSaved: (user: UserView) => void;
}) {
  const [fullName, setFullName] = useState(user.full_name ?? "");
  const [shell, setShell] = useState(user.shell);
  const [administrator, setAdministrator] = useState(user.administrator);
  const [password, setPassword] = useState("");
  const [saving, setSaving] = useState(false);
  const [fields, setFields] = useState<FieldErrors>({});
  const [general, setGeneral] = useState("");

  // Only what changed is sent, so a field nobody touched is a field the node
  // is never asked about.
  const patch = {
    ...(fullName !== (user.full_name ?? "") ? { full_name: fullName.trim() } : {}),
    ...(shell !== user.shell ? { shell } : {}),
    ...(administrator !== user.administrator ? { administrator } : {}),
    ...(password ? { password } : {}),
  };
  const nothingToDo = Object.keys(patch).length === 0;

  const submit = async () => {
    setSaving(true);
    setFields({});
    setGeneral("");
    try {
      onSaved(await updateUser(user.name, patch));
    } catch (err) {
      const problems = fieldErrorsOf(err);
      setFields(problems);
      setGeneral(generalErrorOf(err, problems));
      setSaving(false);
    }
  };

  const demotingYourself = user.is_you && user.administrator && !administrator;

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Edit ${user.name}`}
        subtitle={`Account ${user.uid} on this node.`}
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <Field label="Full name" htmlFor="edit-full-name">
          <TextInput id="edit-full-name" value={fullName} onChange={setFullName} />
        </Field>

        <Field label="Login shell" htmlFor="edit-shell" error={fields.shell}>
          {shells.length > 0 ? (
            <SelectInput id="edit-shell" value={shell} onChange={setShell} mono invalid={!!fields.shell}>
              {/* The account's current shell may not be in the list — an
                  operator who set /sbin/nologin by hand should still see it
                  rather than have the drop-down silently choose something
                  else. */}
              {(shells.includes(shell) ? shells : [shell, ...shells]).map((option) => (
                <option key={option} value={option}>
                  {option}
                </option>
              ))}
            </SelectInput>
          ) : (
            <TextInput id="edit-shell" value={shell} onChange={setShell} mono />
          )}
        </Field>

        <Field
          label="New password"
          htmlFor="edit-password"
          error={fields.password}
          hint="Leave empty to keep the current one."
        >
          <PasswordInput id="edit-password" value={password} onChange={setPassword} />
        </Field>

        <label className="flex items-center gap-[10px] cursor-pointer select-none">
          <Switch on={administrator} onChange={setAdministrator} />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            Administrator <span className="qz-dim">— in {adminGroup}</span>
          </span>
        </label>

        {demotingYourself && (
          <div className="callout callout-warn">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              This is the account you are signed in as. The node will refuse to take its
              administrator rights away — that would end this session with no way back in.
            </div>
          </div>
        )}

        <ErrorText msg={general} />
        <ModalFooter
          onCancel={onClose}
          saving={saving}
          disabled={nothingToDo}
          savingLabel="Saving…"
          submitLabel="Save changes"
          onSubmit={submit}
        />
      </div>
    </ModalShell>
  );
}

/// Remove one. Two decisions, not one: whether the account goes, and whether
/// everything it owns does — the same shape the machine delete keeps.
export function DeleteUserDialog({
  user,
  onClose,
  onDeleted,
}: {
  user: UserView;
  onClose: () => void;
  onDeleted: (response: DeleteUserResponse) => void;
}) {
  const [removeHome, setRemoveHome] = useState(false);
  const [acked, setAcked] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");

  const submit = async () => {
    setSaving(true);
    setError("");
    try {
      onDeleted(await deleteUser(user.name, removeHome, acked));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not remove the account.");
      setSaving(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Remove ${user.name}?`}
        subtitle={`Account ${user.uid} on this node.`}
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
          The account is removed from this node, and with it the ability to sign in to this console
          with it. Its home directory is kept unless you say otherwise — removing an account is not
          the same decision as destroying what it owns.
        </p>
        <label className="flex items-center gap-[10px] cursor-pointer select-none">
          <Switch on={removeHome} onChange={setRemoveHome} />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            Also destroy <span className="qz-mono">{user.home}</span>
          </span>
        </label>
        {removeHome && (
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
        )}
        <ErrorText msg={error} />
        <ModalFooter
          onCancel={onClose}
          saving={saving}
          disabled={removeHome && !acked}
          savingLabel="Removing…"
          submitLabel="Remove account"
          onSubmit={submit}
        />
      </div>
    </ModalShell>
  );
}
