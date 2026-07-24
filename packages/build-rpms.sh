#!/usr/bin/env bash
# Build the Lumen RPMs: the noarch branding/tooling packages (lumen-release,
# lumen-logos, lumen-networking) plus the arch-specific lumen-controlplane
# (management daemon + web console).
#
# The controlplane artifacts are produced here, before rpmbuild: cargo needs
# crates.io and npm needs its registry, so the compile happens in the normal
# build environment and the spec packages the prebuilt results. Requires:
# rust/cargo, pam-devel (libpam headers), nodejs/npm >= 20.
#
# The version is read from the top-level VERSION file and injected into the
# specs as %{lumen_version}. Outputs land in dist/rpms/.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(tr -d '[:space:]' < "$REPO_ROOT/VERSION")"
TOPDIR="$REPO_ROOT/build/rpmbuild"
DIST_DIR="$REPO_ROOT/dist/rpms"

die() { echo "FATAL: $*" >&2; exit 1; }

for tool in rpmbuild cargo npm; do
    command -v "$tool" >/dev/null 2>&1 \
        || die "$tool not found (dnf install rpm-build rust cargo pam-devel nodejs npm)"
done
[ -n "$VERSION" ] || die "VERSION file is empty"

rm -rf "$TOPDIR"
mkdir -p "$TOPDIR"/{SOURCES,BUILD,RPMS,SRPMS} "$DIST_DIR"

# The first crates.io contact is the fragile step of a cold-cache CI build
# (a 2026-07-24 ISO run died on three straight connect failures inside
# cargo's ~15s of built-in retries). Warm the registry with our own
# backoff before building; the build itself then barely needs the network.
cargo_fetch_with_retry() {
    local manifest="$1" attempt
    for attempt in 1 2 3 4; do
        CARGO_NET_RETRY=10 cargo fetch --locked --manifest-path "$manifest" && return 0
        [ "$attempt" -lt 4 ] || break
        echo "==> cargo fetch failed (attempt $attempt/4); retrying in $((attempt * 10))s"
        sleep $((attempt * 10))
    done
    die "cargo fetch failed after 4 attempts (crates.io unreachable?)"
}

# --- controlplane artifacts ---------------------------------------------------
echo "==> Building lumen-controlplane (cargo, release)"
cargo_fetch_with_retry "$REPO_ROOT/lumen-controlplane/Cargo.toml"
cargo build --release --locked \
    --manifest-path "$REPO_ROOT/lumen-controlplane/Cargo.toml" \
    --target-dir "$REPO_ROOT/build/cargo-target-cp"

echo "==> Building lumen-webui (static export)"
(cd "$REPO_ROOT/lumen-webui" && \
    npm ci --no-audit --no-fund --fetch-retries=5 --fetch-retry-maxtimeout=60000 && \
    npm run build)
[ -f "$REPO_ROOT/lumen-webui/out/index.html" ] \
    || die "webui export missing (lumen-webui/out/index.html not produced)"
tar -czf "$TOPDIR/SOURCES/lumen-webui.tar.gz" -C "$REPO_ROOT/lumen-webui/out" .

cp "$REPO_ROOT/build/cargo-target-cp/release/lumen-controlplane" "$TOPDIR/SOURCES/"
# The PAM file shares the daemon's name in-tree; stage it under a distinct
# source name so it can't collide with the binary in SOURCES/.
cp "$REPO_ROOT/lumen-controlplane/pam/lumen-controlplane" \
   "$TOPDIR/SOURCES/lumen-controlplane.pam"
cp "$REPO_ROOT/lumen-controlplane/systemd/lumen-controlplane.service" \
   "$REPO_ROOT/lumen-controlplane/firewalld/lumen-controlplane.xml" \
   "$TOPDIR/SOURCES/"

# --- spec sources from branding/ ----------------------------------------------
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
   "$REPO_ROOT"/branding/console/lumen-console-banner \
   "$REPO_ROOT"/branding/console/lumen-console-banner.service \
   "$REPO_ROOT"/branding/console/50-lumen-banner \
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
