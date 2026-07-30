#!/usr/bin/env bash
# Build the Lumen RPMs: the noarch branding/tooling packages (lumen-release,
# lumen-logos, lumen-networking, lumen-storage, lumen-compute) plus the
# arch-specific lumen-controlplane (management daemon + web console).
#
# The controlplane artifacts are produced here, before rpmbuild: cargo needs
# crates.io and npm needs its registry, so the compile happens in the normal
# build environment and the spec packages the prebuilt results. Requires:
# rust/cargo, pam-devel (libpam headers), libvirt-devel (the hypervisor client
# library the compute domain links), nodejs/npm >= 20, and checkpolicy plus
# policycoreutils (the control plane ships an SELinux module).
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
        || die "$tool not found (dnf install rpm-build rust cargo pam-devel libvirt-devel nodejs npm)"
done

# The SELinux module compiler. Checked separately because its package names
# look nothing like the command names, and a build that got this far and then
# died on "checkmodule: not found" would send someone hunting.
for tool in checkmodule semodule_package; do
    command -v "$tool" >/dev/null 2>&1 \
        || die "$tool not found (dnf install checkpolicy policycoreutils)"
done

# The two libraries the control plane links are checked here, not left to the
# linker: without them cargo compiles happily for several minutes (virt-sys
# ships generated bindings, so no header is needed to *build*) and then dies
# in ld on undefined references to virConnectOpen &c. Fail in seconds instead.
[ -e /usr/include/security/pam_appl.h ] \
    || die "libpam headers not found (dnf install pam-devel)"
pkg-config --exists libvirt 2>/dev/null \
    || die "libvirt client library not found (dnf install libvirt-devel pkgconf-pkg-config)"
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

# The SELinux module, compiled here rather than in the spec for the same
# reason everything else is: rpmbuild stays free of toolchain requirements.
# Without it `systemd-run --pipe` cannot pass the daemon's pipes across the
# bus, and every privileged command the daemon delegates fails — see
# lumen-controlplane/selinux/lumen-controlplane.te and docs/system.md.
echo "==> Building lumen-controlplane SELinux module"
(
    cd "$TOPDIR/SOURCES"
    checkmodule -M -m -o lumen-controlplane.mod \
        "$REPO_ROOT/lumen-controlplane/selinux/lumen-controlplane.te"
    semodule_package -o lumen-controlplane.pp -m lumen-controlplane.mod
    rm -f lumen-controlplane.mod
)
[ -s "$TOPDIR/SOURCES/lumen-controlplane.pp" ] \
    || die "SELinux module not produced (lumen-controlplane.pp missing or empty)"

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
# The network policy fragment shares a very common basename; stage it under a
# distinct source name so it cannot collide in the flat SOURCES dir.
cp "$REPO_ROOT/lumen-networking/system/NetworkManager/00-lumen.conf" \
   "$TOPDIR/SOURCES/lumen-nm-00-lumen.conf"
# Service policy for the two domains that delegate their privileged work,
# plus the cluster stack's keep-it-off preset and the firewalld service
# definitions the cluster networks bind.
cp "$REPO_ROOT/lumen-compute/system/systemd/50-lumen-compute.preset" \
   "$REPO_ROOT/lumen-storage/system/systemd/50-lumen-storage.preset" \
   "$REPO_ROOT/lumen-storage/system/systemd/50-lumen-cluster.preset" \
   "$REPO_ROOT/lumen-storage/system/firewalld/lumen-cluster.xml" \
   "$REPO_ROOT/lumen-storage/system/firewalld/lumen-replication.xml" \
   "$REPO_ROOT/lumen-storage/system/firewalld/lumen-pool.xml" \
   "$REPO_ROOT/lumen-storage/system/sysctl/50-lumen-pool.conf" \
   "$TOPDIR/SOURCES/"

# --- repository configuration (only when the signing key is in the tree) ------
# lumen-repo ships the public key an appliance verifies Lumen's packages and
# repository index against, so it cannot be built without one. That is a
# requirement on this package alone and deliberately not on the build: a
# checkout without the key still produces every other package, which is what
# lets a contributor build and test the appliance without being handed key
# material. The key is generated once and committed; docs/updates.md has the
# procedure.
REPO_KEY="$REPO_ROOT/packages/keys/RPM-GPG-KEY-lumen"
REPO_BASEURL="${LUMEN_REPO_BASEURL:-https://lumen.quartz.systems/repo}"
BUILD_REPO_PACKAGE=0
if [ -s "$REPO_KEY" ]; then
    BUILD_REPO_PACKAGE=1
    echo "==> Staging lumen-repo (base $REPO_BASEURL)"
    sed "s|@BASEURL@|$REPO_BASEURL|g" \
        "$REPO_ROOT/packages/lumen.repo.in" > "$TOPDIR/SOURCES/lumen.repo"
    cp "$REPO_KEY" "$TOPDIR/SOURCES/RPM-GPG-KEY-lumen"
else
    echo "==> NOTE: $REPO_KEY is absent, so lumen-repo will not be built."
    echo "    Installed appliances get their updates from that package; without"
    echo "    it they have no Lumen repository configured. See docs/updates.md"
    echo "    for how to generate the key and where the private half belongs."
fi

for spec in "$REPO_ROOT"/packages/*.spec; do
    if [ "$(basename "$spec")" = "lumen-repo.spec" ] && [ "$BUILD_REPO_PACKAGE" -eq 0 ]; then
        continue
    fi
    echo "==> rpmbuild $(basename "$spec") (version $VERSION)"
    rpmbuild -ba \
        --define "_topdir $TOPDIR" \
        --define "lumen_version $VERSION" \
        "$spec"
done

find "$TOPDIR/RPMS" "$TOPDIR/SRPMS" -name '*.rpm' -exec cp -f {} "$DIST_DIR/" \;
echo "==> RPMs in $DIST_DIR:"
ls -l "$DIST_DIR"
