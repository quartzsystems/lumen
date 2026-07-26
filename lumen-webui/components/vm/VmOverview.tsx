"use client";

import { AlertTriangle, Clock } from "lucide-react";
import { Fact, Facts, Meter, Mono, Panel, StateBadge, Tags } from "@/components/vm/VmBits";
import { LifecycleControls } from "@/components/vm/LifecycleControls";
import {
  cpuModelLabel,
  formatBytes,
  formatMib,
  formatUptime,
  type VmView,
} from "@/lib/vmClient";

/// What the machine is doing right now, and the controls that change it.
export function VmOverview({
  vm,
  busy,
  onAction,
}: {
  vm: VmView;
  busy: boolean;
  onAction: (message: string) => void;
}) {
  const boot = vm.disks.find((disk) => disk.id === vm.boot_disk) ?? null;
  const memoryPercent =
    vm.current_memory_mib !== null && vm.memory_mib > 0
      ? (vm.current_memory_mib / vm.memory_mib) * 100
      : 0;

  return (
    <div className="flex flex-col gap-4">
      {vm.pending_reboot.length > 0 && (
        <div className="callout callout-warn">
          <Clock size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="flex-1 min-w-0">
            <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
              Waiting for a restart
            </div>
            <ul className="m-0 mt-1 pl-4 text-[13px] text-[var(--qz-fg-3)]">
              {vm.pending_reboot.map((item) => (
                <li key={item}>{item}</li>
              ))}
            </ul>
          </div>
        </div>
      )}

      <Panel title="Status">
        <div className="flex flex-col gap-4">
          <div className="flex items-center gap-3 flex-wrap">
            <StateBadge state={vm.state} />
            {vm.uptime_secs !== null && (
              <span className="text-[13px] text-[var(--qz-fg-3)]">
                up {formatUptime(vm.uptime_secs)}
              </span>
            )}
          </div>
          <LifecycleControls vm={vm} busy={busy} onDone={onAction} />
        </div>
      </Panel>

      <Panel title="Resources">
        <Facts>
          <Fact label="Processors">
            {vm.current_vcpus !== null && vm.current_vcpus !== vm.vcpus ? (
              <>
                {vm.current_vcpus} now, {vm.vcpus} after a restart
              </>
            ) : (
              vm.vcpus
            )}
          </Fact>
          <Fact label="Memory">
            <div className="flex items-center gap-3">
              <span>
                {vm.current_memory_mib !== null
                  ? `${formatMib(vm.current_memory_mib)} of ${formatMib(vm.memory_mib)}`
                  : formatMib(vm.memory_mib)}
              </span>
              {vm.current_memory_mib !== null && (
                <span className="w-[120px]">
                  <Meter percent={memoryPercent} />
                </span>
              )}
            </div>
          </Fact>
          <Fact label="Processor model">{cpuModelLabel(vm.cpu_model)}</Fact>
          <Fact label="Firmware">{vm.firmware === "uefi" ? "UEFI" : "Legacy BIOS"}</Fact>
          <Fact label="Machine type">
            <Mono>{vm.machine}</Mono>
          </Fact>
          <Fact label="Node">
            <Mono>{vm.node}</Mono>
          </Fact>
        </Facts>
      </Panel>

      <Panel title="Boot disk">
        {boot ? (
          <Facts>
            <Fact label="Target">
              <Mono>{boot.id}</Mono>
            </Fact>
            <Fact label="Volume">
              <Mono>{boot.source}</Mono>
            </Fact>
            <Fact label="Size">{formatBytes(boot.size)}</Fact>
            <Fact label="Bus">
              <Mono>{boot.bus}</Mono>
            </Fact>
          </Facts>
        ) : (
          <div className="callout callout-warn">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              This machine has no disk, so it has nothing to boot. Add one under Hardware.
            </div>
          </div>
        )}
      </Panel>

      <Panel title="Network">
        {vm.nics.length === 0 ? (
          <p className="text-[13px] text-[var(--qz-fg-4)] m-0">No adapters.</p>
        ) : (
          <Facts>
            {vm.nics.map((nic) => (
              <Fact key={nic.id} label={nic.bridge}>
                <Mono>{nic.id}</Mono>
                <span className="qz-dim"> · {nic.model}</span>
                {nic.vlan_tag ? <span className="qz-dim"> · VLAN {nic.vlan_tag}</span> : null}
              </Fact>
            ))}
          </Facts>
        )}
      </Panel>

      <Panel title="Guest">
        <Facts>
          <Fact label="Guest agent">
            {vm.guest_agent ? (
              // The channel is defined; whether anything is answering on it is
              // a question only the agent itself can settle, and this stage
              // does not ask it.
              <span className="badge badge-info">channel defined</span>
            ) : (
              <span className="badge badge-muted">off</span>
            )}
          </Fact>
          <Fact label="Start on boot">{vm.start_on_boot ? "yes" : "no"}</Fact>
          <Fact label="Tags">
            <Tags tags={vm.tags} />
          </Fact>
          {vm.description && <Fact label="Description">{vm.description}</Fact>}
        </Facts>
      </Panel>
    </div>
  );
}
