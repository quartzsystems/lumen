"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle } from "lucide-react";
import { DataTable, type Column } from "@/components/console/DataTable";
import { fetchVmTasks, type TaskView, type VmView } from "@/lib/vmClient";

/// Matches lib/VmContext.tsx: often enough that history feels live, slow
/// enough to cost nothing.
const POLL_MS = 5000;

/// When, as an operator reads it: absolute, in the node's own locale, because
/// "3 minutes ago" is useless in an incident review.
const formatTime = (unixSecs: number): string => new Date(unixSecs * 1000).toLocaleString();

const columns: Column<TaskView>[] = [
  {
    key: "time",
    header: "Time",
    value: (task) => task.time,
    render: (task) => <span className="qz-mono">{formatTime(task.time)}</span>,
    sortable: true,
    width: 180,
  },
  {
    key: "action",
    header: "Action",
    value: (task) => task.action,
    mono: true,
    sortable: true,
    width: 120,
  },
  {
    key: "detail",
    header: "Description",
    value: (task) => task.detail,
    render: (task) => (
      <span title={task.error ?? undefined}>
        {task.detail}
        {task.error && <span className="qz-dim"> — {task.error}</span>}
      </span>
    ),
  },
  {
    key: "user",
    header: "User",
    value: (task) => task.user,
    mono: true,
    sortable: true,
    width: 140,
  },
  {
    key: "status",
    header: "Status",
    value: (task) => task.status,
    render: (task) =>
      task.status === "ok" ? (
        <span className="badge badge-ok">OK</span>
      ) : (
        <span className="badge badge-crit" title={task.error ?? undefined}>
          error
        </span>
      ),
    sortable: true,
    width: 90,
  },
];

/// Everything that has been asked of this machine, newest first — starts,
/// stops, and reconfigurations, refusals included. The list comes from the
/// control plane's own task log, so it says who asked and what the node
/// answered rather than guessing from the machine's state.
export function VmTasks({ vm }: { vm: VmView }) {
  const [tasks, setTasks] = useState<TaskView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  // The section's own fetch: task history is not part of VmView, and dragging
  // it into every 5-second machine poll would tax the common case for a page
  // most sessions never open.
  const vmid = vm.vmid;
  const alive = useRef(true);

  const load = useCallback(async () => {
    try {
      const response = await fetchVmTasks(vmid);
      if (!alive.current) return;
      setTasks(response.tasks);
      setError(null);
    } catch (err) {
      if (!alive.current) return;
      setError(err instanceof Error ? err.message : "Could not read the task history.");
    }
  }, [vmid]);

  useEffect(() => {
    alive.current = true;
    void load();
    const timer = setInterval(() => void load(), POLL_MS);
    return () => {
      alive.current = false;
      clearInterval(timer);
    };
  }, [load]);

  const filters = [
    {
      key: "status",
      label: "Status",
      options: [
        { value: "ok", label: "OK" },
        { value: "error", label: "error" },
      ],
      predicate: (task: TaskView, value: string) => task.status === value,
    },
  ];

  return (
    <div className="flex flex-col gap-4">
      {error && (
        <div className="callout callout-crit">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
        </div>
      )}
      {tasks === null && !error ? (
        <div className="text-[13px] text-[var(--qz-fg-4)]">Reading the history…</div>
      ) : (
        <DataTable
          rows={tasks ?? []}
          columns={columns}
          filters={filters}
          rowId={(task) => String(task.id)}
          storageKey="vm-tasks"
          searchPlaceholder="Search tasks…"
          emptyMessage="Nothing has been done to this machine yet."
          onRefresh={load}
        />
      )}
    </div>
  );
}
