# Building Lumen

## Overview

The Lumen ISO is a **custom live installer image** — no Anaconda, no
kickstart. It boots a minimal AlmaLinux 10 live environment that auto-starts
`lumen-installer` (Rust, GTK4, Quartz-styled) under `gnome-kiosk` on
Wayland, which installs the appliance onto a **ZFS boot drive**
(see [lumen-installer/README.md](../lumen-installer/README.md)).

Pipeline (`iso/build-live-iso.sh`, entry point `make iso`):

1. verify the upstream AlmaLinux 10 minimal ISO's SHA-256
2. extract its on-media `Minimal` repo (the offline install source) and
   **gate**: the repo's kernel must equal `KERNEL_NEVR` in `iso/pins.env`
3. build the Lumen RPMs; mirror the pinned OpenZFS EL10 kABI subset from
   `ZFS_REPO_URL`; `createrepo_c` the combined `lumen` repo
4. **gate**: the full target package set must resolve against *only* the
   two on-media repos (catches offline-completeness regressions)
5. `cargo build --release` the installer (AppStream rust, distro gtk4)
6. `lumen-installer/live/build-live.sh`: `dnf --installroot` live rootfs
   (pinned kernel + ZFS + kiosk + GTK4), **gate**: `modprobe --dry-run zfs`
   proves the kABI kmod resolves against the pinned kernel, chrooted
   `dracut` (dmsquash-live) initramfs, zstd squashfs
7. assemble a **UEFI-only** ISO: fresh ESP FAT image built with
   `mkfs.vfat` + `mtools` (shim + signed GRUB taken from the live rootfs
   RPMs), `xorrisofs` with the ESP as an appended GPT partition,
   `implantisomd5` (the "verify media" menu entry / `rd.live.check`)

Everything runs as root in an **unprivileged** `almalinux:10` container:
chroot and `mknod` only — no loop devices, no mounts, no `--privileged`.

## Prerequisites

```sh
dnf config-manager --set-enabled crb    # gtk4-devel lives in CRB
dnf install rpm-build rpmdevtools rpmlint createrepo_c xorriso mtools \
            dosfstools squashfs-tools isomd5sum kmod \
            rust cargo rustfmt clippy gtk4-devel make
```

- `xorriso`/`xorrisofs` assemble the ISO; `mtools` + `dosfstools` create
  and populate the ESP image without loop mounts
- `squashfs-tools` (mksquashfs) packs the live rootfs
- `kmod` provides `depmod` for the kABI gate
- `rust`/`cargo` + `gtk4-devel` build the installer (AppStream toolchain —
  deliberately not rustup, so the compiler comes from the same repo
  snapshot as the runtime libraries)
- `rpmlint` and `shellcheck` (EPEL 10) are optional, for `make lint`

## RPMs

```sh
make rpms       # -> dist/rpms/*.rpm (lumen-release, -logos, -networking)
```

`packages/build-rpms.sh` stages sources from `branding/` and
`lumen-networking/` into a scratch rpmbuild tree and injects the version
from `VERSION` via `--define "lumen_version ..."`.

`lumen-networking` ships `lumen-nicnames`: deterministic `nic0…nicN` names
(PCI order) via systemd `.link` files. The live environment runs it with
`--apply` before NetworkManager so the installer already shows the final
names; the installer runs it against the target so the installed system
matches. Re-run it after adding/replacing NICs — existing MAC pins are
never renumbered.

## ISO

```sh
make iso UPSTREAM_ISO=~/isos/AlmaLinux-10.2-x86_64-minimal.iso \
         UPSTREAM_SHA256=<sha256 from the official CHECKSUM file>
```

Alternatively put the hash in `<iso path>.sha256` and omit `UPSTREAM_SHA256`.
The build **fails loudly** on checksum mismatch, missing tools, or any of
the three gates above.

### Version pins — update together

- `iso/upstream.env` — upstream ISO URL + SHA-256 (from the official
  CHECKSUM file)
