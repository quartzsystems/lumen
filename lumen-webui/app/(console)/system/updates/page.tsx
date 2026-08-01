"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  CircleDashed,
  Download,
  Layers,
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
  STAGE_LABEL,
  checkClusterUpdates,
  checkUpdates,
  fetchClusterUpdates,
  fetchRollProgress,
  fetchUpdateProgress,
  fetchUpdates,
  formatAgo,
  formatElapsed,
  installClusterUpdates,
  installPlatform,
  installUpdates,
  rollClusterUpdates,
  summarize,
  type ClusterUpdateView,
  type MemberStep,
  type MemberUpdates,
  type RollProgress,
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
/// ABI. They have to move together, and the backend refuses to
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
  const [cluster, setCluster] = useState<ClusterUpdateView | null>(null);
  const [roll, setRoll] = useState<RollProgress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  const [checkingCluster, setCheckingCluster] = useState(false);
  const [busy, setBusy] = useState(false);
  const [confirmingPlatform, setConfirmingPlatform] = useState(false);
  const [confirmingCluster, setConfirmingCluster] = useState(false);
  const [confirmingRoll, setConfirmingRoll] = useState(false);
  /// The browser's clock when the node's answer arrived, so "last checked"
  /// ages against the node's clock rather than against a workstation that is a
  /// minute out — the same discipline the power page uses.
  const [, setTick] = useState(0);
  const polling = useRef(false);

  const refresh = useCallback(async () => {
    try {
      const [next, job, members, walk] = await Promise.all([
        fetchUpdates(),
        fetchUpdateProgress(),
        fetchClusterUpdates(),
        fetchRollProgress(),
      ]);
      setView(next);
      setProgress(job);
      setCluster(members);
      setRoll(walk);
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
  //
  // A running *walk* is polled together with the member table, because the
  // table is where its progress is actually visible: the walk's own record
  // stops when this node updates itself, and the members' own answers do not.
  const watching = progress?.phase === "running" || roll?.phase === "running";
  useEffect(() => {
    const timer = setInterval(() => {
      setTick((n) => n + 1);
      if (watching && !polling.current) {
        polling.current = true;
        void (async () => {
          try {
            const [job, walk, members] = await Promise.all([
              fetchUpdateProgress(),
              fetchRollProgress(),
              fetchClusterUpdates(),
            ]);
            setProgress(job);
            setRoll(walk);
            setCluster(members);
            // The moment it finishes, re-read what is left waiting.
            if (
              (job === null || job.phase !== "running") &&
              (walk === null || walk.phase !== "running")
            ) {
              void refresh();
            }
          } catch {
            // A poll that fails changes nothing; the next one will say. This
            // is also the ordinary state while this node restarts its own
            // control plane at the end of a walk.
          } finally {
            polling.current = false;
          }
        })();
      }
    }, 1000);
    return () => clearInterval(timer);
  }, [watching, refresh]);

  const checkEveryNode = async () => {
    setCheckingCluster(true);
    try {
      setCluster(await checkClusterUpdates());
      // This node is one of the members that was just asked, so its own panel
      // is stale until it is re-read.
      setView(await fetchUpdates());
      setError(null);
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Could not ask every node.");
    } finally {
      setCheckingCluster(false);
    }
  };

  const updateEveryNode = async (rolling: boolean, acknowledged = false) => {
    setBusy(true);
    try {
      setRoll(rolling ? await rollClusterUpdates(acknowledged) : await installClusterUpdates());
      setConfirmingCluster(false);
      setConfirmingRoll(false);
      setToast(
        rolling
          ? "Rolling through the cluster, one node at a time. This page shows it through to the end."
          : "Updating every node, one at a time. This page shows it through to the end.",
      );
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Could not start the update.");
    } finally {
      setBusy(false);
    }
  };

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
  const rolling = roll?.phase === "running";
  const nowSeconds = Math.floor(Date.now() / 1000);
  const platform = view?.platform;
  const ordinaryCount = view?.updates.length ?? 0;
  /// The cluster panel earns its space only when there is more than one node
  /// to talk about. On a single appliance the panel below says everything it
  /// would, and a table of one row is furniture.
  const members = cluster?.members ?? [];
  const clustered = members.length > 1;
  const clusterWaiting = cluster?.counts.updates ?? 0;
  /// Nodes a rolling update would actually restart: a kernel waiting that can
  /// move, or a restart already owed from an earlier install. A node with only
  /// userland updates is not taken down for them, so it does not count here.
  const needingRestart = members.filter((member) => {
    const view = member.updates?.view;
    if (!view) return false;
    return (view.platform.updates.length > 0 && view.platform.resolves) || view.reboot.required;
  });
  const blockedPlatform = cluster?.counts.platform_blocked ?? 0;

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

          {clustered && (
            <Panel
              title="Every node"
              actions={
                <div className="flex items-center gap-2">
                  <Button
                    kind="secondary"
                    icon={RefreshCw}
                    disabled={checkingCluster || rolling}
                    onClick={() => void checkEveryNode()}
                  >
                    {checkingCluster ? "Checking…" : "Check All"}
                  </Button>
                  <Button
                    kind="secondary"
                    icon={Layers}
                    disabled={busy || rolling || running || clusterWaiting === 0}
                    onClick={() => setConfirmingCluster(true)}
                  >
                    Update All Nodes
                  </Button>
                  <span
                    title={
                      needingRestart.length === 0
                        ? "No node needs a restart, so there is nothing to roll."
                        : undefined
                    }
                  >
                    <Button
                      icon={RotateCw}
                      disabled={busy || rolling || running || needingRestart.length === 0}
                      onClick={() => setConfirmingRoll(true)}
                    >
                      Rolling Update
                    </Button>
                  </span>
                </div>
              }
            >
              <p className="text-[13px] text-[var(--qz-fg-3)] m-0 mb-4">
                {summarizeCluster(cluster)}
              </p>
              <p className="text-[13px] text-[var(--qz-fg-3)] m-0 mb-4">
                <strong>Update All Nodes</strong> installs the ordinary updates on one node at a
                time. Nothing is taken out of service and nothing restarts, so the kernel is not
                moved by it. <strong>Rolling Update</strong> is the one that moves the kernel: each
                node that needs a restart is emptied of its machines, brought fully up to date,
                restarted, and waited for before the next one is touched. Either way, the first
                node that fails stops the rest — an update that will not install on one member
                usually will not install on the next.
              </p>

              {blockedPlatform > 0 && (
                <div className="callout callout-warn mb-4">
                  <AlertTriangle
                    size={17}
                    className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]"
                  />
                  <div className="text-[13px] text-[var(--qz-fg-2)]">
                    {blockedPlatform} node{blockedPlatform === 1 ? "" : "s"} cannot install
                    {blockedPlatform === 1 ? " its " : " their "}kernel and storage modules
                    together yet, so a rolling update would stop there. This is ordinary for a few
                    days after a point release — the storage module has not caught up with the new
                    kernel. Hover the node&apos;s badge for what its package manager said.
                  </div>
                </div>
              )}

              {roll && <RollCard roll={roll} nowSeconds={nowSeconds} />}

              <MembersTable
                members={members}
                roll={roll}
                nowSeconds={nowSeconds}
                onRefresh={() => void refresh()}
              />
            </Panel>
          )}

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

      {confirmingCluster && cluster && (
        <ConfirmClusterDialog
          order={members.map((member) => member.node)}
          localNode={members.find((member) => member.local)?.node ?? ""}
          waiting={clusterWaiting}
          working={busy}
          onClose={() => setConfirmingCluster(false)}
          onConfirm={() => void updateEveryNode(false)}
        />
      )}

      {confirmingRoll && cluster && (
        <ConfirmRollingDialog
          restarting={needingRestart.map((member) => member.node)}
          localNode={members.find((member) => member.local)?.node ?? ""}
          working={busy}
          onClose={() => setConfirmingRoll(false)}
          onConfirm={() => void updateEveryNode(true, true)}
        />
      )}
    </Page>
  );
}

