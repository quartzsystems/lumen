#!/usr/bin/env bash
# Build the Lumen appliance ISO from an upstream AlmaLinux 10 x86_64
# minimal/boot ISO:
#
#   1. verify the upstream ISO's SHA-256 against a trusted value
#   2. build the Lumen RPMs and create a local package repo
#   3. render the kickstart template (version + build date from VERSION)
#   4. remaster with mkksiso: embed kickstart + repo, set volid LUMEN
#
# Output: dist/lumen-<version>-x86_64.iso (+ .sha256)
#
# Note: mkksiso must update the volume label inside the EFI boot image; on
# most systems this needs mtools (mcopy) or root privileges. See
# docs/build.md.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: build-iso.sh --upstream-iso PATH [--sha256 HASH] [--output DIR]

  --upstream-iso PATH  Upstream AlmaLinux 10 x86_64 minimal/boot ISO (required)
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
for tool in mkksiso createrepo_c rpmbuild ksvalidator sha256sum sed cut; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if [ "${#missing[@]}" -gt 0 ]; then
    die "missing required tools: ${missing[*]}
  install with: dnf install lorax createrepo_c rpm-build pykickstart"
fi
command -v mcopy >/dev/null 2>&1 || \
    echo "WARNING: mtools (mcopy) not found; mkksiso may fail to update the EFI boot image" >&2

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
"$REPO_ROOT/packages/build-rpms.sh"

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

# --- remaster the ISO -------------------------------------------------------
OUT_ISO="$OUTPUT_DIR/lumen-$VERSION-x86_64.iso"
mkdir -p "$OUTPUT_DIR"
rm -f "$OUT_ISO" "$OUT_ISO.sha256"

echo "==> Running mkksiso"
mkksiso --volid LUMEN \
        --ks "$WORK/lumen.ks" \
        --add "$WORK/lumen" \
        "$UPSTREAM_ISO" "$OUT_ISO"

(
    cd "$OUTPUT_DIR"
    sha256sum "$(basename "$OUT_ISO")" > "$(basename "$OUT_ISO").sha256"
)

echo "==> Done:"
ls -l "$OUT_ISO" "$OUT_ISO.sha256"
