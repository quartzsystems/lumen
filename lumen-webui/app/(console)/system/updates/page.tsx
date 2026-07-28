"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  Download,
  RefreshCw,
  RotateCw,
  ShieldAlert,
  Wrench,
} from "lucide-react";
import Link from "next/link";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { Button } from "@/components/ui/Button";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { ErrorText, ModalFooter } from "@/components/ui/formkit";
import { DataTable, type Column } from "@/components/console/DataTable";
import { Fact, Facts, Mono, Panel } from "@/components/vm/VmBits";
import { useConsole } from "@/lib/ConsoleContext";
import { ApiError } from "@/lib/authClient";
import { formatMoment } from "@/lib/systemClient";
import {
  KIND_LABEL,
  KIND_TONE,
  checkUpdates,
  fetchUpdateProgress,
  fetchUpdates,
  formatAgo,
  formatElapsed,
  installPlatform,
  installUpdates,
  summarize,
  type Update,
  type UpdateProgress,
  type UpdateView,
} from "@/lib/updateClient";

/// Installing updates on this node.
///
/// The page is two decisions, deliberately kept apart and never joined into
/// one button.
///
/// **Updates** is everything in userland — Lumen's own packages and the
/// distribution's. Installing them cannot move the kernel, because the
/// transaction the backend builds excludes the whole platform set by name.
///
/// **Kernel and storage modules** is the other one. On this appliance the root
/// file system is ZFS and ZFS is an out-of-tree module tracking the kernel's
/// ABI; so is DRBD. They have to move together, and the backend refuses to
/// install any of them unless the package manager has already confirmed, in a
/// dry run, that it can move them all. When it cannot, this page says so and
/// offers nothing — that is the state that would otherwise end with a node
/// that cannot import its pool at the next restart.
///
/// Nothing here restarts anything. A new kernel is installed and not running
/// until somebody says so on Maintenance, which is where the cluster quorum
/// guard and the drain already live.
export default function UpdatesPage() {
  const { setToast } = useConsole();
  const [view, setView] = useState<UpdateView | null>(null);
  const [progress, setProgress] = useState<UpdateProgress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [confirmingPlatform, setConfirmingPlatform] = useState(false);
  /// The browser's clock when the node's answer arrived, so "last checked"
  /// ages against the node's clock rather than against a workstation that is a
  /// minute out — the same discipline the power page uses.
  const [, setTick] = useState(0);
  const polling = useRef(false);

  const refresh = useCallback(async () => {
    try {
      const [next, job] = await Promise.all([fetchUpdates(), fetchUpdateProgress()]);
      setView(next);
      setProgress(job);
      setError(null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read this node's updates.");
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // A running transaction is polled; a finished one is not. The elapsed
  // counter still needs a re-render every second, which is what the tick is.
  useEffect(() => {
    const timer = setInterval(() => {
      setTick((n) => n + 1);
      if (progress?.phase === "running" && !polling.current) {
        polling.current = true;
        void (async () => {
          try {
            const job = await fetchUpdateProgress();
            setProgress(job);
            // The moment it finishes, re-read what is left waiting.
            if (job && job.phase !== "running") void refresh();
          } catch {
            // A poll that fails changes nothing; the next one will say.
          } finally {
            polling.current = false;
          }
        })();
      }
    }, 1000);
    return () => clearInterval(timer);
  }, [progress?.phase, refresh]);

  const check = async () => {
    setChecking(true);
    try {
      setView(await checkUpdates());
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not reach the repositories.");
    } finally {
      setChecking(false);
    }
  };

  const install = async (platform: boolean, acknowledged = false) => {
    setBusy(true);
    try {
      setProgress(platform ? await installPlatform(acknowledged) : await installUpdates());
      setConfirmingPlatform(false);
      setToast("Installing. This page shows it through to the end.");
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Could not start the update.");
    } finally {
      setBusy(false);
    }
  };

  const running = progress?.phase === "running";
  const nowSeconds = Math.floor(Date.now() / 1000);
  const platform = view?.platform;
  const ordinaryCount = view?.updates.length ?? 0;

  return (
    <Page>
      <PageHeader
        title="Updates"
        description="What this node could install, and installing it."
      />
      <PageBody>
        <div className="flex flex-col gap-4">
          {error && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
            </div>
          )}

          {/* The last check failed but the page still renders — including the
              restart notice below, which is read from the node itself and is
              often exactly what an operator in this state needs to see. */}
          {view?.error && !error && (
            <div className="callout callout-warn">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                The last check did not finish: {view.error}
              </div>
            </div>
          )}

          {view?.reboot.required && (
            <div className="callout callout-warn">
              <RotateCw size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="flex-1 min-w-0">
                <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
                  This node needs restarting to finish an update
                </div>
                <div className="text-[13px] text-[var(--qz-fg-3)] mt-1">
                  {view.reboot.reason}
                </div>
              </div>
              <Link href="/system/maintenance" className="flex-shrink-0">
                <Button kind="secondary" icon={Wrench}>
                  Maintenance
                </Button>
              </Link>
            </div>
          )}

          {progress && <TransactionCard progress={progress} nowSeconds={nowSeconds} />}

          <Panel
            title="This node"
            actions={
              <Button kind="secondary" icon={RefreshCw} disabled={checking || running} onClick={check}>
                {checking ? "Checking…" : "Check Now"}
              </Button>
            }
          >
            <Facts>
              <Fact label="Node">
                <Mono>{view?.node ?? "—"}</Mono>
              </Fact>
              <Fact label="Last checked">
                {view?.checked_at ? (
                  <span title={formatMoment(view.checked_at)}>
                    {formatAgo(view.checked_at, nowSeconds)}
                  </span>
                ) : (
                  <span className="qz-dim">never</span>
                )}
              </Fact>
              <Fact label="Waiting">{view ? summarize(view) : "—"}</Fact>
              <Fact label="Running kernel">
                <Mono>{view?.reboot.kernel.running || "—"}</Mono>
              </Fact>
              <Fact label="Newest installed">
                {view?.reboot.kernel.newest ? (
                  <Mono>{view.reboot.kernel.newest}</Mono>
                ) : (
                  <span className="qz-dim">unknown</span>
                )}
              </Fact>
            </Facts>
          </Panel>

          <Panel
            title="Updates"
            actions={
              <Button
                icon={Download}
                disabled={busy || running || ordinaryCount === 0}
                onClick={() => void install(false)}
              >
                Install {ordinaryCount > 0 ? `${ordinaryCount} ` : ""}Update
                {ordinaryCount === 1 ? "" : "s"}
              </Button>
            }
          >
            <p className="text-[13px] text-[var(--qz-fg-3)] m-0 mb-4">
              Lumen&apos;s own packages and the rest of the system. Installing these never moves the
              kernel or the storage modules — those are the set below, and they are installed
              separately on purpose.
            </p>
            <UpdatesTable rows={view?.updates ?? []} onRefresh={check} storageKey="updates" />
          </Panel>

          <Panel
            title="Kernel and storage modules"
            actions={
              platform?.updates.length ? (
                <span title={platform.resolves ? undefined : "The package manager cannot install these together yet."}>
                  <Button
                    kind="secondary"
                    icon={Download}
                    disabled={busy || running || !platform.resolves}
                    onClick={() => setConfirmingPlatform(true)}
                  >
                    Install {platform.updates.length} Package
                    {platform.updates.length === 1 ? "" : "s"}
                  </Button>
                </span>
              ) : undefined
            }
          >
            <p className="text-[13px] text-[var(--qz-fg-3)] m-0 mb-4">
              This node boots from ZFS, and ZFS — like the replication module — is built against one
              exact kernel. They move as one set or not at all, and Lumen installs them only after
              the package manager has confirmed it can move all of them together.
            </p>

            {platform && !platform.resolves && platform.updates.length > 0 && (
              <div className="callout callout-warn mb-4">
                <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
                <div className="flex-1 min-w-0">
                  <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
                    These cannot be installed together yet
                  </div>
                  <div className="text-[13px] text-[var(--qz-fg-3)] mt-1">
                    Usually the storage module has not caught up with a new kernel yet, which
                    happens for a few days after a point release. Nothing is installed until it
                    has — installing the kernel on its own is what leaves a node unable to import
                    its pool. The package manager said:
                  </div>
                  <div className="text-[12px] qz-mono text-[var(--qz-fg-3)] mt-2 break-words">
                    {platform.detail}
                  </div>
                </div>
              </div>
            )}

            {platform && platform.resolves && platform.updates.length > 0 && (
              <div className="callout callout-ok mb-4">
                <CheckCircle2 size={17} className="flex-shrink-0 text-[var(--qz-success)] mt-[1px]" />
                <div className="text-[13px] text-[var(--qz-fg-2)]">
                  The package manager can install these together. The node keeps running its
                  current kernel until it is restarted.
                </div>
              </div>
            )}

            <UpdatesTable
              rows={platform?.updates ?? []}
              onRefresh={check}
              storageKey="updates-platform"
            />
          </Panel>
        </div>
      </PageBody>

      {confirmingPlatform && platform && (
        <ConfirmPlatformDialog
          count={platform.updates.length}
          names={platform.updates.map((u) => u.name)}
          working={busy}
          onClose={() => setConfirmingPlatform(false)}
          onConfirm={() => void install(true, true)}
        />
      )}
    </Page>
  );
}

/// One transaction, running or finished.
function TransactionCard({
  progress,
  nowSeconds,
}: {
  progress: UpdateProgress;
  nowSeconds: number;
}) {
  const elapsed = (progress.finished_at ?? nowSeconds) - progress.started_at;
  const what = progress.kind === "platform" ? "kernel and storage modules" : "updates";

  if (progress.phase === "running") {
    return (
      <div className="callout callout-info">
        <RefreshCw
          size={17}
          className="flex-shrink-0 text-[var(--qz-info)] mt-[1px] animate-spin"
        />
        <div className="flex-1 min-w-0">
          <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
            Installing {what} — {formatElapsed(Math.max(0, elapsed))}
          </div>
          <div className="text-[13px] text-[var(--qz-fg-3)] mt-1">
            Started by {progress.by}. The package manager reports nothing until the transaction
            finishes, so there is no progress bar to show — leaving this page does not stop it.
          </div>
        </div>
      </div>
    );
  }

  const failed = progress.phase === "failed";
  return (
    <div className={`callout ${failed ? "callout-crit" : "callout-ok"}`}>
      {failed ? (
        <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
      ) : (
        <CheckCircle2 size={17} className="flex-shrink-0 text-[var(--qz-success)] mt-[1px]" />
      )}
      <div className="flex-1 min-w-0">
        <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
          {failed
            ? `Installing ${what} did not finish`
            : `Installed ${progress.changed.length} package${
                progress.changed.length === 1 ? "" : "s"
              } in ${formatElapsed(Math.max(0, elapsed))}`}
        </div>
        {failed ? (
          <div className="text-[13px] text-[var(--qz-fg-3)] mt-1">{progress.error}</div>
        ) : (
          progress.changed.length > 0 && (
            <div className="text-[12px] qz-mono text-[var(--qz-fg-3)] mt-1 break-words">
              {progress.changed.join(", ")}
            </div>
          )
        )}
        {progress.log && (
          <details className="mt-2">
            <summary className="text-[12px] text-[var(--qz-fg-3)] cursor-pointer">
              What the package manager said
            </summary>
            <pre className="text-[11px] qz-mono text-[var(--qz-fg-3)] mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words">
              {progress.log}
            </pre>
          </details>
        )}
      </div>
    </div>
  );
}

/// The table both panels use. One definition, because a package waiting to be
/// installed reads the same either way — what differs is the decision above it,
/// not the columns.
function UpdatesTable({
  rows,
  onRefresh,
  storageKey,
}: {
  rows: Update[];
  onRefresh: () => void;
  storageKey: string;
}) {
  const columns: Column<Update>[] = [
    {
      key: "name",
      header: "Package",
      value: (row) => row.name,
      mono: true,
      width: 240,
      render: (row) => (
        <span className="inline-flex items-center gap-2 min-w-0">
          <span className="qz-mono truncate">{row.name}</span>
          {row.security && (
            <span className="badge badge-crit inline-flex items-center gap-1" title={row.advisory ?? undefined}>
              <ShieldAlert size={11} />
              security
            </span>
          )}
        </span>
      ),
    },
    {
      key: "installed",
      header: "Installed",
      value: (row) => row.installed ?? "",
      mono: true,
      width: 160,
      render: (row) =>
        row.installed ? <Mono>{row.installed}</Mono> : <span className="qz-dim">not installed</span>,
    },
    {
      key: "version",
      header: "Available",
      value: (row) => row.version,
      mono: true,
      width: 160,
    },
    {
      key: "kind",
      header: "Kind",
      value: (row) => KIND_LABEL[row.kind],
      width: 140,
      render: (row) => (
        <span className={`badge badge-${KIND_TONE[row.kind]}`}>{KIND_LABEL[row.kind]}</span>
      ),
    },
    { key: "repo", header: "From", value: (row) => row.repo, mono: true, width: 150 },
    {
      key: "advisory",
      header: "Advisory",
      value: (row) => row.advisory ?? "",
      mono: true,
      width: 160,
      render: (row) =>
        row.advisory ? <Mono>{row.advisory}</Mono> : <span className="qz-dim">—</span>,
    },
  ];

  return (
    <DataTable
      rows={rows}
      columns={columns}
      rowId={(row) => `${row.name}.${row.arch}`}
      searchPlaceholder="Search packages…"
      emptyMessage="Nothing waiting."
      onRefresh={onRefresh}
      storageKey={storageKey}
    />
  );
}

/// The confirmation in front of a platform install.
///
/// A checkbox rather than typing the node's name, and that is a deliberate
/// difference from the restart dialog. Installing these packages is not the
/// dangerous step — the node carries on running its current kernel afterwards,
/// and the restart that makes them live has its own confirmation, its own
/// quorum guard, and its own drain. What this dialog has to do is make sure
/// nobody presses it thinking it is the ordinary button.
function ConfirmPlatformDialog({
  count,
  names,
  working,
  onClose,
  onConfirm,
}: {
  count: number;
  names: string[];
  working: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const [acknowledged, setAcknowledged] = useState(false);
  const [error] = useState("");

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Install ${count} kernel and storage package${count === 1 ? "" : "s"}?`}
        subtitle="They move as one set. Nothing restarts."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <div className="callout callout-warn">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            This replaces the kernel and the storage modules built against it. The package manager
            has already confirmed it can install them together — that check is why this button is
            offered at all. Afterwards this node keeps running its <strong>current</strong> kernel
            until it is restarted from Maintenance, where its machines are moved off first.
          </div>
        </div>

        <div className="text-[12px] qz-mono text-[var(--qz-fg-3)] break-words">
          {names.join(", ")}
        </div>

        <label className="flex items-start gap-2 text-[13px] text-[var(--qz-fg-2)] cursor-pointer">
          <input
            type="checkbox"
            checked={acknowledged}
            onChange={(e) => setAcknowledged(e.target.checked)}
            className="mt-[3px]"
          />
          <span>
            I understand the kernel and storage modules are being replaced, and that this node has
            to be restarted before they take effect.
          </span>
        </label>

        <ErrorText msg={error} />
        <ModalFooter
          onCancel={onClose}
          saving={working}
          disabled={!acknowledged}
          savingLabel="Starting…"
          submitLabel="Install"
          onSubmit={onConfirm}
        />
      </div>
    </ModalShell>
  );
}
