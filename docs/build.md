# Building Lumen

## Prerequisites

Build host: **AlmaLinux 10 x86_64** (bare metal, VM, or container — the ISO
step is best done on a host or privileged container, see
[CI limitations](#ci-limitations)).

```sh
dnf install rpm-build rpmdevtools rpmlint createrepo_c pykickstart \
            xorriso mtools isomd5sum cpio make
```

- `pykickstart` provides `ksvalidator`
- `xorriso` (also provides `xorrisofs`) does the ISO remaster
- `mtools` edits the EFI boot image without loop mounts — the entire ISO
  build runs unprivileged
- `cpio` (+ `gzip`) packs the Anaconda branding overlay `images/product.img`
- `isomd5sum` provides `implantisomd5`/`checkisomd5`
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
required tool (`xorriso`, `createrepo_c`, `rpmbuild`, `ksvalidator`, `cpio`,
...) is missing.

The script then:

- builds the RPMs and creates a `lumen` repo directory (`createrepo_c`),
- renders `iso/lumen.ks.in` (stamps `@LUMEN_VERSION@` / `@LUMEN_BUILD_DATE@`
  into `%post`) and validates it with `ksvalidator -v RHEL10`,
- extracts the ISO tree, rewrites the grub configs (volume label `LUMEN`,
  `inst.ks=`, `inst.profile=lumen`), re-brands the grub config inside the
  appended EFI boot partition via `mtools`, and rebuilds with `xorrisofs`
  using the same boot recipe as the upstream media (verified against
  `xorriso -report_el_torito as_mkisofs`),
- packs the Anaconda branding overlay `images/product.img` (gzipped cpio
  with the Lumen profile, stylesheet, and sidebar logo from
  `branding/anaconda/`; Anaconda's dracut module unpacks it over the
  installer runtime automatically),
- implants the media checksum (`implantisomd5`) and writes
  `dist/lumen-<version>-x86_64.iso` plus a `.sha256` file.

### Why not mkksiso?

lorax 40.x's `mkksiso` fails on AlmaLinux/RHEL 10 media with
`xorriso : SORRY : Cannot enable EL Torito boot image #2 because it is not
a data file in the ISO filesystem`: EL10 ISOs keep the EFI boot image only
in an appended GPT partition, which mkksiso's `-boot_image any replay`
invocation cannot re-enable. It also requires root for loop mounts
(`mkefiboot`). The manual remaster in `iso/build-iso.sh` avoids both
problems and verifies the result (both El Torito entries, volid,
`checkisomd5`). Revisit if a fixed lorax lands in EL10.

### The installer

The ISO boots the branded **graphical Anaconda installer** (the upstream
minimal ISO's stage2 already contains the GUI, so no lorax rebuild is
needed). The kickstart is deliberately partial: it preseeds everything
that is appliance policy and leaves exactly four decisions to the
operator —

- **root password** (mandatory — `rootpw` is omitted from the kickstart),
- **installation destination** (mandatory — pick the boot drive; automatic
  LVM partitioning is the preselected scheme),
- **network** (pick the management NIC, DHCP or static, in the
  Network & Host Name spoke; hostname defaults to `lumen`),
- **time zone** (prefilled `Etc/UTC`, changeable).

The user-creation and software-selection spokes are hidden by the Anaconda
profile (`branding/anaconda/lumen-profile.conf`): the appliance is
root-only at install time and the package set (`@core` + `lumen-release`,
no GUI) is fixed, with SELinux enforcing, firewalld allowing SSH only, and
`chronyd`/`sshd` enabled.

### First boot

Log in as `root` with the password chosen in the installer (console or
SSH — the firewall allows SSH by default). There is no baked-in default
password; images built before the interactive installer landed used
`root` / `lumen`, pre-expired.

## CI

The GitHub Actions release workflow (tag push `v*`) builds the RPMs and the
ISO in unprivileged `almalinux:10` containers and attaches everything to
the release. Because the remaster needs no loop mounts (see above), no
`--privileged` container options are required. The upstream AlmaLinux ISO
is pinned by URL + SHA-256 in `iso/upstream.env` and cached between runs
with `actions/cache`; update that file (both values together, from the
official CHECKSUM) to move to a new upstream point release.

An ISO build can also be kicked off manually: Actions tab → **ISO** →
*Run workflow* (`workflow_dispatch` on `.github/workflows/iso.yml`). Manual
runs upload the ISO + `.sha256` as a workflow artifact named `lumen-iso`
(14-day retention) instead of attaching to a release. The release workflow
reuses the same job via `workflow_call`, so the two paths cannot drift.

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