/// The one-line summary above the member table.
function summarizeCluster(cluster: ClusterUpdateView | null): string {
  if (!cluster) return "";
  const { counts } = cluster;
  const unreachable = counts.members - counts.reachable;
  const parts: string[] = [];

  if (counts.updates === 0) {
    parts.push(
      unreachable === counts.members
        ? "No node could be asked what it has waiting."
        : "Every node that answered is up to date.",
    );
  } else {
    parts.push(
      `${counts.updates} update${counts.updates === 1 ? "" : "s"} waiting across ` +
        `${counts.members_with_updates} of ${counts.members} nodes.`,
    );
  }
  if (counts.security > 0) {
    parts.push(`${counts.security} of them are named in a security advisory.`);
  }
  if (unreachable > 0 && unreachable < counts.members) {
    parts.push(
      `${unreachable} node${unreachable === 1 ? "" : "s"} could not be asked, so ` +
        `${unreachable === 1 ? "it is" : "they are"} not counted.`,
    );
  }
  if (counts.restarts_required > 0) {
    parts.push(
      `${counts.restarts_required} ${counts.restarts_required === 1 ? "is" : "are"} waiting on a ` +
        "restart to finish an earlier update.",
    );
  }
  return parts.join(" ");
}

/// One environment-wide update, running or finished.
///
/// Unlike the single-node card, this one has real steps to show: the walk
/// visits members one at a time and knows which one it is on, exactly as a
/// drain does.
function RollCard({ roll, nowSeconds }: { roll: RollProgress; nowSeconds: number }) {
  const elapsed = (roll.finished_at ?? nowSeconds) - roll.started_at;
  const done = roll.steps.filter((step) => step.state === "done").length;
  const current = roll.steps.find((step) => step.state === "running");
  const failed = roll.phase === "failed";
  const what = roll.kind === "rolling" ? "Rolling through the cluster" : "Updating every node";

  const tone = roll.phase === "running" ? "info" : failed ? "crit" : "ok";
  const Icon = roll.phase === "running" ? RefreshCw : failed ? AlertTriangle : CheckCircle2;

  return (
    <>
      <div className={`callout callout-${tone} mb-4`}>
        <Icon
          size={17}
          className={`flex-shrink-0 mt-[1px] text-[var(--qz-${
            tone === "crit" ? "danger" : tone === "ok" ? "success" : "info"
          })] ${roll.phase === "running" ? "animate-spin" : ""}`}
        />
        <div className="flex-1 min-w-0">
          <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
            {roll.phase === "running"
              ? `${what} — ${done} of ${roll.steps.length} finished, ` +
                `${formatElapsed(Math.max(0, elapsed))}`
              : failed
                ? `${what} stopped early`
                : `${roll.kind === "rolling" ? "Rolled through the cluster" : "Every node updated"}` +
                  ` in ${formatElapsed(Math.max(0, elapsed))}`}
          </div>
          <div className="text-[13px] text-[var(--qz-fg-3)] mt-1">
            {failed
              ? roll.error
              : roll.phase === "running"
                ? `Started by ${roll.by}. ${
                    current
                      ? `${current.node} is ${STAGE_LABEL[current.stage]}. `
                      : ""
                  }Nodes are done one at a time; leaving this page does not stop it.`
                : `Started by ${roll.by}.`}
          </div>
        </div>
      </div>

      {/* The one node a rolling update will not restart: the one running it.
          Its own step says so too, but a table cell is the wrong place for a
          sentence the operator has to act on. */}
      {roll.left_to_you && (
        <div className="callout callout-warn mb-4">
          <Wrench size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="flex-1 min-w-0">
            <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
              One node is left for you
            </div>
            <div className="text-[13px] text-[var(--qz-fg-3)] mt-1">{roll.left_to_you}</div>
          </div>
          <Link href="/system/maintenance" className="flex-shrink-0">
            <Button kind="secondary" icon={Wrench}>
              Maintenance
            </Button>
          </Link>
        </div>
      )}
    </>
  );
}

