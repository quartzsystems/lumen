#!/usr/bin/env bash
# Build the Lumen appliance ISO from an upstream AlmaLinux 10 x86_64
# minimal ISO:
#
#   1. verify the upstream ISO's SHA-256 against a trusted value
#   2. build the Lumen RPMs and create a local package repo
#   3. render the kickstart template (version + build date from VERSION)
#   4. remaster the ISO: extract the tree, brand the boot configs, embed
#      the kickstart + local repo, set volume label LUMEN, and rebuild
#      with xorrisofs
#
# Output: dist/lumen-<version>-x86_64.iso (+ .sha256)
#
# Why not mkksiso? lorax 40.x's mkksiso fails on AlmaLinux/RHEL 10 media
# ("Cannot enable EL Torito boot image #2...") because these ISOs keep the
# EFI boot image only in an appended GPT partition, and it also needs root
# for loop mounts. This script edits the EFI image with mtools instead and
# rebuilds with the same xorrisofs recipe lorax uses to create the media,
# so it runs fully unprivileged (works in CI containers).
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: build-iso.sh --upstream-iso PATH [--sha256 HASH] [--output DIR]

  --upstream-iso PATH  Upstream AlmaLinux 10 x86_64 minimal ISO (required)
  --sha256 HASH        Expected SHA-256 of the upstream ISO. If omitted, the
                       script reads PATH.sha256 (first word = hash). Get the
                       official value from the CHECKSUM file at
                       https://almalinux.org/get-almalinux/
  --output DIR         Output directory (default: <repo>/dist)

The flags may also be provided via the UPSTREAM_ISO / UPSTREAM_SHA256 /
OUTPUT_DIR environment variables.
EOF
}

die() { echo "FATAL: $*" >&2; exit 1; }

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(tr -d '[:space:]' < "$REPO_ROOT/VERSION")"
[ -n "$VERSION" ] || die "VERSION file is empty"

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

# --- required tools: report everything missing, then fail loudly ----------
missing=()
for tool in xorriso xorrisofs mcopy mdel createrepo_c rpmbuild ksvalidator \
            implantisomd5 sha256sum sed awk dd cut; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if [ "${#missing[@]}" -gt 0 ]; then
    die "missing required tools: ${missing[*]}
  install with: dnf install xorriso mtools createrepo_c rpm-build pykickstart isomd5sum"
fi

# --- upstream ISO + checksum verification ---------------------------------
[ -n "$UPSTREAM_ISO" ] || { usage >&2; die "--upstream-iso is required"; }
[ -r "$UPSTREAM_ISO" ] || die "upstream ISO not readable: $UPSTREAM_ISO"

# ISO 9660 magic ("CD001" at offset 32769) — xorriso silently treats a
# non-ISO input as a blank image, so reject junk before it gets that far.
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
if [ "$actual" != "${UPSTREAM_SHA256,,}" ]; then
    die "upstream ISO checksum mismatch!
  file:     $UPSTREAM_ISO
  expected: ${UPSTREAM_SHA256,,}
  actual:   $actual"
fi
echo "    OK: $actual"

# --- build RPMs and the local 'lumen' repo --------------------------------
bash "$REPO_ROOT/packages/build-rpms.sh"

