#!/usr/bin/env bash
# pool-bench.sh — what the pool actually delivers, measured on the node.
#
# burn-in.sh proves the engine keeps its word; this proves the word arrives
# at a usable speed. It mints its own scratch vdisk through the daemon's
# control socket, exports it, runs a fio battery against the real ublk
# device — the same path every machine disk takes, replication included —
# and deletes what it made. It never touches a machine's disk: scratch ids
# live below 256, where no vm-<id>-disk-<n> can reach, and the one
# destructive act is against a vdisk this script created.
#
# The battery, in order:
#   fill       sequential write over the whole vdisk — first-allocation
#              bandwidth, and the precondition for every read test: an
#              unwritten vdisk answers zeros without asking the disk, and
#              reads from it would measure nothing.
#   seqread    1M sequential read       — image pulls, backups, boot storms
#   seqwrite   1M sequential overwrite  — steady-state, past first allocation
#   randread   4k random read           — the general VM workload
#   randwrite  4k random write          — replication under small-block load
#   syncwrite  4k, queue depth 1, fsync after every write — the durability
#              path: what a guest's database or journal actually waits on.
#              The daemon's own `durable` stream counter must advance during
#              this test, or the numbers are declared lies — the elided-flush
#              failure mode, where a write-through device answers fsync
#              without engaging the engine, reads as impossibly fast and is
#              caught by exactly this counter.
#   mixed      4k random, 70/30 read/write — a day at the office
#
# Buffers are refilled with fresh random data (`refill_buffers`,
# `randrepeat=0`) so dedupe cannot flatter the write numbers: a benchmark
# writing the same block a million times measures the hash table, not the
# disks.
#
# Run it on one node at a time: every write replicates, so the peer is
# doing work too, and two batteries at once measure each other.
#
# Usage:
#   pool-bench.sh [options]
#     --size BYTES     scratch vdisk size (default 4 GiB)
#     --vdisk N        scratch vdisk id, must be 2..255 (default 250)
#     --seconds N      per-test runtime (default 30)
#     --control ADDR   daemon control address (default: LUMEN_FSD_CONTROL
#                      from /etc/lumen/fsd.conf, else 127.0.0.1:7799)
#     --baseline PATH  run the same battery against PATH afterwards, for a
#                      side-by-side — a spare raw device or a file on local
#                      storage. DESTROYS a block device's contents; a file
#                      path is created and removed.
#     --fresh          delete a leftover scratch vdisk from an earlier run
#     --keep           leave the scratch vdisk exported at the end
set -euo pipefail

size=$((4 * 1024 * 1024 * 1024))
vdisk=250
seconds=30
control=
baseline=
fresh=0
keep=0

die() {
    echo "pool-bench: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --size) size="${2:?--size needs bytes}"; shift 2 ;;
        --vdisk) vdisk="${2:?--vdisk needs an id}"; shift 2 ;;
        --seconds) seconds="${2:?--seconds needs a count}"; shift 2 ;;
        --control) control="${2:?--control needs host:port}"; shift 2 ;;
        --baseline) baseline="${2:?--baseline needs a path}"; shift 2 ;;
        --fresh) fresh=1; shift ;;
        --keep) keep=1; shift ;;
        -h|--help) sed -n '2,56p' "$0"; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

[ "$vdisk" -ge 2 ] && [ "$vdisk" -le 255 ] ||
    die "--vdisk must be 2..255: below the machine-disk ids, and not the bootstrap vdisk"

command -v fio >/dev/null || die "fio is not installed — dnf install fio"
command -v python3 >/dev/null || die "python3 is needed to read fio's json"

# The control address: given, or the drop-in's, or the unit's constant.
if [ -z "$control" ]; then
    control="$(sed -n 's/^LUMEN_FSD_CONTROL=//p' /etc/lumen/fsd.conf 2>/dev/null | tail -1)"
    control="${control:-127.0.0.1:7799}"
fi
control_host="${control%:*}"
control_port="${control##*:}"

