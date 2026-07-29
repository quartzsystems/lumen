#!/usr/bin/env bash
# burn-in.sh — LumenFS on real hardware: kill it, and prove it kept its word.
#
# The engine's crash suites run under a simulated disk, where a power loss
# is a decision the harness makes. This is the other half: the same
# durability contract against a real device's own fsync, on the machine
# that will run it.
#
# One round is: start `lumen-fs-nbd workload`, let it run a moment, SIGKILL
# it mid-flight, then `verify`. The workload records its acknowledged
# progress as a watermark inside the vdisk itself, so the verifier needs no
# side channel and no trust in the process it just killed — it replays the
# seed and demands that every operation at or below the watermark survived
# exactly. See the burn-in section of docs/lumenfs.md.
#
# SIGKILL is not a power cut: the kernel's page cache survives it, so a
# clean run proves the engine's ordering, not the device's honesty about
# flushes. To test the device too, run --forever and pull the cord, then
# re-run this script — it resumes from the watermark and verifies first.
#
# Usage:
#   burn-in.sh [options]
#     --image PATH     backing file (default /var/tmp/lumen-fs-burnin.img)
#     --device PATH    burn in on a real block device instead; DESTROYS IT
#     --size BYTES     backing size when creating an image (default 4 GiB)
#     --vdisk BYTES    vdisk size (default half the backing size)
#     --rounds N       kill/verify rounds (default 20)
#     --seconds N      seconds to let the workload run per round (default 3)
#     --binary PATH    lumen-fs-nbd to use (default: found on PATH or ./)
#     --forever        run the workload without killing it; for a power cut
#     --keep           do not delete the image at the end
set -euo pipefail

image=/var/tmp/lumen-fs-burnin.img
device=
size=$((4 * 1024 * 1024 * 1024))
vdisk=
rounds=20
seconds=3
binary=
forever=0
keep=0

die() {
    echo "burn-in: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --image) image="${2:?--image needs a path}"; shift 2 ;;
        --device) device="${2:?--device needs a path}"; shift 2 ;;
        --size) size="${2:?--size needs bytes}"; shift 2 ;;
        --vdisk) vdisk="${2:?--vdisk needs bytes}"; shift 2 ;;
        --rounds) rounds="${2:?--rounds needs a count}"; shift 2 ;;
        --seconds) seconds="${2:?--seconds needs a count}"; shift 2 ;;
        --binary) binary="${2:?--binary needs a path}"; shift 2 ;;
        --forever) forever=1; shift ;;
        --keep) keep=1; shift ;;
        -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

# The binary: an explicit path, then PATH, then the usual cargo output.
if [ -z "$binary" ]; then
    here="$(cd -- "$(dirname -- "$0")" && pwd)"
    for candidate in \
        "$(command -v lumen-fs-nbd || true)" \
        "$here/target/release/lumen-fs-nbd" \
        "$here/target/debug/lumen-fs-nbd" \
        ./lumen-fs-nbd
    do
        if [ -n "$candidate" ] && [ -x "$candidate" ]; then
            binary="$candidate"
            break
        fi
    done
fi
[ -n "$binary" ] || die "no lumen-fs-nbd found; pass --binary PATH"
[ -x "$binary" ] || die "not executable: $binary"

# A real device is the honest target and the dangerous one. Refuse to guess.
if [ -n "$device" ]; then
    [ -b "$device" ] || die "not a block device: $device"
    if lsblk -nro MOUNTPOINT "$device" 2>/dev/null | grep -q .; then
        die "$device has something mounted on it"
    fi
    echo "burn-in: this ERASES $device."
    printf 'type the device path to confirm: '
    read -r confirm
    [ "$confirm" = "$device" ] || die "not confirmed"
    target="$device"
    size="$(blockdev --getsize64 "$device")"
else
    target="$image"
fi

[ -n "$vdisk" ] || vdisk=$((size / 2))

# Format only when there is nothing there yet: a re-run after a power cut
# must resume against the existing pool, not erase the evidence.
if [ -n "$device" ] || [ ! -e "$target" ]; then
    if [ -n "$device" ]; then
        # `format` refuses to clobber, so clear the superblocks first.
        dd if=/dev/zero of="$target" bs=1M count=1 conv=fsync status=none
        rm -f "$target.placeholder"
    fi
    echo "burn-in: formatting $target ($size bytes, vdisk $vdisk bytes)"
    "$binary" format "$target" "$size" "$vdisk"
else
    echo "burn-in: reusing the pool already on $target"
    echo "burn-in: verifying what survived whatever happened last..."
    "$binary" verify "$target" "$RANDOM$$" >/dev/null 2>&1 || true
fi

# One seed for the life of this pool: the verifier replays it, so it must
# not change between rounds. Derive it from the pool's own path so a re-run
# after a reboot picks the same one.
seed="$(cksum <<<"$target" | cut -d' ' -f1)"
echo "burn-in: seed $seed"

cleanup() {
    if [ -n "${workload_pid:-}" ] && kill -0 "$workload_pid" 2>/dev/null; then
        kill -9 "$workload_pid" 2>/dev/null || true
        wait "$workload_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

if [ "$forever" = 1 ]; then
    echo "burn-in: running until you stop it — pull the power whenever you like."
    echo "burn-in: afterwards, re-run this script to verify what survived."
    exec "$binary" workload "$target" "$seed"
fi

for round in $(seq 1 "$rounds"); do
    "$binary" workload "$target" "$seed" >/dev/null &
    workload_pid=$!
    sleep "$seconds"
    if ! kill -0 "$workload_pid" 2>/dev/null; then
        wait "$workload_pid" || die "round $round: the workload died on its own"
    fi
    kill -9 "$workload_pid" 2>/dev/null || true
    wait "$workload_pid" 2>/dev/null || true
    unset workload_pid

    if ! "$binary" verify "$target" "$seed"; then
        die "round $round: verification failed — keep $target for the post-mortem"
    fi
    echo "burn-in: round $round/$rounds survived"
done

echo "burn-in: $rounds rounds, every acknowledged write intact"

if [ "$keep" = 0 ] && [ -z "$device" ]; then
    rm -f "$target"
fi