WORK="$REPO_ROOT/build/iso"
rm -rf "$WORK"
mkdir -p "$WORK/lumen"
cp "$REPO_ROOT"/dist/rpms/*.noarch.rpm "$WORK/lumen/"
echo "==> Creating local package repo"
createrepo_c --quiet "$WORK/lumen"

# --- render + validate the kickstart ---------------------------------------
BUILD_DATE="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
sed -e "s/@LUMEN_VERSION@/$VERSION/g" \
    -e "s/@LUMEN_BUILD_DATE@/$BUILD_DATE/g" \
    "$REPO_ROOT/iso/lumen.ks.in" > "$WORK/lumen.ks"

echo "==> Validating kickstart (RHEL10 profile)"
ksvalidator -v RHEL10 "$WORK/lumen.ks"

# --- inspect the upstream ISO ----------------------------------------------
VOLID="LUMEN"
OLD_VOLID="$(xorriso -indev "$UPSTREAM_ISO" -pvd_info 2>/dev/null \
             | sed -n 's/^Volume Id *: //p')"
[ -n "$OLD_VOLID" ] || die "could not read volume id from $UPSTREAM_ISO"
echo "==> Upstream volume id: $OLD_VOLID"

# Locate the appended EFI system partition (GPT type C12A7328-...) so its
# FAT image can be extracted and re-branded. Units are 512-byte blocks.
sysarea="$(xorriso -indev "$UPSTREAM_ISO" -report_system_area plain 2>/dev/null)"
efi_part="$(awk '/^GPT type GUID/ && /28732ac11ff8d211ba4b00a0c93ec93b/ {print $5}' <<<"$sysarea" | head -n1)"
[ -n "$efi_part" ] || die "no EFI system partition found in $UPSTREAM_ISO (unexpected media layout)"
read -r efi_start efi_size < <(awk -v p="$efi_part" \
    '$1=="GPT" && $2=="start" && $6==p {print $7, $8}' <<<"$sysarea")
if [ -z "${efi_start:-}" ] || [ -z "${efi_size:-}" ]; then
    die "could not determine EFI partition geometry"
fi
echo "==> EFI partition: #$efi_part start=$efi_start size=$efi_size (512-byte blocks)"

# --- extract the ISO tree ---------------------------------------------------
TREE="$WORK/tree"
mkdir -p "$TREE"
echo "==> Extracting ISO tree"
xorriso -osirrox on -indev "$UPSTREAM_ISO" -extract / "$TREE" >/dev/null 2>&1
chmod -R u+w "$TREE"
[ -f "$TREE/images/eltorito.img" ] || die "unexpected media layout: no images/eltorito.img (BIOS boot image)"
[ -f "$TREE/EFI/BOOT/grub.cfg" ]   || die "unexpected media layout: no EFI/BOOT/grub.cfg"

# --- brand the boot configuration -------------------------------------------
# Point every LABEL= reference at the new volid and append the kickstart to
# each kernel command line (grub2 config on the ISO plus BOOT.conf copy).
KS_ARG="inst.ks=hd:LABEL=$VOLID:/lumen.ks"
brand_grub_cfg() {
    sed -i -e "s/$OLD_VOLID/$VOLID/g" \
           -e "/^[[:space:]]*linux\(efi\)\{0,1\}[[:space:]]/s|\$| $KS_ARG|" "$1"
}
echo "==> Branding boot configs"
brand_grub_cfg "$TREE/EFI/BOOT/grub.cfg"
brand_grub_cfg "$TREE/boot/grub2/grub.cfg"
[ -f "$TREE/EFI/BOOT/BOOT.conf" ] && brand_grub_cfg "$TREE/EFI/BOOT/BOOT.conf"

# --- brand the EFI boot image (mtools: no root, no loop mounts) -------------
EFIBOOT="$WORK/efiboot.img"
dd if="$UPSTREAM_ISO" of="$EFIBOOT" bs=512 skip="$efi_start" count="$efi_size" status=none
for cfg in grub.cfg BOOT.conf; do
    if mcopy -n -i "$EFIBOOT" "::/EFI/BOOT/$cfg" "$WORK/efi-$cfg" 2>/dev/null; then
        brand_grub_cfg "$WORK/efi-$cfg"
        mdel  -i "$EFIBOOT" "::/EFI/BOOT/$cfg"
        mcopy -i "$EFIBOOT" "$WORK/efi-$cfg" "::/EFI/BOOT/$cfg"
    fi
done

# --- add the Lumen payload ---------------------------------------------------
cp -r "$WORK/lumen" "$TREE/lumen"
cp "$WORK/lumen.ks" "$TREE/lumen.ks"

# --- rebuild the ISO ---------------------------------------------------------
# Same recipe lorax/xorrisofs uses for AlmaLinux 10 x86_64 media (verified
# against `xorriso -report_el_torito as_mkisofs` of the upstream ISO):
# grub2 MBR taken from the upstream ISO's system area, BIOS boot via
# images/eltorito.img, UEFI boot via the appended EFI partition.
OUT_ISO="$OUTPUT_DIR/lumen-$VERSION-x86_64.iso"
mkdir -p "$OUTPUT_DIR"
rm -f "$OUT_ISO" "$OUT_ISO.sha256"

echo "==> Rebuilding ISO (xorrisofs)"
xorrisofs -o "$OUT_ISO" \
    -R -J -joliet-long \
    -V "$VOLID" \
    --grub2-mbr "--interval:local_fs:0s-15s:zero_mbrpt,zero_gpt:$UPSTREAM_ISO" \
    --protective-msdos-label \
    -partition_cyl_align off \
    -partition_offset 0 \
    -partition_hd_cyl 99 \
    -partition_sec_hd 32 \
    -append_partition 2 C12A7328-F81F-11D2-BA4B-00A0C93EC93B "$EFIBOOT" \
    -appended_part_as_gpt \
    --boot-catalog-hide \
    -b images/eltorito.img \
    -no-emul-boot -boot-load-size 4 -boot-info-table --grub2-boot-info \
    -eltorito-alt-boot \
    -e '--interval:appended_partition_2:all::' \
    -no-emul-boot \
    "$TREE" >/dev/null

echo "==> Implanting ISO md5 (rd.live.check support)"
implantisomd5 "$OUT_ISO" >/dev/null

# Sanity: the result must carry both boot entries and the LUMEN volid.
et="$(xorriso -indev "$OUT_ISO" -report_el_torito plain 2>/dev/null)"
grep -q "El Torito boot img :   1  BIOS" <<<"$et" || die "output ISO lost its BIOS boot entry"
grep -q "El Torito boot img :   2  UEFI" <<<"$et" || die "output ISO lost its UEFI boot entry"
xorriso -indev "$OUT_ISO" -pvd_info 2>/dev/null | grep -q "Volume Id    : $VOLID" \
    || die "output ISO volume id is not $VOLID"

(
    cd "$OUTPUT_DIR"
    sha256sum "$(basename "$OUT_ISO")" > "$(basename "$OUT_ISO").sha256"
)

echo "==> Done:"
ls -l "$OUT_ISO" "$OUT_ISO.sha256"