# One line out, one line back. The daemon answers `ok`/`ok: detail` or
# `error: why`, and a refusal is surfaced as its own sentence.
control_cmd() {
    local reply
    exec 3<>"/dev/tcp/$control_host/$control_port" ||
        die "cannot reach the daemon's control socket at $control"
    printf '%s\n' "$*" >&3
    IFS= read -r reply <&3 || die "the daemon closed the connection mid-answer"
    exec 3<&- 3>&-
    case "$reply" in
        ok) ;;
        "ok: "*) printf '%s\n' "${reply#ok: }" ;;
        *) die "'$*' was refused: $reply" ;;
    esac
}

status="$(control_cmd status)"
echo "pool-bench: daemon at $control answers: $status"
state="$(sed -n 's/.*state=\([a-z]*\).*/\1/p' <<<"$status")"
if [ "$state" != "synced" ]; then
    echo "pool-bench: NOTE — replication is '$state', not synced. These numbers will" >&2
    echo "pool-bench: describe the pool as it is right now, not the pool at its best." >&2
fi

# A leftover from an interrupted run is deleted only when asked: ids below
# 256 are this harness's by convention, but somebody may have minted one by
# hand, and a benchmark must not be the thing that deletes it unprompted.
if control_cmd vdisks | tr ' ' '\n' | grep -q "^$vdisk="; then
    if [ "$fresh" = 1 ]; then
        echo "pool-bench: deleting leftover scratch vdisk $vdisk"
        control_cmd unexport "$vdisk" >/dev/null 2>&1 || true
        control_cmd vdisk-delete "$vdisk" >/dev/null
    else
        die "vdisk $vdisk already exists — a leftover from an interrupted run? \
--fresh deletes and recreates it"
    fi
fi

outdir="$(mktemp -d /var/tmp/pool-bench-XXXXXX)"
echo "pool-bench: raw fio output in $outdir"

