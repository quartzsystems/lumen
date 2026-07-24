#!/usr/bin/env bash
# Build the Lumen appliance ISO: a custom live image that boots the Rust
# GUI installer (lumen-installer) under gnome-kiosk — no Anaconda.
#
#   1. verify the upstream AlmaLinux 10 minimal ISO (source of the on-media
#      Minimal repo and the kernel pin)
#   2. build the Lumen RPMs; mirror the pinned OpenZFS EL10 subset; create
#      the local 'lumen' repo
#   3. cargo-build lumen-installer (gtk4-rs, AppStream toolchain)
#   4. build the live rootfs -> dracut dmsquash-live initramfs -> squashfs
#      (lumen-installer/live/build-live.sh)
#   5. assemble a UEFI-only ISO: fresh ESP FAT image via mkfs.vfat+mtools
#      (no loop mounts), xorrisofs with appended GPT ESP, implantisomd5
#
# Fully unprivileged apart from running as root in a container (chroot +
# mknod only — no loop devices, no mounts). Hard gates: kernel pin vs
# media, kmod-zfs kABI dry-run, offline dependency resolution.
#
# Output: dist/lumen-<version>-x86_64.iso (+ .sha256)
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: build-live-iso.sh --upstream-iso PATH [--sha256 HASH] [--output DIR]

  --upstream-iso PATH  Upstream AlmaLinux 10 x86_64 minimal ISO (required)
  --sha256 HASH        Expected SHA-256 of the upstream ISO. If omitted, the
                       script reads PATH.sha256 (first word = hash).
  --output DIR         Output directory (default: <repo>/dist)

Also configurable via UPSTREAM_ISO / UPSTREAM_SHA256 / OUTPUT_DIR env vars.
Version pins (kernel NEVR, ZFS repo) live in iso/pins.env.
EOF
}

die() { echo "FATAL: $*" >&2; exit 1; }

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(tr -d '[:space:]' < "$REPO_ROOT/VERSION")"
[ -n "$VERSION" ] || die "VERSION file is empty"
# shellcheck disable=SC1091
. "$REPO_ROOT/iso/pins.env"
if [ -z "${KERNEL_NEVR:-}" ] || [ -z "${ZFS_REPO_URL:-}" ] || [ -z "${ZFS_SERIES:-}" ]; then
    die "iso/pins.env is missing KERNEL_NEVR / ZFS_REPO_URL / ZFS_SERIES"
fi

UPSTREAM_ISO="${UPSTREAM_ISO:-}"
UPSTREAM_SHA256="${UPSTREAM_SHA256:-}"
OUTPUT_DIR="${OUTPUT_DIR:-$REPO_ROOT/dist}"

while [ $# -gt 0 ]; do
    case "$1" in
        --upstream-iso) UPSTREAM_ISO="${2:?--upstream-iso needs a value}"; shift 2 ;;
        --sha256)       UPSTREAM_SHA256="${2:?--sha256 needs a value}"; shift 2 ;;
        --output)       OUTPUT_DIR="${2:?--output needs a value}"; shift 2 ;;
        -h|--help)      usage; exit 0 ;;
        *)              usage >&2; die "unknown argument: $1" ;;
    esac
done

# --- required tools ----------------------------------------------------------
missing=()
for tool in xorriso xorrisofs mcopy mmd mkfs.vfat mksquashfs createrepo_c \
            rpmbuild cargo dnf depmod implantisomd5 truncate sha256sum sed \
            awk dd cut; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if [ "${#missing[@]}" -gt 0 ]; then
    die "missing required tools: ${missing[*]}
  install with: dnf install xorriso mtools dosfstools squashfs-tools \\
      createrepo_c rpm-build rust cargo gtk4-devel isomd5sum kmod"
fi

# --- upstream ISO + checksum verification ------------------------------------
[ -n "$UPSTREAM_ISO" ] || { usage >&2; die "--upstream-iso is required"; }
[ -r "$UPSTREAM_ISO" ] || die "upstream ISO not readable: $UPSTREAM_ISO"