- `iso/pins.env` — `KERNEL_NEVR` (must equal the kernel in that ISO's
  Minimal repo; the gate prints the actual media kernel on mismatch, so a
  failed build tells you the correct value), `ZFS_REPO_URL`
  (**major-version path** `epel/10/kmod/`, server-side aliased to the
  point release OpenZFS currently targets; plain http — the host serves no
  https, and integrity comes from RPM signatures against the pinned key in
  `iso/keys/`), `ZFS_SERIES`

Moving to a new AlmaLinux point release means updating both files in one
commit — and checking that OpenZFS has published kABI kmods for that point
release first ([they can lag](https://github.com/openzfs/zfs/issues/17966);
the kABI gate fails the build if the aliased repo hasn't caught up).

### Installed system layout (UEFI-only, ZFS-only)

GPT: 1 GiB ESP + 2 GiB ext4 `/boot` + remainder ZFS pool `rpool`
(`ashift=12`, lz4, `xattr=sa`, `acltype=posixacl`), root dataset
`rpool/ROOT/lumen` (`bootfs`). Stock EL10 shim/GRUB2 from the RPMs (no
`grub2-install`); dracut's `zfs` module imports the pool
(`root=zfs:rpool/ROOT/lumen`); `/etc/hostid` is copied from the live env
(the pool creator) into the target and the pool is exported before reboot,
so the first import never needs force. `/etc/dracut.conf.d/zfs.conf` keeps
the zfs module in every future initramfs, and `/etc/kernel/cmdline` carries
the root argument for future kernel installs. First boot relabels for
SELinux (`.autorelabel`) — allow a few minutes.

**Secure Boot must be disabled**: zfs.ko is unsigned. The installer checks
firmware state and refuses with a clear message otherwise.

## CI

- **CI workflow**: shellcheck; RPMs + rpmlint (almalinux:10 container);
  installer job (cargo fmt/clippy/test against distro gtk4).
- **ISO workflow** (`workflow_dispatch`, or `workflow_call` from the
  release workflow on tag push): builds the ISO in an unprivileged
  container, then the **boot-smoke** job boots it on the runner host with
  QEMU/KVM + OVMF (hosted runners expose `/dev/kvm`) and waits for the
  `LUMEN-INSTALLER-READY` marker the installer writes to the serial
  console — an end-to-end proof that kernel → dracut → squashfs → logind →
  gnome-kiosk → GTK app all work. The serial log is uploaded as an
  artifact either way.
- The upstream ISO is cached keyed on `iso/upstream.env`.

## Notes, conventions, and known gaps

- **Architecture**: AlmaLinux 10 standard x86_64 targets x86-64-v3. An
  `x86_64_v2` variant can be added later if pre-Haswell hardware matters.
- **Verified against EL10 containers** (2026-07): rust 1.92 + gtk4 4.16
  (CRB), gnome-kiosk 49 + script session (started via
  `gnome-session --session=gnome-kiosk-script`), jetbrains-mono-fonts
  (EPEL 10), rpmlint zero-error, installer clippy/test clean.
- **First pipeline-run verifications still open**: chrooted dracut without
  /proc in the CI container, dnf4 flag syntax for `--repofrompath`/
  `download`/`repoquery --qf`, xorrisofs UEFI-only El Torito form, OpenZFS
  library subpackage names in the download glob.
- **OpenZFS RPM verification**: every mirrored RPM is checked with
  `rpmkeys --checksig` against `iso/keys/RPM-GPG-KEY-openzfs-2022`
  (extracted from the official `zfs-release-3-0.el10` package); the build
  fails on any unsigned or wrongly-signed package.
- `/etc/issue` has no ASCII art on purpose (agetty escape handling);
  `/etc/motd` carries the ANSI-color lockup. `/etc/os-release`, `issue`,
  `motd` are applied via `lumen-release` `%post` (files owned by
  `almalinux-release`/`setup`), so `rpm -V` flags them — expected.
- **rpmlint**: specs and RPMs pass with no errors; expected warnings are
  documented in the spec comments (`dangerous-command-in-%post`,
  `non-conffile-in-etc`, `no-documentation`).