created=0
exported=0
cleanup() {
    if [ "$keep" = 1 ]; then
        echo "pool-bench: keeping vdisk $vdisk exported (--keep); clean up with:"
        echo "pool-bench:   unexport $vdisk / vdisk-delete $vdisk on the control socket"
        return
    fi
    # Subshells: control_cmd exits on a dead socket, and cleanup must try
    # the delete even when the unexport could not be asked.
    [ "$exported" = 1 ] && (control_cmd unexport "$vdisk") >/dev/null 2>&1 || true
    [ "$created" = 1 ] && (control_cmd vdisk-delete "$vdisk") >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

control_cmd vdisk-create "$vdisk" "$size" 0 >/dev/null
created=1
device="$(control_cmd export "$vdisk" "$vdisk")"
exported=1
for _ in $(seq 1 100); do
    [ -b "$device" ] && break
    sleep 0.1
done
[ -b "$device" ] || die "exported, but $device never appeared"
echo "pool-bench: scratch vdisk $vdisk is $device ($size bytes)"

# --- the battery --------------------------------------------------------------

summary=()

# One fio run: fixed honesty flags, per-test shape on top, one summary line
# parsed out of the json. Buffers are random and never repeat, so dedupe
# measures the disks rather than the hash table.
run_fio() {
    local name="$1" target="$2"; shift 2
    local json="$outdir/$name.json"
    fio --name="$name" --filename="$target" --direct=1 --ioengine=libaio \
        --refill_buffers --randrepeat=0 --group_reporting \
        --output-format=json --output="$json" "$@" >/dev/null ||
        die "fio $name failed — its output is in $outdir"
    python3 - "$json" "$name" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
job = doc["jobs"][0]
def fmt_bw(bytes_s):
    return f"{bytes_s / 1e6:8.1f} MB/s"
def fmt_lat(ns):
    return f"{ns / 1e6:7.2f} ms" if ns >= 1e6 else f"{ns / 1e3:7.0f} us"
parts = []
for side in ("read", "write"):
    s = job[side]
    if s["total_ios"] == 0:
        continue
    lat = s["clat_ns"]
    p99 = lat.get("percentile", {}).get("99.000000", 0)
    parts.append(
        f"{side}: {fmt_bw(s['bw_bytes'])} {s['iops']:9.0f} iops"
        f"  lat avg {fmt_lat(lat['mean'])} p99 {fmt_lat(p99)}"
    )
# fsync latency is its own distribution: the flush is what a guest's
# journal waits on, and averaging it into the writes would hide it.
sync = job.get("sync", {})
if sync.get("total_ios", 0) > 0:
    lat = sync["lat_ns"]
    p99 = lat.get("percentile", {}).get("99.000000", 0)
    parts.append(f"fsync: avg {fmt_lat(lat['mean'])} p99 {fmt_lat(p99)}")
print(f"{sys.argv[2]:>9}  " + "   ".join(parts))
PY
}

durable_now() {
    control_cmd status | tr ' ' '\n' | sed -n 's/^durable=//p' | head -1
}

battery() {
    local target="$1" label="$2"
    echo
    echo "pool-bench: === $label ==="

    # The fill: not time-based — the whole device, so later reads hit real
    # data. Its bandwidth is the first-allocation number, reported as such.
    summary+=("$(run_fio fill-"$label" "$target" --rw=write --bs=1M --iodepth=16)")
    echo "${summary[-1]}"

    for spec in \
        "seqread   --rw=read      --bs=1M --iodepth=16" \
        "seqwrite  --rw=write     --bs=1M --iodepth=16" \
        "randread  --rw=randread  --bs=4k --iodepth=32 --numjobs=4" \
        "randwrite --rw=randwrite --bs=4k --iodepth=32 --numjobs=4" \
        "mixed     --rw=randrw    --rwmixread=70 --bs=4k --iodepth=16 --numjobs=2"
    do
        # shellcheck disable=SC2086 — the spec is a flag list on purpose
        set -- $spec
        local name="$1"; shift
        summary+=("$(run_fio "$name-$label" "$target" --time_based --runtime="$seconds" "$@")")
        echo "${summary[-1]}"
    done

    # The durability path, bracketed by the stream counter that convicted
    # the elided-flush bug: if fsync is not reaching the engine, `durable`
    # stands still while the latency reads impossibly fast.
    local before after
    [ "$label" = pool ] && before="$(durable_now)"
    summary+=("$(run_fio syncwrite-"$label" "$target" \
        --time_based --runtime="$seconds" --rw=randwrite --bs=4k --iodepth=1 --fsync=1)")
    echo "${summary[-1]}"
    if [ "$label" = pool ]; then
        after="$(durable_now)"
        if [ -n "$before" ] && [ -n "$after" ] && [ "$after" -le "$before" ]; then
            echo "pool-bench: WARNING — the daemon's durable counter did not move during" >&2
            echo "pool-bench: the fsync test ($before -> $after). The device is answering" >&2
            echo "pool-bench: flushes without engaging the engine; every sync number above" >&2
            echo "pool-bench: is a lie. This is the elided-flush failure mode." >&2
        else
            echo "pool-bench: fsync engaged the engine (durable $before -> $after)"
        fi
    fi
}

battery "$device" pool

if [ -n "$baseline" ]; then
    if [ -b "$baseline" ]; then
        lsblk -nro MOUNTPOINT "$baseline" 2>/dev/null | grep -q . &&
            die "$baseline has something mounted on it"
        echo
        echo "pool-bench: baseline DESTROYS the contents of $baseline."
        printf 'type the path to confirm: '
        read -r confirm
        [ "$confirm" = "$baseline" ] || die "not confirmed"
        battery "$baseline" baseline
    else
        # A file baseline sizes itself like the scratch vdisk and is removed.
        truncate -s "$size" "$baseline" || die "cannot create $baseline"
        battery "$baseline" baseline
        rm -f "$baseline"
    fi
fi

echo
echo "pool-bench: ================= summary ================="
printf '%s\n' "${summary[@]}"
echo
echo "pool-bench: how to read this:"
echo "  - syncwrite is what a guest's database commits wait on. Its average"
echo "    rides the network round trip to the peer plus both members' flushes;"
echo "    compare it against 'ping <peer>' — a large multiple of the RTT means"
echo "    the time is going somewhere other than the wire."
echo "  - randread/randwrite iops bound how many busy guests the pool carries."
echo "  - fill vs seqwrite: first allocation vs steady-state overwrite."
echo "  - replication was '$state' at the start; a degraded pool writes faster"
echo "    (nothing to wait for) and that speed is not the number to plan on."
echo "pool-bench: raw fio json kept in $outdir"
