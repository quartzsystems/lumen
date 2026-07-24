# Lumen build entry points — see README.md and docs/build.md.
#
# make rpms                          build lumen-release + lumen-logos
# make iso UPSTREAM_ISO=... \
#          [UPSTREAM_SHA256=...]     build dist/lumen-<version>-x86_64.iso
# make lint                          shellcheck + rpmlint
# make ks-validate                   validate the kickstart (RHEL10 profile)
# make clean                         remove build/ and dist/

VERSION := $(shell cat VERSION)

SCRIPTS := packages/build-rpms.sh iso/build-iso.sh
SPECS   := packages/lumen-release.spec packages/lumen-logos.spec

.PHONY: all rpms iso lint ks-validate clean

all: rpms

# Scripts are invoked via bash so builds work even if a checkout loses the
# executable bits (easy to do when developing on Windows).
rpms:
	bash packages/build-rpms.sh

iso:
	UPSTREAM_ISO="$(UPSTREAM_ISO)" UPSTREAM_SHA256="$(UPSTREAM_SHA256)" \
		bash iso/build-iso.sh

ks-validate:
	ksvalidator -v RHEL10 iso/lumen.ks.in

lint:
	shellcheck $(SCRIPTS)
	rpmlint $(SPECS)

clean:
	rm -rf build dist
