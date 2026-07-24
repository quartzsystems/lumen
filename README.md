<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="branding/logos/png/lumen-lockup-dark-bg.png">
    <img src="branding/logos/png/lumen-lockup-light-bg.png" alt="Lumen — Quartz Systems" width="380">
  </picture>
</p>

**Light-weight KVM orchestration for AlmaLinux.**

Lumen is a KVM hypervisor platform by [Quartz Systems](https://www.quartzsystems.net).
At this stage the repository contains the **appliance ISO build system and
branding base**: a minimal AlmaLinux 10 install with Quartz Systems / Lumen
branding. Hypervisor components (libvirt, QEMU, orchestration) land later.

## Repository layout

```
VERSION            Single source of truth for the Lumen version
Makefile           make rpms | make iso | make lint | make clean
iso/               Kickstart + ISO build tooling (mkksiso)
branding/          Release files, MOTD/issue, os-release additions, artwork
packages/          RPM spec files (lumen-release, lumen-logos)
docs/              Build and design documentation
.github/           CI (RPM build + release on tag push)
```

## Prerequisites

Building is supported on **AlmaLinux 10 x86_64** (a container is fine for the
RPMs; the ISO build needs the tools below on the host or a privileged
container):

```sh
dnf install rpm-build rpmdevtools rpmlint createrepo_c lorax pykickstart make
```

Optional for linting shell scripts: `shellcheck` (EPEL).

## Building

### 1. RPMs

```sh
make rpms
```

Builds `lumen-release` and `lumen-logos` as noarch RPMs into `dist/rpms/`.
The version is read from the top-level `VERSION` file.

### 2. ISO

Download the upstream AlmaLinux 10 x86_64 **minimal** ISO and its official
`CHECKSUM` file from <https://almalinux.org/get-almalinux/>, then:

```sh
make iso UPSTREAM_ISO=/path/to/AlmaLinux-10-latest-x86_64-minimal.iso \
         UPSTREAM_SHA256=<sha256 from the official CHECKSUM file>
```

This verifies the upstream ISO checksum, builds the Lumen RPMs, embeds the
kickstart and a local `lumen` package repo into the ISO with `mkksiso`, and
produces `dist/lumen-<version>-x86_64.iso` (volume label `LUMEN`) plus a
`.sha256` file.

Instead of passing `UPSTREAM_SHA256` on the command line you can place the
expected checksum in a file next to the ISO named `<iso>.sha256`.

### 3. Validation

```sh
make lint          # rpmlint on specs, shellcheck on scripts
make ks-validate   # ksvalidator against the RHEL10 profile
```

## Installing the appliance

Boot the generated ISO; the embedded kickstart performs a fully automated
minimal install (SELinux enforcing, firewalld with SSH only, chronyd enabled,
hostname `lumen`). The default root password is `lumen` and **must be changed
at first login** (it is pre-expired). See [docs/build.md](docs/build.md).

## Versioning

`VERSION` at the repo root is the single source of truth. It is consumed by
the RPM specs (via a macro the Makefile defines), the kickstart `%post`
version stamp, and the ISO file name. Tag releases as `v<version>`.

## License

See [LICENSE](LICENSE).
