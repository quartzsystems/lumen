<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="branding/logos/png/lumen-lockup-dark-bg.png">
    <img src="branding/logos/png/lumen-lockup-light-bg.png" alt="Lumen — Quartz Systems" width="380">
  </picture>
</p>

**Lumen — a KVM hypervisor built to illuminate your infrastructure.**

Lumen is a KVM hypervisor platform by [Quartz Systems](https://www.quartzsystems.net).
At this stage the repository contains the **appliance ISO with a custom
graphical installer**: a minimal AlmaLinux 10 system installed onto a **ZFS
boot drive** by `lumen-installer`, a Rust/GTK4 wizard with Quartz styling.
Hypervisor components (libvirt, QEMU, orchestration) land later.

## Repository layout

```
VERSION              Single source of truth for the Lumen version
Makefile             make rpms | installer | controlplane | webui | iso | test | lint
lumen-installer/     Rust GUI installer (app/) + live environment (live/)
lumen-controlplane/  Rust (axum) control plane: auth API + web UI server on :8443
lumen-webui/         Next.js/TypeScript/Tailwind web console (login page)
lumen-networking/    lumen-nicnames: deterministic nic0..nicN naming
iso/                 ISO build pipeline + version pins (upstream.env, pins.env)
branding/            Release files, MOTD/issue, os-release additions, artwork
packages/            RPM specs (lumen-release, lumen-logos, lumen-networking)
docs/                Build and design documentation
.github/             CI (RPMs, Rust, web UI, ISO + QEMU boot smoke test)
```

## Control plane & web UI

`lumen-controlplane` (Rust/axum) serves the management surface on
**https://\<host\>:8443**: the REST auth API plus the static `lumen-webui`
export (Next.js + Tailwind, Quartz design system) — no Node.js on the
appliance. Sign-in goes through pluggable **realms**; the built-in
`lumen` realm is the OS's own authentication (PAM → the accounts created
by the installer, i.e. root today). See
[docs/controlplane.md](docs/controlplane.md) for the architecture and the
development workflow.

## Prerequisites

Building is supported on **AlmaLinux 10 x86_64**; a container works — root
inside it, but no `--privileged`, loop devices, or mounts needed:

```sh
dnf config-manager --set-enabled crb
dnf install rpm-build rpmdevtools rpmlint createrepo_c xorriso mtools \
            dosfstools squashfs-tools isomd5sum kmod \
            rust cargo rustfmt clippy gtk4-devel make
```

Optional for linting shell scripts: `shellcheck` (EPEL).

## Building

### 1. RPMs

```sh
make rpms
```

Builds `lumen-release`, `lumen-logos`, and `lumen-networking` as noarch
RPMs into `dist/rpms/`. The version comes from the top-level `VERSION` file.

### 2. ISO

Download the upstream AlmaLinux 10 x86_64 **minimal** ISO pinned in
`iso/upstream.env` (its on-media `Minimal` repo becomes the offline install
source), then:

```sh
make iso UPSTREAM_ISO=/path/to/AlmaLinux-10.2-x86_64-minimal.iso \
         UPSTREAM_SHA256=<sha256 from the official CHECKSUM file>
```

This builds the Rust installer, assembles a live installer environment
(squashfs + dracut), mirrors a pinned OpenZFS EL10 subset, and produces a
**UEFI-only** `dist/lumen-<version>-x86_64.iso` (+ `.sha256`). Hard build
gates keep the kernel pin, the ZFS kABI kmod, and offline dependency
resolution honest — see [docs/build.md](docs/build.md).

### 3. Validation

```sh
make test    # installer engine unit tests (headless)
make lint    # shellcheck, rpmlint, cargo fmt/clippy
```

## Installing the appliance

Boot the ISO (UEFI, **Secure Boot disabled** — zfs.ko is unsigned; the
media has **no BIOS boot path**, so VMs must use EFI firmware — in VMware
Workstation: VM Settings → Options → Advanced → Firmware type → UEFI. If
that option is greyed out, power the VM off fully and set the guest OS
type to RHEL 9/10 64-bit, or add `firmware = "efi"` to the .vmx); the
Quartz-styled installer asks exactly four questions: root password, time
zone, management NIC (DHCP or static), and the boot drive. NICs are named
`nic0…nicN` (PCI order) in the installer and identically on the installed
system. The chosen drive is erased: EFI system partition + ext4 `/boot` +
ZFS pool `rpool` holding the OS root dataset. Everything else is fixed
appliance policy: minimal package set, SELinux enforcing (labeled at
install time), firewalld with SSH only, chronyd enabled, hostname `lumen`.
Log in as `root` with the password chosen in the installer.

## Versioning

`VERSION` at the repo root is the single source of truth. It is consumed by
the RPM specs (via a macro the build defines), the installer build stamp,
and the ISO file name. Upstream/kernel/ZFS pins live in `iso/upstream.env`
and `iso/pins.env` and must move together. Tag releases as `v<version>`.

## License

See [LICENSE](LICENSE).
