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
#     --verify-only    check what survived and stop; the post-power-cut step
#     --fresh          reformat a --device that already holds a pool
#     --keep           do not delete the image at the end
#
# After a power cut, run --verify-only against the same target: an existing
# pool is always verified before anything else touches it, and that check is
# the experiment.
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
fresh=0
verify_only=0

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
        --fresh) fresh=1; shift ;;
        --verify-only) verify_only=1; shift ;;
        -h|--help) sed -n '2,36p' "$0"; exit 0 ;;
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

if [ -n "$device" ]; then
    [ -b "$device" ] || die "not a block device: $device"
    if lsblk -nro MOUNTPOINT "$device" 2>/dev/null | grep -q .; then
        die "$device has something mounted on it"
    fi
    target="$device"
    size="$(blockdev --getsize64 "$device")"
else
    target="$image"
fi

[ -n "$vdisk" ] || vdisk=$((size / 2))

# One seed for the life of this pool: the verifier replays it, so it must
# not change between rounds — and must be the same one a re-run after a
# reboot picks up. Derived from the pool's own path, so it is.
seed="$(cksum <<<"$target" | cut -d' ' -f1)"
echo "burn-in: seed $seed"

# The high-water mark this harness has been shown, kept outside the pool.
# A pool that has lost some of what it owed cannot be trusted to say how
# much that was — its own watermark shrinks with the loss, forgiving the
# very debt under examination. So the creditor keeps the books.
progress="$target.progress"

run_verify() {
    local min out seen
    min="$(cat "$progress" 2>/dev/null || echo 0)"
    if ! out="$("$binary" verify "$target" "$seed" "$min" 2>&1)"; then
        printf '%s\n' "$out" >&2
        return 1
    fi
    printf '%s\n' "$out"
    seen="$(printf '%s\n' "$out" | sed -n 's/.*watermark=\([0-9][0-9]*\).*/\1/p' | head -1)"
    if [ -n "$seen" ] && [ "$seen" -gt "$min" ]; then
        printf '%s\n' "$seen" >"$progress"
        sync
    fi
    return 0
}

# Whether a pool is already here decides everything that follows, and the
# question is "is there a pool", not "is there a path": a block device
# always exists, so asking about the path would treat a blank disk as an
# established one and fail to find the superblock it never had.
if [ -e "$target" ] && "$binary" info "$target" >/dev/null 2>&1; then
    has_pool=1
else
    has_pool=0
fi

# Formatting over something is the one destructive act here, so it is the
# only one that asks.
if [ "$has_pool" = 1 ] && [ "$fresh" = 1 ]; then
    echo "burn-in: --fresh will DESTROY the pool already on $target."
    printf 'type the target path to confirm: '
    read -r confirm
    [ "$confirm" = "$target" ] || die "not confirmed"
fi

# A fresh pool gets formatted, and the books start empty with it. An
# existing one is resumed and, first of all, verified: after a power cut
# that check *is* the experiment, and it is measured against what this
# harness was promised, not against what the pool now claims.
if [ "$has_pool" = 0 ] || [ "$fresh" = 1 ]; then
    echo "burn-in: formatting $target ($size bytes, vdisk $vdisk bytes)"
    rm -f "$progress"
    "$binary" format "$target" "$size" "$vdisk"
else
    echo "burn-in: resuming the pool already on $target"
    echo "burn-in: verifying what survived whatever happened last..."
    run_verify || die "the pool did not survive intact — keep $target for the post-mortem"
    echo "burn-in: everything acknowledged before the interruption is intact"
fi

if [ "$verify_only" = 1 ]; then
    exit 0
fi

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

# One round: run the workload for `$1` seconds of actual work, then kill it.
run_round() {
    local budget="$1" started
    started="$(mktemp)"
    "$binary" workload "$target" "$seed" >"$started" &
    workload_pid=$!
    # Opening a pool means scanning what is stored in it, and that grows
    # with the data. Timing the round from process start would measure
    # recovery rather than writing, and — since a batch only counts when it
    # finishes — a round short enough to be mostly open time looks exactly
    # like a wedge. Wait until it says it is working, then start counting.
    for _ in $(seq 1 3000); do
        grep -q '^workload:' "$started" 2>/dev/null && break
        kill -0 "$workload_pid" 2>/dev/null || break
        sleep 0.1
    done
    sleep "$budget"
    if ! kill -0 "$workload_pid" 2>/dev/null; then
        wait "$workload_pid" || die "the workload died on its own"
    fi
    kill -9 "$workload_pid" 2>/dev/null || true
    wait "$workload_pid" 2>/dev/null || true
    unset workload_pid
    rm -f "$started"
}

for round in $(seq 1 "$rounds"); do
    before="$(cat "$progress" 2>/dev/null || echo 0)"
    run_round "$seconds"
    run_verify || die "round $round: verification failed — keep $target for the post-mortem"
    after="$(cat "$progress" 2>/dev/null || echo 0)"

    # A round that acknowledged nothing has proved nothing, and left
    # unchecked it reads as a pass forever — the watermark stops moving and
    # every later verify agrees with the last one. But "nothing in three
    # seconds" is not the same as "stuck": a pool that has been worked hard
    # takes longer to open and longer to collect, and a batch only counts
    # once it finishes. So ask again with room to breathe before calling it.
    if [ "$after" = "$before" ]; then
        echo "burn-in: round $round acknowledged nothing in ${seconds}s — retrying with $((seconds * 8))s"
        run_round "$((seconds * 8))"
        run_verify || die "round $round: verification failed — keep $target for the post-mortem"
        after="$(cat "$progress" 2>/dev/null || echo 0)"
        if [ "$after" = "$before" ]; then
            die "round $round: nothing acknowledged in $((seconds * 9))s — genuinely stuck at $after.
Ask the pool what it thinks:  $binary info $target  and  $binary gc $target"
        fi
        echo "burn-in: round $round/$rounds survived, slowly (watermark $after)"
        continue
    fi
    echo "burn-in: round $round/$rounds survived (watermark $after)"
done

echo "burn-in: $rounds rounds, every acknowledged write intact"

if [ "$keep" = 0 ] && [ -z "$device" ]; then
    rm -f "$target" "$progress"
fi