magic="$(dd if="$UPSTREAM_ISO" bs=1 skip=32769 count=5 2>/dev/null || true)"
[ "$magic" = "CD001" ] || die "not an ISO 9660 image: $UPSTREAM_ISO"

if [ -z "$UPSTREAM_SHA256" ]; then
    sidecar="${UPSTREAM_ISO}.sha256"
    [ -r "$sidecar" ] || die "no --sha256 given and $sidecar not found; refusing to build from an unverified ISO"
    UPSTREAM_SHA256="$(cut -d' ' -f1 < "$sidecar" | head -n1)"
fi
case "$UPSTREAM_SHA256" in
    *[!0-9a-fA-F]*|"") die "expected SHA-256 is not a hex string: '$UPSTREAM_SHA256'" ;;
esac

echo "==> Verifying upstream ISO checksum"
actual="$(sha256sum "$UPSTREAM_ISO" | cut -d' ' -f1)"
[ "$actual" = "${UPSTREAM_SHA256,,}" ] || die "upstream ISO checksum mismatch!
  file:     $UPSTREAM_ISO
  expected: ${UPSTREAM_SHA256,,}
  actual:   $actual"
echo "    OK: $actual"

WORK="$REPO_ROOT/build/iso"
TREE="$WORK/tree"
rm -rf "$WORK"
mkdir -p "$TREE/images" "$TREE/LiveOS" "$TREE/EFI/BOOT"

# --- on-media Minimal repo ----------------------------------------------------
echo "==> Extracting on-media Minimal repo from upstream ISO"
xorriso -osirrox on -indev "$UPSTREAM_ISO" -extract /Minimal "$TREE/Minimal" \
    >/dev/null 2>&1 || die "upstream ISO has no /Minimal repo (unexpected media layout)"
chmod -R u+w "$TREE/Minimal"

echo "==> Gate: kernel pin matches the media repo"
media_kernel="$(dnf repoquery --quiet --disablerepo='*' \
    --repofrompath="media,$TREE/Minimal" --setopt=media.gpgcheck=0 \
    --latest-limit=1 --qf '%{name}-%{version}-%{release}' kernel | head -n1)"
if [ "$media_kernel" != "$KERNEL_NEVR" ]; then
    die "iso/pins.env KERNEL_NEVR=$KERNEL_NEVR but the media carries '$media_kernel'.
  Update KERNEL_NEVR (and re-check ZFS_REPO_URL) in iso/pins.env."
fi