/// The member table.
///
/// The walk's step for a member is folded into its row rather than shown as a
/// second list, because they are the same fact seen twice — and while the walk
/// is running, the step is the more current of the two.
function MembersTable({
  members,
  roll,
  nowSeconds,
  onRefresh,
}: {
  members: MemberUpdates[];
  roll: RollProgress | null;
  nowSeconds: number;
  onRefresh: () => void;
}) {
  const stepFor = (node: string): MemberStep | undefined =>
    roll?.steps.find((step) => step.node === node);

  const columns: Column<MemberUpdates>[] = [
    {
      key: "node",
      header: "Node",
      value: (row) => row.node,
      mono: true,
      width: 180,
      render: (row) => (
        <span className="inline-flex items-center gap-2 min-w-0">
          <span className="qz-mono truncate">{row.node}</span>
          {row.local && <span className="badge badge-muted">this node</span>}
        </span>
      ),
    },
    {
      key: "status",
      header: "Status",
      value: (row) => statusOf(row, stepFor(row.node)).label,
      width: 170,
      render: (row) => {
        const status = statusOf(row, stepFor(row.node));
        return (
          <span
            className={`badge badge-${status.tone} inline-flex items-center gap-1`}
            title={status.title}
          >
            {status.spinning && <RefreshCw size={11} className="animate-spin" />}
            {status.pending && <CircleDashed size={11} />}
            {status.label}
          </span>
        );
      },
    },
    {
      key: "waiting",
      header: "Waiting",
      value: (row) => row.updates?.view.updates.length ?? -1,
      width: 110,
      render: (row) => {
        if (!row.updates) return <span className="qz-dim">—</span>;
        const count = row.updates.view.updates.length;
        return count === 0 ? (
          <span className="qz-dim">none</span>
        ) : (
          <span>
            {count} update{count === 1 ? "" : "s"}
          </span>
        );
      },
    },
    {
      key: "security",
      header: "Security",
      value: (row) => row.updates?.view.counts.security ?? 0,
      width: 110,
      render: (row) => {
        const count = row.updates?.view.counts.security ?? 0;
        return count === 0 ? (
          <span className="qz-dim">—</span>
        ) : (
          <span className="badge badge-crit inline-flex items-center gap-1">
            <ShieldAlert size={11} />
            {count}
          </span>
        );
      },
    },
    {
      key: "platform",
      header: "Kernel & storage",
      value: (row) => row.updates?.view.platform.updates.length ?? 0,
      width: 160,
      render: (row) => {
        const plan = row.updates?.view.platform;
        if (!plan || plan.updates.length === 0) return <span className="qz-dim">—</span>;
        return (
          <span
            className={`badge badge-${plan.resolves ? "warn" : "crit"}`}
            title={
              plan.resolves
                ? "Installed from that node's own page, and only with a restart afterwards."
                : (plan.detail ?? undefined)
            }
          >
            {plan.updates.length} {plan.resolves ? "waiting" : "blocked"}
          </span>
        );
      },
    },
    {
      key: "restart",
      header: "Restart",
      value: (row) => (row.updates?.view.reboot.required ? 1 : 0),
      width: 120,
      render: (row) => {
        if (!row.updates) return <span className="qz-dim">—</span>;
        return row.updates.view.reboot.required ? (
          <span
            className="badge badge-warn inline-flex items-center gap-1"
            title={row.updates.view.reboot.reason ?? undefined}
          >
            <RotateCw size={11} />
            needed
          </span>
        ) : (
          <span className="qz-dim">no</span>
        );
      },
    },
    {
      key: "checked",
      header: "Last checked",
      value: (row) => row.updates?.view.checked_at ?? 0,
      width: 150,
      render: (row) => {
        const at = row.updates?.view.checked_at;
        if (!at) return <span className="qz-dim">never</span>;
        return <span title={formatMoment(at)}>{formatAgo(at, nowSeconds)}</span>;
      },
    },
  ];

  return (
    <DataTable
      rows={members}
      columns={columns}
      rowId={(row) => row.node}
      searchPlaceholder="Search nodes…"
      emptyMessage="No members answered."
      onRefresh={onRefresh}
      storageKey="updates-members"
    />
  );
}

