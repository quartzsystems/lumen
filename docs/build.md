# Building Lumen

## Prerequisites

Build host: **AlmaLinux 10 x86_64** (bare metal, VM, or container — the ISO
step is best done on a host or privileged container, see
[CI limitations](#ci-limitations)).

```sh
dnf install rpm-build rpmdevtools rpmlint createrepo_c lorax pykickstart \
            mtools make
```

- `lorax` provides `mkksiso`
- `pykickstart` provides `ksvalidator`
- `mtools` lets `mkksiso` update the EFI boot image without loop mounts
- `rpmlint` and `shellcheck` (both from **EPEL 10**: `dnf install
  epel-release` first) are optional, for `make lint`

## RPMs

```sh
make rpms       # -> dist/rpms/*.rpm (noarch + src)
```

`packages/build-rpms.sh` stages sources from `branding/` into a scratch
rpmbuild tree under `build/rpmbuild/` and injects the version from the
top-level `VERSION` file via `--define "lumen_version ..."`. Both specs keep
a fallback default so a bare `rpmbuild -bb packages/<spec>` (or a mock
build) also works; `VERSION` remains the source of truth for releases.

Mock example:

```sh
mock -r almalinux-10-x86_64 --spec packages/lumen-release.spec \
     --sources build/rpmbuild/SOURCES --define "lumen_version $(cat VERSION)"
```

## ISO

1. Download the upstream AlmaLinux 10 x86_64 **minimal** ISO and its official
   `CHECKSUM` file from <https://almalinux.org/get-almalinux/>.
2. Build:

```sh
make iso UPSTREAM_ISO=~/isos/AlmaLinux-10-latest-x86_64-minimal.iso \
         UPSTREAM_SHA256=<sha256 from the official CHECKSUM file>
```

Alternatively put the hash in `<iso path>.sha256` and omit `UPSTREAM_SHA256`.
The build **fails loudly** if the checksum is absent or mismatched, or if any
required tool (`mkksiso`, `createrepo_c`, `rpmbuild`, `ksvalidator`) is
missing.

The script then:

- builds the RPMs and creates a `lumen` repo directory (`createrepo_c`),
- renders `iso/lumen.ks.in` (stamps `@LUMEN_VERSION@` / `@LUMEN_BUILD_DATE@`
  into `%post`) and validates it with `ksvalidator -v RHEL10`,
- runs `mkksiso --volid LUMEN --ks ... --add lumen/` to produce
  `dist/lumen-<version>-x86_64.iso` plus a `.sha256` file.

The kickstart installs `@core` + `lumen-release` only (no GUI), with SELinux
enforcing, firewalld allowing SSH only, `chronyd`/`sshd` enabled, and static
hostname `lumen`.

### First boot

The root password is `lumen` and is **pre-expired** (`chage -d 0 root` in
`%post`): the first console or SSH login forces a password change. Change
this policy in `iso/lumen.ks.in` if the appliance will be provisioned
differently.

## CI limitations

The GitHub Actions release workflow builds and publishes the **RPMs only**
(in an `almalinux:10` container). The ISO is not built in CI: when `mkksiso`
changes the volume label it must regenerate boot configs inside the EFI boot
image, which on some lorax versions falls back to `mkefiboot`/loop mounts —
unavailable in unprivileged GitHub-hosted runner containers. Build the ISO
locally with `make iso`; the workflow's release notes point this out.

## Notes and conventions

- **Architecture**: AlmaLinux 10 standard x86_64 targets the x86-64-v3
  microarchitecture level. AlmaLinux also publishes an `x86_64_v2` variant of
  AlmaLinux 10 — if Lumen ever needs to support older CPUs (pre-Haswell era),
  an `x86_64_v2` ISO/RPM variant can be added; for now Lumen targets standard
  x86_64 (v3) only.
- **`/etc/issue` has no ASCII art** on purpose: agetty interprets
  backslash escapes (`\r`, `\m`, ...) in `/etc/issue`, which would mangle the
  ASCII mark. The art lives in `/etc/motd` (printed verbatim);
  `/etc/issue` carries the plain-text brand line.
- **`/etc/motd` uses ANSI color**: it renders the full-color Lumen lockup
  (256-color ANSI art supplied by branding). The template
  (`branding/release/motd.in`) keeps a readable `@ESC@` token in place of
  the escape byte; the spec substitutes the real byte at build time. The
  art is ~120 columns wide, so it wraps on consoles narrower than that,
  and on a terminal without color support the escape codes may show
  literally — acceptable for an appliance whose consoles are modern
  TTYs/SSH clients.
- **File ownership**: `/etc/os-release`, `/etc/issue` and `/etc/motd` are
  owned by `almalinux-release`/`setup`, so `lumen-release` ships its content
  under `/usr/share/lumen-release/` and applies it in `%post` (and restores
  stock content on erase). Consequence: `rpm -V almalinux-release setup`
  reports those files as modified — expected on a branded appliance.
- **rpmlint** (2.8.0, EL10): both specs and the built RPMs pass with **no
  errors**. Expected warnings:
  - `dangerous-command-in-%post` / `%postun` (`mv`, `ln`) — deliberate, see
    file-ownership note above;
  - `non-conffile-in-etc /etc/lumen-release` — intentional: the file is
    version data, not admin-editable config;
  - `no-documentation` / `no-%check-section` — nothing to ship or test yet;
  - possible `incoherent-version-in-changelog` when building a version other
    than the changelog's latest entry (the version is macro-injected).
- **Kickstart `%post` stamp**: the appended `build <version> (<date>)` line
  makes `rpm -V lumen-release` flag `/etc/lumen-release` as modified;
  accepted for appliance images.
