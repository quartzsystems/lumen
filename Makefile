# Lumen build entry points — see README.md and docs/build.md.
#
# make rpms                          build lumen-release/-logos/-networking/
#                                    -storage/-compute
# make installer                     cargo build the Rust installer (release)
# make controlplane                  cargo build lumen-controlplane (release)
# make webui                         Next.js static export -> lumen-webui/out
# make iso UPSTREAM_ISO=... \
#          [UPSTREAM_SHA256=...]     build dist/lumen-<version>-x86_64.iso
# make test                          installer + domain crates + controlplane
# make lint                          shellcheck + rpmlint + fmt/clippy
# make clean                         remove build/ and dist/

VERSION := $(shell cat VERSION)

SCRIPTS := packages/build-rpms.sh iso/build-live-iso.sh \
           lumen-installer/live/build-live.sh \
           lumen-networking/nicnames/lumen-nicnames \
           branding/console/lumen-console-banner \
           branding/console/50-lumen-banner
SPECS   := packages/lumen-release.spec packages/lumen-logos.spec \
           packages/lumen-networking.spec packages/lumen-storage.spec \
           packages/lumen-compute.spec
CARGO_MANIFEST := lumen-installer/app/Cargo.toml
CP_MANIFEST    := lumen-controlplane/Cargo.toml
# The four domain crates are path dependencies of the control plane, not
# workspace members (the manifests are independent by design — see
# docs/networking.md and docs/compute.md), so each gets its own
# fmt/clippy/test invocation. lumen-sys is first because it is the most basic:
# it depends on none of the others, and lumen-zfs depends on it.
SYS_MANIFEST   := lumen-system/lumen-sys/Cargo.toml
NET_MANIFEST   := lumen-networking/lumen-net/Cargo.toml
ZFS_MANIFEST   := lumen-storage/lumen-zfs/Cargo.toml
VIRT_MANIFEST  := lumen-compute/lumen-virt/Cargo.toml

.PHONY: all rpms installer controlplane webui iso test lint clean

all: rpms

# Scripts are invoked via bash so builds work even if a checkout loses the
# executable bits (easy to do when developing on Windows).
rpms:
	bash packages/build-rpms.sh

installer:
	cargo build --release --manifest-path $(CARGO_MANIFEST) \
		--target-dir build/cargo-target

# Needs libpam headers (pam-devel / libpam0g-dev) and the hypervisor client
# library (libvirt-devel / libvirt-dev). Neither pulls a code generator into
# the toolchain — see docs/compute.md.
controlplane:
	cargo build --release --manifest-path $(CP_MANIFEST) \
		--target-dir build/cargo-target-cp

webui:
	cd lumen-webui && npm ci && npm run build

iso:
	UPSTREAM_ISO="$(UPSTREAM_ISO)" UPSTREAM_SHA256="$(UPSTREAM_SHA256)" \
		bash iso/build-live-iso.sh

# Every domain crate's tests run entirely against its in-memory backend: no
# system bus, no hypervisor, no pools, nothing touched on the machine running
# them. Building lumen-virt still needs libvirt-devel to link against;
# running the tests does not need a hypervisor.
test:
	cargo test --manifest-path $(CARGO_MANIFEST) \
		--target-dir build/cargo-target
	cargo test --manifest-path $(SYS_MANIFEST) \
		--target-dir build/cargo-target-sys
	cargo test --manifest-path $(NET_MANIFEST) \
		--target-dir build/cargo-target-net
	cargo test --manifest-path $(ZFS_MANIFEST) \
		--target-dir build/cargo-target-zfs
	cargo test --manifest-path $(VIRT_MANIFEST) \
		--target-dir build/cargo-target-virt
	cargo test --manifest-path $(CP_MANIFEST) \
		--target-dir build/cargo-target-cp

lint:
	shellcheck $(SCRIPTS)
	rpmlint $(SPECS)
	cargo fmt --manifest-path $(CARGO_MANIFEST) --check
	cargo clippy --manifest-path $(CARGO_MANIFEST) \
		--target-dir build/cargo-target -- -D warnings
	cargo fmt --manifest-path $(SYS_MANIFEST) --check
	cargo clippy --manifest-path $(SYS_MANIFEST) --all-targets \
		--target-dir build/cargo-target-sys -- -D warnings
	cargo fmt --manifest-path $(NET_MANIFEST) --check
	cargo clippy --manifest-path $(NET_MANIFEST) --all-targets \
		--target-dir build/cargo-target-net -- -D warnings
	cargo fmt --manifest-path $(ZFS_MANIFEST) --check
	cargo clippy --manifest-path $(ZFS_MANIFEST) --all-targets \
		--target-dir build/cargo-target-zfs -- -D warnings
	cargo fmt --manifest-path $(VIRT_MANIFEST) --check
	cargo clippy --manifest-path $(VIRT_MANIFEST) --all-targets \
		--target-dir build/cargo-target-virt -- -D warnings
	cargo fmt --manifest-path $(CP_MANIFEST) --check
	cargo clippy --manifest-path $(CP_MANIFEST) --all-targets \
		--target-dir build/cargo-target-cp -- -D warnings

clean:
	rm -rf build dist