/// What one member's row says in its status cell.
///
/// The walk's step wins while there is one, because during a walk it is the
/// more current of the two facts — and because "restarting its control plane"
/// is the honest reading of a member that has stopped answering mid-walk,
/// which the row alone would render as a plain failure.
function statusOf(
  member: MemberUpdates,
  step: MemberStep | undefined,
): {
  label: string;
  tone: "ok" | "warn" | "crit" | "info" | "muted";
  title?: string;
  spinning?: boolean;
  pending?: boolean;
} {
  if (step?.state === "running") {
    // The stage is what it is *doing*; unreachable while installing is the
    // control plane restarting under it, which is the update working rather
    // than a member that has gone away.
    if (!member.reachable && step.stage === "installing") {
      return {
        label: "restarting",
        tone: "info",
        spinning: true,
        title:
          "It has stopped answering while it installs. Updating Lumen's own packages restarts " +
          "the control plane, so this is expected — the transaction outlives it.",
      };
    }
    return { label: STAGE_LABEL[step.stage], tone: "info", spinning: true };
  }
  if (step?.state === "failed") {
    return { label: "failed", tone: "crit", title: step.detail ?? undefined };
  }
  if (step?.state === "pending" && step.detail) {
    // A rolling update naming the node that is running it. Not a failure, and
    // not something the walk will get to on its own.
    return { label: "left for you", tone: "warn", title: step.detail };
  }
  if (step?.state === "pending") {
    return {
      label: "not yet attempted",
      tone: "muted",
      pending: true,
      title: "The update stopped before reaching this node.",
    };
  }
  if (step?.state === "done") {
    const changed = step.changed.length;
    return {
      label: changed > 0 ? `updated ${changed}` : "updated",
      tone: "ok",
      title: changed > 0 ? step.changed.join(", ") : (step.detail ?? undefined),
    };
  }

  if (!member.reachable) {
    return { label: "unreachable", tone: "crit", title: member.error ?? undefined };
  }
  const view = member.updates?.view;
  if (!view) return { label: "unknown", tone: "muted" };
  if (view.error) {
    return { label: "check failed", tone: "warn", title: view.error };
  }
  if (view.updates.length > 0) {
    return { label: "updates waiting", tone: "warn" };
  }
  if (!view.checked_at) {
    return { label: "never checked", tone: "muted", pending: true };
  }
  return { label: "up to date", tone: "ok" };
}

