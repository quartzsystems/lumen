<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="branding/logos/png/lumen-lockup-dark-bg.png">
    <img src="branding/logos/png/lumen-lockup-light-bg.png" alt="Lumen — Quartz Systems" width="380">
  </picture>
</p>

**Lumen — a KVM hypervisor built to illuminate your infrastructure.**

Lumen is a KVM hypervisor platform by [Quartz Systems](https://quartz.systems/).
At this stage the repository contains the **appliance ISO with a custom
graphical installer** — a minimal AlmaLinux 10 system installed onto a **ZFS
boot drive** by `lumen-installer`, a Rust/GTK4 wizard with Quartz styling —
plus the management console, its networking, and **the first virtual machines
that boot**. Clustering, migration, and backups land later.

## Repository layout

```
VERSION              Single source of truth for the Lumen version
Makefile             make rpms | installer | controlplane | webui | iso | test | lint
lumen-installer/     Rust GUI installer (app/) + live environment (live/)
lumen-controlplane/  Rust (axum) control plane: auth API + web UI server on :8443
lumen-webui/         Next.js/TypeScript/Tailwind web console (login + shell)
lumen-networking/    lumen-net (bridges/bonds/VLANs) + nic0..nicN naming
lumen-storage/       lumen-zfs (pools, datasets, virtual machine volumes)
lumen-compute/       lumen-virt (the machine model, domain documents, lifecycle)
iso/                 ISO build pipeline + version pins (upstream.env, pins.env)
branding/            Release files, MOTD/issue, os-release additions, artwork
packages/            RPM specs (lumen-release/-logos/-networking/-storage/-compute)
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

## Networking

The console configures **bridges, bonds, VLAN interfaces, and per-adapter
settings** through NetworkManager over the system bus. The management
address lives on a bridge (`br0`) from the first boot, so the first
virtual machine needs no change to it. Every change is staged, validated,
and applied inside a **NetworkManager checkpoint that reverts itself** if
nobody confirms it within the window — a configuration that cuts your own
path to the node heals on its own instead of costing a trip to the rack.
See [docs/networking.md](docs/networking.md).

## Virtual machines & storage

The console **defines, starts, and removes virtual machines**, with their
disks created as zvols under each pool's `lumen` dataset and their adapters
attached to the node's bridges. libvirt is the source of truth — there is no
database, and Lumen's own per-machine data rides inside the domain document's
`<metadata>`, so `virsh dumpxml` shows the whole picture. Changes to a running
machine report **what reached the guest and what waits for a restart**, using
libvirt's own answer rather than a guess. A running machine's screen is on the
**Console** tab, or in a window of its own — the hypervisor's own stream,
carried over this console's connection and not interpreted on the way through.
See [docs/compute.md](docs/compute.md).

**Storage pools** are built and destroyed from the console too. The picker
reports what is already on every disk, so the one the appliance is running from
cannot be reformatted by accident, and a pool is built on the disk's stable
identifier rather than on a `/dev/sdX` name that moves between boots.

## The node itself

**System → Authentication** manages the node's local accounts, which are the
console's accounts: the `lumen` realm is PAM, so an account made here is an
account at the keyboard and over SSH. **System → Maintenance** restarts and
shuts the node down, now or at a scheduled moment held by logind rather than by
this console.

Creating an account and creating a pool are the only two operations that cannot
happen inside the management daemon's sandbox. Neither of them loosened it —
both are handed to **systemd**, which runs them as a transient unit outside it,
the same way networking asks NetworkManager and machines ask the hypervisor.
See [docs/system.md](docs/system.md).

## Updates

An installed appliance updates itself from `lumen.quartz.systems`, through
**System → Updates** in the console. Two decisions are kept apart there and
never joined into one button: ordinary updates, which can never move the
kernel, and the kernel with the storage modules built against its ABI, which
move as one set and only after the package manager has confirmed it can move
all of them together. Nothing is ever restarted by the update itself.

Packages and the repository index are both signed, and both are verified before
anything is installed. See [docs/updates.md](docs/updates.md).

## Prerequisites

Building is supported on **AlmaLinux 10 x86_64**; a container works — root
inside it, but no `--privileged`, loop devices, or mounts needed:

```sh
dnf config-manager --set-enabled crb
dnf install rpm-build rpmdevtools rpmlint createrepo_c xorriso mtools \
            dosfstools squashfs-tools isomd5sum kmod \
            rust cargo rustfmt clippy gtk4-devel pam-devel libvirt-devel \
            pkgconf-pkg-config nodejs npm systemd-rpm-macros make
```

`libvirt-devel` is needed to **link** the control plane against the
hypervisor's client library; nothing needs a hypervisor running, and no code
generator comes with it (see [docs/compute.md](docs/compute.md)).

Optional for linting shell scripts: `shellcheck` (EPEL).

## Building

### 1. RPMs

```sh
make rpms
```

Builds `lumen-release`, `lumen-logos`, `lumen-networking`, `lumen-storage`,
and `lumen-compute` (noarch) plus `lumen-controlplane` (x86_64: management
daemon + web UI export) into `dist/rpms/`. The version comes from the
top-level `VERSION` file.

### 2. ISO

Download the upstream AlmaLinux 10 x86_64 **minimal** ISO pinned in
`iso/upstream.env` (its on-media `Minimal` repo becomes the offline install
source), then:

```sh
make iso UPSTREAM_ISO=/path/to/AlmaLinux-10.2-x86_64-minimal.iso \
         UPSTREAM_SHA256=<sha256 from the official CHECKSUM file>
```

This builds the Rust installer, assembles a live installer environment
(squashfs + dracut), mirrors a pinned OpenZFS EL10 subset and the
virtualization stack the media does not carry, and produces a **UEFI-only**
`dist/lumen-<version>-x86_64.iso` (+ `.sha256`). Hard build gates keep the
kernel pin, the ZFS kABI kmod, and offline dependency resolution honest — see
[docs/build.md](docs/build.md).

### 3. Validation

```sh
make test    # installer, networking, storage, compute, control plane
make lint    # shellcheck, rpmlint, cargo fmt/clippy across five manifests
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
ZFS pool `boot` holding the OS root dataset. Everything else is fixed
appliance policy: minimal package set, SELinux enforcing (labeled at
install time), firewalld with SSH and the management console only,
chronyd enabled, hostname `lumen`. Log in as `root` with the password
chosen in the installer — on the console at **https://\<ip\>:8443**
(self-signed certificate; the built-in Lumen realm authenticates the
appliance's own accounts) or over SSH.

## Versioning

`VERSION` at the repo root is the single source of truth. It is consumed by
the RPM specs (via a macro the build defines), the installer build stamp,
and the ISO file name. Upstream/kernel/ZFS pins live in `iso/upstream.env`
and `iso/pins.env` and must move together. Tag releases as `v<version>`.

## License

See [LICENSE](LICENSE).
