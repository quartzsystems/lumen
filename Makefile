# Lumen build entry points — see README.md and docs/build.md.
#
# make rpms                          build lumen-release/-logos/-networking
# make installer                     cargo build the Rust installer (release)
# make controlplane                  cargo build lumen-controlplane (release)
# make webui                         Next.js static export -> lumen-webui/out
# make iso UPSTREAM_ISO=... \
#          [UPSTREAM_SHA256=...]     build dist/lumen-<version>-x86_64.iso
# make test                          installer + networking + controlplane tests
# make lint                          shellcheck + rpmlint + fmt/clippy
# make clean                         remove build/ and dist/

VERSION := $(shell cat VERSION)

SCRIPTS := packages/build-rpms.sh iso/build-live-iso.sh \
           lumen-installer/live/build-live.sh \
           lumen-networking/nicnames/lumen-nicnames \
           branding/console/lumen-console-banner \
           branding/console/50-lumen-banner
SPECS   := packages/lumen-release.spec packages/lumen-logos.spec \
           packages/lumen-networking.spec
CARGO_MANIFEST := lumen-installer/app/Cargo.toml
CP_MANIFEST    := lumen-controlplane/Cargo.toml
# lumen-net is a path dependency of the control plane, not a workspace member
# (the three manifests are independent by design — see docs/networking.md), so
# it gets its own fmt/clippy/test invocations.
NET_MANIFEST   := lumen-networking/lumen-net/Cargo.toml

.PHONY: all rpms installer controlplane webui iso test lint clean

all: rpms

# Scripts are invoked via bash so builds work even if a checkout loses the
# executable bits (easy to do when developing on Windows).
rpms:
	bash packages/build-rpms.sh

installer:
	cargo build --release --manifest-path $(CARGO_MANIFEST) \
		--target-dir build/cargo-target

# Needs libpam headers: pam-devel (EL) / libpam0g-dev (Debian).
controlplane:
	cargo build --release --manifest-path $(CP_MANIFEST) \
		--target-dir build/cargo-target-cp

webui:
	cd lumen-webui && npm ci && npm run build

iso:
	UPSTREAM_ISO="$(UPSTREAM_ISO)" UPSTREAM_SHA256="$(UPSTREAM_SHA256)" \
		bash iso/build-live-iso.sh

# Networking tests run entirely against the in-memory backend: no system bus,
# no NetworkManager, nothing touched on the machine running them.
test:
	cargo test --manifest-path $(CARGO_MANIFEST) \
		--target-dir build/cargo-target
	cargo test --manifest-path $(NET_MANIFEST) \
		--target-dir build/cargo-target-net
	cargo test --manifest-path $(CP_MANIFEST) \
		--target-dir build/cargo-target-cp

lint:
	shellcheck $(SCRIPTS)
	rpmlint $(SPECS)
	cargo fmt --manifest-path $(CARGO_MANIFEST) --check
	cargo clippy --manifest-path $(CARGO_MANIFEST) \
		--target-dir build/cargo-target -- -D warnings
	cargo fmt --manifest-path $(NET_MANIFEST) --check
	cargo clippy --manifest-path $(NET_MANIFEST) --all-targets \
		--target-dir build/cargo-target-net -- -D warnings
	cargo fmt --manifest-path $(CP_MANIFEST) --check
	cargo clippy --manifest-path $(CP_MANIFEST) --all-targets \
		--target-dir build/cargo-target-cp -- -D warnings

clean:
	rm -rf build dist