/// The confirmation in front of updating every node.
///
/// No checkbox: nothing here replaces a kernel and nothing restarts a machine,
/// so this is not the acknowledgement the platform dialog is. What it has to
/// do is say what the operator has not been told anywhere else — that the
/// nodes are done in a fixed order, that a failure stops the rest, and that
/// the console they are reading this on is the last one to go.
function ConfirmClusterDialog({
  order,
  localNode,
  waiting,
  working,
  onClose,
  onConfirm,
}: {
  order: string[];
  localNode: string;
  waiting: number;
  working: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Install ${waiting} update${waiting === 1 ? "" : "s"} across ${order.length} nodes?`}
        subtitle="One node at a time. Nothing restarts."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <div className="text-[13px] text-[var(--qz-fg-2)]">
          Each node installs its own updates in turn. A node with nothing waiting is stepped over,
          and the first node that fails stops the rest — an update that will not install on one
          member usually will not install on the next, and pressing on would turn one node that
          could not update into all of them.
        </div>

        <div>
          <div className="text-[12px] text-[var(--qz-fg-3)] mb-2">In this order:</div>
          <div className="text-[12px] qz-mono text-[var(--qz-fg-2)] break-words">
            {order.join(" → ")}
          </div>
        </div>

        {localNode && (
          <div className="callout callout-warn">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              Installing Lumen&apos;s own packages restarts a node&apos;s control plane, so each
              node&apos;s console goes away for a few seconds as its turn finishes.{" "}
              <strong>{localNode}</strong> is updated last, which is the one serving this page — so
              this console is the one that goes at the end. The updates finish either way; reload
              the page and it will show what every node ended up with.
            </div>
          </div>
        )}

        <div className="text-[13px] text-[var(--qz-fg-3)]">
          The kernel and the storage modules are not installed by this. They take effect only on a
          restart, and that is a decision per node — on that node&apos;s own page, through
          Maintenance.
        </div>

        <ModalFooter
          onCancel={onClose}
          saving={working}
          disabled={false}
          savingLabel="Starting…"
          submitLabel="Update All Nodes"
          onSubmit={onConfirm}
        />
      </div>
    </ModalShell>
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

/// The confirmation in front of a rolling update.
///
/// A checkbox, unlike the plain cluster update above it, and the same one the
/// backend requires. This is the dialog where something genuinely goes down:
/// machines are interrupted for as long as a live migration takes, and nodes
/// restart. What it must not do is pretend that is free.
function ConfirmRollingDialog({
  restarting,
  localNode,
  working,
  onClose,
  onConfirm,
}: {
  restarting: string[];
  localNode: string;
  working: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const [acknowledged, setAcknowledged] = useState(false);
  /// The node running this page is the one node a rolling update cannot
  /// restart, so it is called out before the operator starts rather than
  /// discovered at the end.
  const localIsIncluded = restarting.includes(localNode);

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Roll ${restarting.length} node${restarting.length === 1 ? "" : "s"} through a restart?`}
        subtitle="One at a time, machines moved off first."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <div className="text-[13px] text-[var(--qz-fg-2)]">
          Each node in turn is taken out of service, its running machines are migrated to members
          that hold current replicas of their disks, everything waiting is installed including the
          kernel, and the node is restarted. The next node is not touched until the previous one
          has come back, is running its new kernel, and is online in the cluster again.
        </div>

        <div>
          <div className="text-[12px] text-[var(--qz-fg-3)] mb-2">
            Nodes that will restart, in this order:
          </div>
          <div className="text-[12px] qz-mono text-[var(--qz-fg-2)] break-words">
            {restarting.join(" → ")}
          </div>
          <div className="text-[12px] text-[var(--qz-fg-3)] mt-2">
            Nodes with only ordinary updates waiting are not taken down for them — they are
            installed in place.
          </div>
        </div>

        <div className="callout callout-warn">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            A machine that cannot move — no replicated disk, media attached, no eligible target —
            stops the update rather than being restarted underneath. That node is put back into
            service and nothing after it is attempted.
          </div>
        </div>

        {localIsIncluded && (
          <div className="callout callout-info">
            <Wrench size={17} className="flex-shrink-0 text-[var(--qz-info)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              <strong>{localNode}</strong> is the node running this update, and it is the one node
              the update will not restart — a node cannot drive a rolling update through its own
              reboot. Every other node is done, and this one is named at the end. Finish it by
              running this same update from another member&apos;s console, where it will be the
              only work left, or take it through Maintenance by hand.
            </div>
          </div>
        )}

        <label className="flex items-start gap-2 text-[13px] text-[var(--qz-fg-2)] cursor-pointer">
          <input
            type="checkbox"
            checked={acknowledged}
            onChange={(e) => setAcknowledged(e.target.checked)}
            className="mt-[3px]"
          />
          <span>
            I understand each of these nodes restarts, one at a time, and that the machines running
            on them are interrupted for as long as a live migration takes.
          </span>
        </label>

        <ModalFooter
          onCancel={onClose}
          saving={working}
          disabled={!acknowledged}
          savingLabel="Starting…"
          submitLabel="Start Rolling Update"
          onSubmit={onConfirm}
        />
      </div>
    </ModalShell>
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
