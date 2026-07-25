"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Lock, Pencil, Plus, Trash2, Unlock } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column, type FilterDef } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import {
  CreateUserDialog,
  DeleteUserDialog,
  EditUserDialog,
} from "@/components/system/UserDialogs";
import { useConsole } from "@/lib/ConsoleContext";
import { ApiError } from "@/lib/authClient";
import {
  fetchUsers,
  updateUser,
  LOGIN_LABEL,
  LOGIN_TONE,
  type UserView,
  type UsersResponse,
} from "@/lib/systemClient";

/// The accounts this node can be signed in to, and the console with it.
///
/// The `lumen` realm authenticates against PAM — that is, against this node's
/// own accounts — so there is no separate console user list to keep in step.
/// An account made here is an account at the keyboard, over SSH, and on this
/// page, which is the whole reason the page manages the node's accounts rather
/// than inventing its own.
export default function AuthenticationPage() {
  const { setToast } = useConsole();
  const [data, setData] = useState<UsersResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<UserView | null>(null);
  const [deleting, setDeleting] = useState<UserView | null>(null);

  const refresh = useCallback(async () => {
    try {
      setData(await fetchUsers());
      setError(null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read this node's accounts.");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Deliberately not polled. The account files change only when somebody
  // changes them, and a table that reorders itself under a cursor for no
  // reason is worse than one that needs Refresh.
  const users = data?.users ?? [];
  const shells = data?.shells ?? [];
  const adminGroup = data?.admin_group ?? "wheel";

  const setLocked = async (user: UserView, locked: boolean) => {
    setBusy(true);
    try {
      await updateUser(user.name, { locked });
      setToast(`${user.name} ${locked ? "locked" : "unlocked"}.`);
      await refresh();
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Something went wrong.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Page>
      <PageHeader
        title="Authentication"
        description="The local accounts on this node. Signing in to this console uses them, so an account here is an account at the keyboard and over SSH too."
      />
      <PageBody>
        <div className="flex flex-col gap-4">
          {error && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
            </div>
          )}

          {loading && users.length === 0 && !error && (
            <div className="text-[13px] text-[var(--qz-fg-4)]">Reading this node&rsquo;s accounts…</div>
          )}

          <UsersTable
            rows={users}
            busy={busy}
            adminGroup={adminGroup}
            onRefresh={refresh}
            onCreate={() => setCreating(true)}
            onEdit={setEditing}
            onDelete={setDeleting}
            onLock={(user) => setLocked(user, true)}
            onUnlock={(user) => setLocked(user, false)}
          />

          <p className="text-[12px] text-[var(--qz-fg-4)] m-0">
            Accounts are this node&rsquo;s own — <span className="qz-mono">/etc/passwd</span>,{" "}
            <span className="qz-mono">/etc/shadow</span>, and{" "}
            <span className="qz-mono">/etc/group</span> — so <span className="qz-mono">getent</span>{" "}
            and this page can never disagree. Membership of{" "}
            <span className="qz-mono">{adminGroup}</span> is what grants administrative rights.{" "}
            <span className="qz-mono">root</span> is shown but is not changed from here: it is the
            account this appliance is recovered with.
          </p>
        </div>
      </PageBody>

      {creating && (
        <CreateUserDialog
          shells={shells}
          adminGroup={adminGroup}
          onClose={() => setCreating(false)}
          onCreated={async (user) => {
            setCreating(false);
            setToast(`${user.name} created.`);
            await refresh();
          }}
        />
      )}

      {editing && (
        <EditUserDialog
          key={editing.name}
          user={editing}
          shells={shells}
          adminGroup={adminGroup}
          onClose={() => setEditing(null)}
          onSaved={async (user) => {
            setEditing(null);
            setToast(`${user.name} updated.`);
            await refresh();
          }}
        />
      )}

      {deleting && (
        <DeleteUserDialog
          user={deleting}
          onClose={() => setDeleting(null)}
          onDeleted={async (response) => {
            setDeleting(null);
            setToast(
              response.removed_home
                ? `${response.name} removed, along with ${response.removed_home}.`
                : `${response.name} removed. ${response.kept_home ?? "Its files"} kept.`,
            );
            await refresh();
          }}
        />
      )}
    </Page>
  );
}

// --- the table ---------------------------------------------------------------

function UsersTable({
  rows,
  busy,
  adminGroup,
  onRefresh,
  onCreate,
  onEdit,
  onDelete,
  onLock,
  onUnlock,
}: {
  rows: UserView[];
  busy: boolean;
  adminGroup: string;
  onRefresh: () => Promise<void>;
  onCreate: () => void;
  onEdit: (user: UserView) => void;
  onDelete: (user: UserView) => void;
  onLock: (user: UserView) => void;
  onUnlock: (user: UserView) => void;
}) {
  const columns: Column<UserView>[] = useMemo(
    () => [
      {
        key: "name",
        header: "Account",
        value: (user) => user.name,
        sortable: true,
        width: 180,
        render: (user) => (
          <span className="inline-flex items-center gap-2 min-w-0">
            <span
              className="text-[var(--qz-fg-1)] font-semibold truncate"
              style={{ fontFamily: "var(--qz-font-mono)" }}
            >
              {user.name}
            </span>
            {user.is_you && <span className="badge badge-info">you</span>}
          </span>
        ),
      },
      {
        key: "uid",
        header: "UID",
        value: (user) => user.uid,
        sortable: true,
        mono: true,
        width: 80,
      },
      {
        key: "full_name",
        header: "Full name",
        value: (user) => user.full_name ?? "",
        sortable: true,
        width: 190,
        render: (user) => (user.full_name ? <>{user.full_name}</> : <Dash />),
      },
      {
        key: "login",
        header: "Sign-in",
        value: (user) => LOGIN_LABEL[user.login],
        sortable: true,
        width: 130,
        render: (user) => (
          <span className={`badge badge-${LOGIN_TONE[user.login]}`}>{LOGIN_LABEL[user.login]}</span>
        ),
      },
      {
        key: "administrator",
        header: "Role",
        value: (user) => (user.administrator ? "administrator" : "user"),
        sortable: true,
        width: 130,
        render: (user) =>
          user.administrator ? (
            <span className="badge badge-ok" title={`in ${adminGroup}`}>
              administrator
            </span>
          ) : (
            <span className="badge badge-muted">user</span>
          ),
      },
      {
        key: "shell",
        header: "Shell",
        value: (user) => user.shell,
        mono: true,
        sortable: true,
        width: 150,
      },
      {
        key: "home",
        header: "Home",
        value: (user) => user.home,
        mono: true,
        sortable: true,
        width: 160,
      },
      {
        key: "groups",
        header: "Groups",
        value: (user) => user.groups.join(", "),
        width: 180,
        render: (user) =>
          user.groups.length === 0 ? (
            <Dash />
          ) : (
            <span className="inline-flex flex-wrap gap-[6px]">
              {user.groups.map((group) => (
                <span key={group} className="badge badge-muted">
                  {group}
                </span>
              ))}
            </span>
          ),
      },
    ],
    [adminGroup],
  );

  // The filters offer what is actually on this node rather than every value
  // the API can produce — and "people" versus "the operating system's" is the
  // one an operator opening this page nearly always wants.
  const filters: FilterDef<UserView>[] = useMemo(
    () => [
      {
        key: "kind",
        label: "Show",
        options: [
          { value: "people", label: "People" },
          { value: "system", label: "System accounts" },
        ],
        predicate: (user, value) => (value === "system" ? user.system : !user.system),
      },
      {
        key: "role",
        label: "Role",
        options: [
          { value: "administrator", label: "Administrators" },
          { value: "user", label: "Users" },
        ],
        predicate: (user, value) =>
          value === "administrator" ? user.administrator : !user.administrator,
      },
    ],
    [],
  );

  return (
    <DataTable
      rows={rows}
      columns={columns}
      filters={filters}
      rowId={(user) => user.name}
      storageKey="system-authentication"
      searchPlaceholder="Search accounts…"
      emptyMessage="No accounts on this node."
      onRefresh={onRefresh}
      toolbar={
        <Button kind="primary" size="sm" icon={Plus} onClick={onCreate}>
          Create
        </Button>
      }
      // Edit, lock or unlock, and remove — three controls, and the fixed
      // layout will not find the room for them on its own.
      actionsWidth={132}
      actions={(user) => (
        <div className="inline-flex items-center gap-1 justify-end">
          <span title={user.actions.edit.reason}>
            <Button
              kind="ghost"
              size="sm"
              icon={Pencil}
              disabled={busy || !user.actions.edit.allowed}
              onClick={() => onEdit(user)}
            />
          </span>
          {user.login === "locked" ? (
            <span title={user.actions.unlock.reason}>
              <Button
                kind="ghost"
                size="sm"
                icon={Unlock}
                disabled={busy || !user.actions.unlock.allowed}
                onClick={() => onUnlock(user)}
              />
            </span>
          ) : (
            <span title={user.actions.lock.reason}>
              <Button
                kind="ghost"
                size="sm"
                icon={Lock}
                disabled={busy || !user.actions.lock.allowed}
                onClick={() => onLock(user)}
              />
            </span>
          )}
          <span title={user.actions.delete.reason}>
            <Button
              kind="ghost"
              size="sm"
              icon={Trash2}
              disabled={busy || !user.actions.delete.allowed}
              onClick={() => onDelete(user)}
            />
          </span>
        </div>
      )}
    />
  );
}
