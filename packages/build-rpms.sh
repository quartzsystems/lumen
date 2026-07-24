#!/usr/bin/env bash
# Build the Lumen noarch RPMs (lumen-release, lumen-logos).
#
# The version is read from the top-level VERSION file and injected into the
# specs as %{lumen_version}. Outputs land in dist/rpms/.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(tr -d '[:space:]' < "$REPO_ROOT/VERSION")"
TOPDIR="$REPO_ROOT/build/rpmbuild"
DIST_DIR="$REPO_ROOT/dist/rpms"

die() { echo "FATAL: $*" >&2; exit 1; }

command -v rpmbuild >/dev/null 2>&1 || die "rpmbuild not found (dnf install rpm-build)"
[ -n "$VERSION" ] || die "VERSION file is empty"

rm -rf "$TOPDIR"
mkdir -p "$TOPDIR"/{SOURCES,BUILD,RPMS,SRPMS} "$DIST_DIR"

# Stage spec sources out of branding/ into a single rpmbuild SOURCES dir.
cp "$REPO_ROOT"/branding/release/lumen-release.in \
   "$REPO_ROOT"/branding/release/os-release-lumen.conf.in \
   "$REPO_ROOT"/branding/release/issue.in \
   "$REPO_ROOT"/branding/release/motd.in \
   "$REPO_ROOT"/branding/grub/theme.txt \
   "$REPO_ROOT"/branding/grub/lumen-grub-bg.png \
   "$REPO_ROOT"/branding/logos/lumen-mark.svg \
   "$REPO_ROOT"/branding/logos/lumen-mark-light-bg.svg \
   "$REPO_ROOT"/branding/logos/lumen-favicon.svg \
   "$REPO_ROOT"/branding/logos/lumen-lockup-dark-bg.svg \
   "$REPO_ROOT"/branding/logos/lumen-lockup-light-bg.svg \
   "$REPO_ROOT"/branding/logos/png/lumen-favicon-64.png \
   "$REPO_ROOT"/branding/logos/png/lumen-lockup-dark-bg.png \
   "$REPO_ROOT"/branding/logos/png/lumen-lockup-light-bg.png \
   "$REPO_ROOT"/branding/logos/png/lumen-mark-1024.png \
   "$REPO_ROOT"/branding/logos/README.md \
   "$REPO_ROOT"/branding/plymouth/lumen.plymouth \
   "$REPO_ROOT"/lumen-networking/nicnames/lumen-nicnames \
   "$TOPDIR/SOURCES/"

for spec in "$REPO_ROOT"/packages/*.spec; do
    echo "==> rpmbuild $(basename "$spec") (version $VERSION)"
    rpmbuild -ba \
        --define "_topdir $TOPDIR" \
        --define "lumen_version $VERSION" \
        "$spec"
done

find "$TOPDIR/RPMS" "$TOPDIR/SRPMS" -name '*.rpm' -exec cp -f {} "$DIST_DIR/" \;
echo "==> RPMs in $DIST_DIR:"
ls -l "$DIST_DIR"
