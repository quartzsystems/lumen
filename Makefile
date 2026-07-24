# Lumen build entry points — see README.md and docs/build.md.
#
# make rpms                          build lumen-release/-logos/-networking
# make installer                     cargo build the Rust installer (release)
# make iso UPSTREAM_ISO=... \
#          [UPSTREAM_SHA256=...]     build dist/lumen-<version>-x86_64.iso
# make test                          installer unit tests (headless)
# make lint                          shellcheck + rpmlint + fmt/clippy
# make clean                         remove build/ and dist/

VERSION := $(shell cat VERSION)

SCRIPTS := packages/build-rpms.sh iso/build-live-iso.sh \
           lumen-installer/live/build-live.sh \
           lumen-networking/nicnames/lumen-nicnames
SPECS   := packages/lumen-release.spec packages/lumen-logos.spec \
           packages/lumen-networking.spec
CARGO_MANIFEST := lumen-installer/app/Cargo.toml

.PHONY: all rpms installer iso test lint clean

all: rpms

# Scripts are invoked via bash so builds work even if a checkout loses the
# executable bits (easy to do when developing on Windows).
rpms:
	bash packages/build-rpms.sh

installer:
	cargo build --release --manifest-path $(CARGO_MANIFEST) \
		--target-dir build/cargo-target

iso:
	UPSTREAM_ISO="$(UPSTREAM_ISO)" UPSTREAM_SHA256="$(UPSTREAM_SHA256)" \
		bash iso/build-live-iso.sh

test:
	cargo test --manifest-path $(CARGO_MANIFEST) \
		--target-dir build/cargo-target

lint:
	shellcheck $(SCRIPTS)
	rpmlint $(SPECS)
	cargo fmt --manifest-path $(CARGO_MANIFEST) --check
	cargo clippy --manifest-path $(CARGO_MANIFEST) \
		--target-dir build/cargo-target -- -D warnings

clean:
	rm -rf build dist