# --- lumen repo: our RPMs + pinned OpenZFS subset -----------------------------
bash "$REPO_ROOT/packages/build-rpms.sh"
mkdir -p "$TREE/lumen"
cp "$REPO_ROOT"/dist/rpms/*.noarch.rpm "$TREE/lumen/"

echo "==> Mirroring OpenZFS $ZFS_SERIES kABI subset from $ZFS_REPO_URL"
zfs_dl="$WORK/zfs-download"
mkdir -p "$zfs_dl"
dnf download --quiet --disablerepo='*' \
    --repofrompath="zfs,$ZFS_REPO_URL" --setopt=zfs.gpgcheck=0 \
    --exclude='*-debuginfo' --exclude='*-devel' \
    --destdir "$zfs_dl" \
    "zfs-$ZFS_SERIES*" "kmod-zfs-$ZFS_SERIES*" "zfs-dracut-$ZFS_SERIES*" \
    "libzfs*" "libnvpair*" "libuutil*" "libzpool*" \
    || die "OpenZFS download failed — is $ZFS_REPO_URL reachable and does it carry the $ZFS_SERIES series?"

echo "==> Gate: OpenZFS RPM signatures (pinned key)"
rpmkeys --import "$REPO_ROOT/iso/keys/RPM-GPG-KEY-openzfs-2022"
for rpm in "$zfs_dl"/*.rpm; do
    sig="$(rpmkeys --checksig "$rpm")"
    grep -q "signatures OK" <<<"$sig" \
        || die "RPM signature check failed: $sig"
done
mv "$zfs_dl"/*.rpm "$TREE/lumen/"

# ZFS userland dependencies the Minimal media repo lacks (see pins.env).
# Pulled from the build container's AlmaLinux repos — the almalinux:10
# container tracks the same point-release stream the upstream ISO pin
# targets (the kernel-pin gate forces both pins to move together).
if [ -n "${MEDIA_EXTRA_PACKAGES:-}" ]; then
    echo "==> Mirroring AlmaLinux extras: $MEDIA_EXTRA_PACKAGES"
    extra_dl="$WORK/alma-extra-download"
    mkdir -p "$extra_dl"
    # shellcheck disable=SC2086 # intentional word splitting of the package list
    dnf download --quiet --destdir "$extra_dl" $MEDIA_EXTRA_PACKAGES \
        || die "AlmaLinux extras download failed"

    echo "==> Gate: AlmaLinux extras RPM signatures"
    alma_key="/etc/pki/rpm-gpg/RPM-GPG-KEY-AlmaLinux-10"
    [ -r "$alma_key" ] || die "AlmaLinux GPG key not found at $alma_key (not building on AlmaLinux 10?)"
    rpmkeys --import "$alma_key"
    for rpm in "$extra_dl"/*.rpm; do
        sig="$(rpmkeys --checksig "$rpm")"
        grep -q "signatures OK" <<<"$sig" \
            || die "RPM signature check failed: $sig"
    done
    mv "$extra_dl"/*.rpm "$TREE/lumen/"
fi

echo "==> Creating lumen repo"
createrepo_c --quiet "$TREE/lumen"

echo "==> Gate: target package set resolves offline (media repos only)"
resolve_root="$WORK/resolve-root"
mkdir -p "$resolve_root"
set +e
resolve_out="$(dnf --assumeno --installroot="$resolve_root" --releasever=10 \
    --disablerepo='*' \
    --repofrompath="media,$TREE/Minimal" --repofrompath="lumen,$TREE/lumen" \
    --setopt=media.gpgcheck=0 --setopt=lumen.gpgcheck=0 \
    install @core "$KERNEL_NEVR" zfs kmod-zfs zfs-dracut \
        grub2-efi-x64 shim-x64 grub2-tools grubby efibootmgr \
        e2fsprogs dosfstools NetworkManager chrony firewalld openssh-server \
        lumen-release lumen-networking 2>&1)"
set -e
# --assumeno exits nonzero after successfully resolving; a printed
# transaction summary is the actual success signal.
grep -q "Transaction Summary" <<<"$resolve_out" || {
    echo "$resolve_out" | tail -40 >&2
    die "offline resolution failed: the ISO would not be able to install the target"
}
rm -rf "$resolve_root"

# --- installer binary ---------------------------------------------------------
echo "==> Building lumen-installer (cargo, release)"
locked=()
[ -f "$REPO_ROOT/lumen-installer/app/Cargo.lock" ] && locked=(--locked)
cargo build --release "${locked[@]}" \
    --manifest-path "$REPO_ROOT/lumen-installer/app/Cargo.toml" \
    --target-dir "$REPO_ROOT/build/cargo-target"
INSTALLER_BINARY="$REPO_ROOT/build/cargo-target/release/lumen-installer"

# --- live environment ---------------------------------------------------------
bash "$REPO_ROOT/lumen-installer/live/build-live.sh" \
    --workdir "$WORK/live" \
    --installer-binary "$INSTALLER_BINARY" \
    --lumen-repo "$TREE/lumen" \
    --kernel-nevr "$KERNEL_NEVR" \
    --version "$VERSION" \
    --out-tree "$TREE"
LIVEROOT="$WORK/live/rootfs"

# --- boot configuration -------------------------------------------------------
VOLID="LUMEN"
CMDLINE="root=live:CDLABEL=$VOLID rd.live.image rd.live.overlay.overlayfs=1 rd.live.dir=/LiveOS selinux=0"
cat > "$TREE/EFI/BOOT/grub.cfg" <<EOF
set default=0
set timeout=10

menuentry 'Install Lumen $VERSION' {
    linux /images/vmlinuz $CMDLINE console=ttyS0,115200 console=tty0 quiet
    initrd /images/initrd.img
}
menuentry 'Install Lumen $VERSION (verify media first)' {
    linux /images/vmlinuz $CMDLINE rd.live.check quiet
    initrd /images/initrd.img
}
menuentry 'Install Lumen $VERSION (verbose)' {
    linux /images/vmlinuz $CMDLINE
    initrd /images/initrd.img
}
EOF

# --- ESP image (created from scratch: mkfs.vfat + mtools, no loop mounts) ----
echo "==> Building EFI system partition image"
ESP="$WORK/efiboot.img"
truncate -s 24M "$ESP"
mkfs.vfat -n LUMENEFI "$ESP" >/dev/null
mmd -i "$ESP" ::/EFI ::/EFI/BOOT ::/EFI/almalinux
# shim (BOOTX64.EFI) + signed grub from the live rootfs RPMs.
[ -f "$LIVEROOT/boot/efi/EFI/BOOT/BOOTX64.EFI" ] || die "shim not found in live rootfs (shim-x64 missing?)"
mcopy -i "$ESP" "$LIVEROOT/boot/efi/EFI/BOOT/BOOTX64.EFI" ::/EFI/BOOT/
grubx64="$LIVEROOT/boot/efi/EFI/almalinux/grubx64.efi"
[ -f "$grubx64" ] || die "grubx64.efi not found in live rootfs (grub2-efi-x64 missing?)"
mcopy -i "$ESP" "$grubx64" ::/EFI/BOOT/
mcopy -i "$ESP" "$grubx64" ::/EFI/almalinux/
if [ -f "$LIVEROOT/boot/efi/EFI/almalinux/mmx64.efi" ]; then
    mcopy -i "$ESP" "$LIVEROOT/boot/efi/EFI/almalinux/mmx64.efi" ::/EFI/BOOT/
fi
# Stub config in both prefixes grub might use; chains to the ISO's config.
cat > "$WORK/esp-grub.cfg" <<EOF
search --no-floppy --set=root -l '$VOLID'
set prefix=(\$root)/EFI/BOOT
configfile \$prefix/grub.cfg
EOF
mcopy -i "$ESP" "$WORK/esp-grub.cfg" ::/EFI/BOOT/grub.cfg
mcopy -i "$ESP" "$WORK/esp-grub.cfg" ::/EFI/almalinux/grub.cfg

# --- assemble the ISO (UEFI-only) ---------------------------------------------
OUT_ISO="$OUTPUT_DIR/lumen-$VERSION-x86_64.iso"
mkdir -p "$OUTPUT_DIR"
rm -f "$OUT_ISO" "$OUT_ISO.sha256"

echo "==> Building ISO (xorrisofs, UEFI-only)"
xorrisofs -o "$OUT_ISO" \
    -R -J -joliet-long \
    -V "$VOLID" \
    --protective-msdos-label \
    -partition_cyl_align off \
    -partition_offset 16 \
    -append_partition 2 C12A7328-F81F-11D2-BA4B-00A0C93EC93B "$ESP" \
    -appended_part_as_gpt \
    --boot-catalog-hide \
    -e '--interval:appended_partition_2:all::' \
    -no-emul-boot \
    "$TREE" >/dev/null

echo "==> Implanting ISO md5 (rd.live.check support)"
implantisomd5 "$OUT_ISO" >/dev/null

# Sanity: UEFI boot entry, volid, GPT ESP present.
et="$(xorriso -indev "$OUT_ISO" -report_el_torito plain 2>/dev/null)"
grep -q "UEFI" <<<"$et" || die "output ISO lost its UEFI boot entry"
xorriso -indev "$OUT_ISO" -pvd_info 2>/dev/null | grep -q "Volume Id    : $VOLID" \
    || die "output ISO volume id is not $VOLID"
xorriso -indev "$OUT_ISO" -report_system_area plain 2>/dev/null \
    | grep -qi "28732ac11ff8d211ba4b00a0c93ec93b" \
    || die "output ISO has no GPT EFI system partition"

(
    cd "$OUTPUT_DIR"
    sha256sum "$(basename "$OUT_ISO")" > "$(basename "$OUT_ISO").sha256"
)

echo "==> Done:"
ls -l "$OUT_ISO" "$OUT_ISO.sha256"
