"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle } from "lucide-react";
import { useRouter } from "next/navigation";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, type Column, type FilterDef } from "@/components/console/DataTable";
import { ApiError } from "@/lib/authClient";
import { vmHref } from "@/lib/nav";
import { shortNodeName } from "@/lib/nodeNames";
import { formatMoment } from "@/lib/systemClient";
import {
  fetchEnvironmentTasks,
  type MemberTasks,
  type TaskView,
} from "@/lib/vmClient";

/// How much history to ask each member for. The backend caps it at 500; this
/// is a page of activity rather than an archive, and a window that has to be
/// merged across members is one an operator scrolls, not pages.
const WINDOW = 200;

const POLL_MS = 15000;

/// Everything that has happened across the environment, newest first.
///
/// Each node keeps its own log — what was asked of its machines, and what
/// happened to the node itself — and no node can answer for another. So this
/// page asks all of them and interleaves the answers, which is the shape the
/// question actually has: an operator asking "what happened" rarely knows
/// which node it happened on, and that is usually the thing they are trying to
/// find out.
///
/// The Node column is therefore not decoration. It is the answer to half the
/// question, and it is filterable for the other half.
export default function LogsPage() {
  const router = useRouter();
  const [members, setMembers] = useState<MemberTasks[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const answer = await fetchEnvironmentTasks(WINDOW);
      setMembers(answer.members);
      setError(null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the log.");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

  /// Every member's window, merged into one ordering.
  ///
  /// By the moment each thing happened, and against the clock of the node it
  /// happened on — which is the only clock that recorded it. Two nodes a
  /// second apart will interleave a second wrong, and that is the honest
  /// amount of wrong: the alternative is a page that reorders history to
  /// agree with the browser.
  const rows = useMemo<LogRow[]>(() => {
    const merged = (members ?? []).flatMap((member) =>
      (member.tasks ?? []).map((task) => ({
        key: `${member.node}/${task.id}`,
        node: member.node,
        task,
      })),
    );
    return merged.sort((a, b) => b.task.time - a.task.time || b.task.id - a.task.id);
  }, [members]);

  const silent = (members ?? []).filter((member) => !member.reachable);

  return (
    <Page>
      <PageHeader
        title="Logs"
        description="What has been done across this environment, newest first — by whom, on which node, and whether it worked."
      />
      <PageBody>
        <div className="flex flex-col gap-4">
          {error && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
            </div>
          )}

          {/* A member that could not be asked contributes no rows, so the
              reason is said here rather than leaving a history that is quietly
              short — the missing entries are exactly the ones an operator
              chasing a problem is looking for. */}
          {silent.length > 0 && (
            <div className="callout callout-warn">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                {silent.map((member) => shortNodeName(member.node)).join(", ")} could not be asked,
                so nothing {silent.length === 1 ? "it did is" : "they did is"} listed here.
                {silent[0]?.error && <span className="text-[var(--qz-fg-4)]"> {silent[0].error}</span>}
              </div>
            </div>
          )}

          <LogTable
            rows={rows}
            loading={members === null}
            onRefresh={load}
            onOpenVm={(vmid) => router.push(vmHref(vmid, "tasks"))}
          />
        </div>
      </PageBody>
    </Page>
  );
}

/// One entry, with the node that recorded it — the fact the record itself does
/// not carry, because a log a node keeps about itself has no reason to repeat
/// its own name on every line.
interface LogRow {
  key: string;
  node: string;
  task: TaskView;
}

function LogTable({
  rows,
  loading,
  onRefresh,
  onOpenVm,
}: {
  rows: LogRow[];
  loading: boolean;
  onRefresh: () => void;
  onOpenVm: (vmid: number) => void;
}) {
  const columns: Column<LogRow>[] = [
    {
      key: "status",
      header: "Status",
      value: (row) => row.task.status,
      width: 90,
      render: (row) =>
        row.task.status === "ok" ? (
          <span className="badge badge-ok">OK</span>
        ) : (
          <span className="badge badge-crit" title={row.task.error ?? undefined}>
            error
          </span>
        ),
    },
    {
      key: "time",
      header: "Time",
      value: (row) => row.task.time,
      sortable: true,
      width: 180,
      render: (row) => <span className="qz-mono">{formatMoment(row.task.time)}</span>,
    },
    {
      key: "node",
      header: "Node",
      value: (row) => row.node,
      sortable: true,
      width: 150,
      render: (row) => (
        <span className="qz-mono text-[12px] truncate" title={row.node}>
          {shortNodeName(row.node)}
        </span>
      ),
    },
    {
      key: "vmid",
      header: "Machine",
      // Sorts the node's own entries together at one end rather than
      // scattering them through the machines.
      value: (row) => row.task.vmid ?? -1,
      width: 120,
      render: (row) =>
        row.task.vmid == null ? (
          // Not a gap: this happened to the node, and saying "the node" is
          // more use than an em dash an operator has to interpret.
          <span className="qz-dim">the node</span>
        ) : (
          <span className="qz-mono">{row.task.vmid}</span>
        ),
    },
    {
      key: "action",
      header: "Action",
      value: (row) => row.task.action,
      sortable: true,
      mono: true,
      width: 140,
    },
    {
      key: "user",
      header: "User",
      value: (row) => row.task.user,
      sortable: true,
      mono: true,
      width: 170,
    },
    {
      key: "detail",
      header: "Description",
      value: (row) => `${row.task.detail} ${row.task.error ?? ""}`,
      width: 420,
      render: (row) => (
        <span title={row.task.error ? `${row.task.detail} — ${row.task.error}` : row.task.detail}>
          {row.task.detail}
          {row.task.error && <span className="qz-dim"> — {row.task.error}</span>}
        </span>
      ),
    },
  ];

  const filters: FilterDef<LogRow>[] = useMemo(() => {
    const nodes = Array.from(new Set(rows.map((row) => row.node))).sort();
    const actions = Array.from(new Set(rows.map((row) => row.task.action))).sort();
    return [
      ...(nodes.length > 1
        ? [
            {
              key: "node",
              label: "Node",
              options: nodes.map((node) => ({ value: node, label: shortNodeName(node) })),
              predicate: (row: LogRow, value: string) => row.node === value,
            },
          ]
        : []),
      {
        key: "what",
        label: "About",
        options: [
          { value: "node", label: "The nodes" },
          { value: "machine", label: "Machines" },
        ],
        predicate: (row, value) =>
          value === "node" ? row.task.vmid == null : row.task.vmid != null,
      },
      {
        key: "status",
        label: "Status",
        options: [
          { value: "error", label: "Errors only" },
          { value: "ok", label: "Succeeded" },
        ],
        predicate: (row, value) => row.task.status === value,
      },
      {
        key: "action",
        label: "Action",
        options: actions.map((action) => ({ value: action, label: action })),
        predicate: (row, value) => row.task.action === value,
      },
    ];
  }, [rows]);

  return (
    <DataTable
      rows={rows}
      columns={columns}
      filters={filters}
      rowId={(row) => row.key}
      storageKey="system-logs"
      searchPlaceholder="Search the log…"
      emptyMessage={loading ? "Reading the log…" : "Nothing has been done yet."}
      onRefresh={onRefresh}
      // The whole of a machine's history is one double-click away, on the
      // machine it belongs to. An entry about a node has nowhere else to go —
      // this page is where it lives.
      onRowOpen={(row) => {
        if (row.task.vmid != null) onOpenVm(row.task.vmid);
      }}
    />
  );
}
