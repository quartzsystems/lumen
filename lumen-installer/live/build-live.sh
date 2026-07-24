#!/usr/bin/env bash
# Build the Lumen live installer environment: dnf --installroot rootfs ->
# dracut (dmsquash-live) initramfs -> flattened squashfs.
#
# Runs as root in an UNPRIVILEGED almalinux:10 container: no loop devices,
# no mounts. dracut's dmsquash-live module only copies files at build time
# (device-mapper work happens at boot), the chroot gets a handful of
# mknod'd /dev nodes (CAP_MKNOD is in the default container cap set), and
# mksquashfs never mounts anything.
#
# Invoked by iso/build-live-iso.sh; see --help for the interface.
set -euo pipefail

die() { echo "FATAL: $*" >&2; exit 1; }

usage() {
    cat <<'EOF'
Usage: build-live.sh --workdir DIR --installer-binary PATH --lumen-repo DIR
                     --kernel-nevr NEVR --version V --out-tree DIR

The lumen repo must already contain the signature-verified OpenZFS subset
(zfs, kmod-zfs, libraries) — this script never talks to the ZFS mirror.

Produces in --out-tree:  images/vmlinuz, images/initrd.img,
                         LiveOS/squashfs.img
Leaves the live rootfs at <workdir>/rootfs (ESP binaries + kABI checks are
consumed from it by the caller).
EOF
}

WORKDIR='' INSTALLER_BINARY='' LUMEN_REPO='' KERNEL_NEVR='' VERSION='' OUT_TREE=''
while [ $# -gt 0 ]; do
    case "$1" in
        --workdir)          WORKDIR="${2:?}"; shift 2 ;;
        --installer-binary) INSTALLER_BINARY="${2:?}"; shift 2 ;;
        --lumen-repo)       LUMEN_REPO="${2:?}"; shift 2 ;;
        --kernel-nevr)      KERNEL_NEVR="${2:?}"; shift 2 ;;
        --version)          VERSION="${2:?}"; shift 2 ;;
        --out-tree)         OUT_TREE="${2:?}"; shift 2 ;;
        -h|--help)          usage; exit 0 ;;
        *) usage >&2; die "unknown argument: $1" ;;
    esac
done
for v in WORKDIR INSTALLER_BINARY LUMEN_REPO KERNEL_NEVR VERSION OUT_TREE; do
    [ -n "${!v}" ] || { usage >&2; die "missing --$(tr '[:upper:]_' '[:lower:]-' <<<"$v")"; }
done
[ -x "$INSTALLER_BINARY" ] || die "installer binary not executable: $INSTALLER_BINARY"
[ -d "$LUMEN_REPO/repodata" ] || die "not a repo (no repodata): $LUMEN_REPO"

LIVE_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$WORKDIR/rootfs"
KVER="${KERNEL_NEVR#kernel-}.x86_64"

rm -rf "$ROOT"
mkdir -p "$ROOT/dev" "$ROOT/etc/dracut.conf.d" "$OUT_TREE/images" "$OUT_TREE/LiveOS"

# Basic device nodes for chrooted tooling (no devtmpfs mount available).
for node in "null c 1 3" "zero c 1 5" "random c 1 8" "urandom c 1 9" "tty c 5 0"; do
    # shellcheck disable=SC2086 # intentional word splitting of the spec
    set -- $node
    [ -e "$ROOT/dev/$1" ] || mknod -m 666 "$ROOT/dev/$1" "$2" "$3" "$4"
done

# Neuter the kernel RPM's own initramfs generation (hostonly of a container
# is useless); we build the live initramfs explicitly below.
echo 'hostonly="no"' > "$ROOT/etc/dracut.conf.d/90-lumen-build.conf"

echo "==> Live rootfs: base repos (almalinux + epel)"
dnf -y --installroot="$ROOT" --releasever=10 --setopt=install_weak_deps=False \
    install almalinux-release epel-release

echo "==> Live rootfs: full package set (kernel $KERNEL_NEVR)"
mapfile -t packages < <(sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$LIVE_DIR/packages.txt")
dnf -y --installroot="$ROOT" --releasever=10 --setopt=install_weak_deps=False \
    --repofrompath="lumen,file://$LUMEN_REPO" --setopt=lumen.gpgcheck=0 \
    install "$KERNEL_NEVR" "kernel-modules-${KERNEL_NEVR#kernel-}" "${packages[@]}"

echo "==> kABI gate: kmod-zfs must resolve against $KVER"
# Both commands run INSIDE the chroot: kABI kmods live under the kernel
# version they were built against, and weak-modules links them into our
# kernel's weak-updates/ with ABSOLUTE symlinks — resolvable only from
# within the root.
chroot "$ROOT" depmod -a "$KVER"
chroot "$ROOT" /sbin/modprobe --dry-run -S "$KVER" zfs \
    || die "kmod-zfs does not resolve against $KVER (kABI mismatch — check iso/pins.env)"

echo "==> Installing lumen-installer + overlay"
install -m 0755 "$INSTALLER_BINARY" "$ROOT/usr/bin/lumen-installer"
cp -r "$LIVE_DIR/overlay/." "$ROOT/"
chmod 0755 "$ROOT/root/.local/bin/gnome-kiosk-script"
cat > "$ROOT/etc/lumen-build.env" <<EOF
# Stamped by lumen-installer/live/build-live.sh — read by the installer.
KERNEL_NEVR="$KERNEL_NEVR"
LUMEN_VERSION="$VERSION"
EOF
echo "lumen-live" > "$ROOT/etc/hostname"
: > "$ROOT/etc/machine-id"

systemctl --root="$ROOT" enable \
    lumen-kiosk.service lumen-nicnames.service NetworkManager chronyd
systemctl --root="$ROOT" set-default graphical.target

echo "==> Building live initramfs (dracut dmsquash-live, no-hostonly)"
chroot "$ROOT" dracut --force --no-hostonly --no-hostonly-cmdline \
    --no-hostonly-default-device \
    --add "dmsquash-live pollcdrom" \
    --add-drivers "squashfs overlay iso9660 vfat sr_mod cdrom loop dm_mod" \
    --omit "zfs" \
    /boot/initramfs-live.img "$KVER"

cp "$ROOT/boot/vmlinuz-$KVER" "$OUT_TREE/images/vmlinuz"
cp "$ROOT/boot/initramfs-live.img" "$OUT_TREE/images/initrd.img"

echo "==> Squashing live rootfs"
dnf -y --installroot="$ROOT" clean all >/dev/null
rm -rf "$ROOT/var/cache/dnf"
# Kernels boot from the ISO, not from inside the squashfs.
rm -f "$ROOT"/boot/initramfs-*.img "$ROOT"/boot/vmlinuz-* 2>/dev/null || true
mksquashfs "$ROOT" "$OUT_TREE/LiveOS/squashfs.img" \
    -comp zstd -Xcompression-level 19 -b 1M -noappend -xattrs -quiet

echo "==> Live environment done: $OUT_TREE/LiveOS/squashfs.img"
