# Secure Boot support — scoped plan (next work item)

## Status

Planned, not started. Today the installer refuses to run with Secure Boot
enabled and the docs say to disable it.

## Why it's one problem, not many

Shim, GRUB2, and the kernel on Lumen media and installed systems are the
stock AlmaLinux-signed binaries — they already boot fine under Secure
Boot. The only unsigned artifact in the chain is `zfs.ko` (OpenZFS ships
unsigned kABI kmods), which kernel lockdown refuses to load. So Secure
Boot support == getting one module-signing key trusted.

## Design: Lumen MOK (Machine Owner Key)

1. **Key generation (one-time, offline).** X.509 module-signing keypair
   ("Lumen Secure Boot MOK"). Private key becomes a CI secret (GitHub
   Actions secret for now; hardware-backed signing later if warranted).
   Public cert is committed to the repo (`iso/keys/lumen-mok.der`).
2. **Sign at mirror time (CI).** In `build-live-iso.sh`, after the
   OpenZFS signature gate: unpack `kmod-zfs`, run the kernel's
   `scripts/sign-file sha256 <key> <cert>` over every `.ko`, repackage as
   `kmod-zfs-signed` (Release suffix `.lumen`, Provides/Obsoletes the
   upstream name) into the `lumen` repo. Gate: `modinfo -F sig_key` on
   every module must show the Lumen key.
3. **Installer enrolls the key.** Engine copies the cert into the target
   and runs `mokutil --import` with a generated one-time password shown
   on the Done page. On first reboot, shim's MokManager prompts once;
   the operator confirms ("Enroll MOK" → password). After that, every
   Lumen-signed kmod update loads without interaction.
4. **Installer UX.** The Secure Boot pre-flight check changes from a
   blocker to an informational path: SB off → install as today; SB on →
   proceed, and the Done page walks through the one-time MokManager
   confirmation. `mokutil --sb-state` equivalent read stays as-is
   (efivars).
5. **Kernel updates** need nothing: the kernel is Alma-signed, and any
   future kmod builds flow through the same CI signing step.

## Work items

1. `iso/keys/lumen-mok.der` + CI secret; document key rotation.
2. `build-live-iso.sh`: sign-and-repack step + `modinfo -F sig_key` gate
   (needs `kernel-devel` for sign-file in the build container, or the
   standalone `sign-file` from `kernel-devel` extracted at pin time).
3. Engine: `mokutil --import` step + cert install; surface the one-time
   password and MokManager instructions on the Done page.
4. UI: welcome-page SB check becomes informational; Done-page
   instructions conditional on SB state.
5. Live env: decide whether the *live* session must also load zfs under
   SB (it must — the installer creates the pool). MokManager enrollment
   can't happen before the live boot, so the live ISO needs one of:
   (a) document "first boot of the ISO under SB requires enrolling the
   Lumen MOK from the boot menu" via a shipped `mmx64.efi` flow, or
   (b) simpler v1: installer runs with SB on only after the key was
   enrolled in a prior boot; first-time SB installs still need one
   SB-off boot. Decide during implementation; (a) is the real goal.
6. Docs: replace "disable Secure Boot" with the enrollment flow;
   boot-smoke CI variant with OVMF SB vars enrolling the MOK.

## Acceptance

- ISO boots with SB enabled after MOK enrollment; installer creates
  boot (zfs.ko loads under lockdown).
- Installed system boots with SB enabled end-to-end; `mokutil
  --test-key` confirms enrollment; kmod update via dnf still loads.
- SB-off path unchanged.

## Out of scope

Custom shim (Microsoft signing process), signing the kernel ourselves,
and DKMS-based approaches (incompatible with lockdown's trust model).
