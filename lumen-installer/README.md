# lumen-installer

Custom graphical installer for the Lumen appliance. Replaces Anaconda so
Lumen can install onto a **ZFS boot drive** (which Anaconda/blivet cannot
do), with a deliberately small decision surface: root password, time zone,
management NIC (DHCP/static), target disk.

## Architecture

```
app/        Rust binary (gtk4-rs). GTK UI + install engine.
  src/ui.rs         wizard (GtkStack), Quartz-dark theme (src/theme.css)
  src/engine/       plan builder + runner; GTK-free, unit-tested
  src/sysinfo.rs    NIC/disk/timezone/firmware probing; GTK-free
live/       live installer environment (squashfs) build
  packages.txt      dnf --installroot package set
  overlay/          files copied over the live rootfs (systemd units)
  build-live.sh     installroot -> dracut initramfs -> squashfs
```

- The ISO boots a minimal AlmaLinux 10 live environment (dmsquash-live)
  that auto-starts `gnome-kiosk` (Wayland) as a logind session on tty1
  (`PAMName=login` pattern, no display manager) running `lumen-installer`
  full-screen. tty2 has a root autologin getty as a debug hatch.
- `lumen-nicnames --apply` (from lumen-networking) runs before
  NetworkManager, so the installer and the installed system see identical
  `nic0…nicN` names, ordered by PCI address.
- The engine is a data-first plan (commands + file contents). Inspect it
  without touching hardware:

  ```sh
  lumen-installer --print-plan config.json
  ```

  where `config.json` matches `config::InstallConfig` (see the unit tests
  for an example).

## Installed layout (UEFI-only, ZFS-only)

| Partition | Size    | Contents                                  |
|-----------|---------|-------------------------------------------|
| 1 (ESP)   | 1 GiB   | shim + GRUB2 (RPM-provided, no grub2-install) |
| 2         | 2 GiB   | ext4 `/boot` (kernels + initramfs)        |
| 3         | rest    | ZFS pool `rpool`, root dataset `rpool/ROOT/lumen` |

dracut's `zfs` module imports rpool at boot (`root=zfs:rpool/ROOT/lumen`);
`/etc/hostid` is copied from the live env (pool creator) into the target so
the import never needs force. The pool is cleanly exported before reboot.
Secure Boot must be **disabled** (zfs.ko is unsigned); the installer checks
and refuses with a clear message otherwise.

## Offline install

Packages come from the ISO only: the upstream AlmaLinux `Minimal` repo
(copied on-media) plus the `lumen` repo (our RPMs + a pinned OpenZFS EL10
kmod subset). The kernel NEVR is pinned in `iso/pins.env` and stamped into
the live image (`/etc/lumen-build.env`) so live kernel == target kernel ==
kmod-zfs kABI target; CI enforces all three (see `iso/build-live-iso.sh`).

## Hacking

```sh
cd lumen-installer/app
cargo fmt && cargo clippy && cargo test   # engine/sysinfo tests are headless
cargo run -- --print-plan sample.json     # dry-run the engine plan
```

The GUI needs GTK4 ≥ 4.12 and runs anywhere (it only *probes* the system
until you confirm the summary page — but don't click "Erase & Install" on
your workstation).
