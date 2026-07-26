# lumen-controlplane — the Lumen management daemon (web UI + auth API).
#
# The binary and the web UI export are built by packages/build-rpms.sh
# BEFORE rpmbuild runs (cargo needs the network for crates.io, npm for the
# registry — neither belongs inside the chroot-less rpmbuild step) and are
# handed in as plain sources. That matches how the other Lumen packages
# stage prebuilt content, and keeps the spec free of toolchain
# BuildRequires the EL10 media couldn't satisfy offline.
#
# The version is injected by the build tooling from the top-level VERSION
# file; see lumen-release.spec for details.
%{!?lumen_version:%global lumen_version 0.1.0}

# Sources are prebuilt (cargo already stripped the binary); no debuginfo to
# extract.
%global debug_package %{nil}

Name:           lumen-controlplane
Version:        %{lumen_version}
Release:        1%{?dist}
Summary:        Quartz Systems Lumen management daemon and web console
License:        MIT
URL:            https://quartz.systems/

Source0:        lumen-controlplane
Source1:        lumen-webui.tar.gz
Source2:        lumen-controlplane.pam
Source3:        lumen-controlplane.service
Source4:        lumen-controlplane.xml
Source5:        lumen-controlplane.pp

Requires:       pam
# systemd is not only what starts this daemon. Two operations cannot happen
# inside its sandbox — creating a local account, and creating or removing a
# storage pool — so they are handed to systemd, which runs them as a unit of
# its own outside that sandbox. See docs/system.md.
Requires:       systemd
# The account tools those two paths hand over: useradd, usermod, userdel, and
# chpasswd. Present on every install, and named because the daemon runs them
# by absolute path.
Requires:       shadow-utils
# Loading the module below needs semodule, and the module is what makes the
# delegation above work at all under Enforcing.
Requires:       policycoreutils
Requires(post): policycoreutils
Requires(postun): policycoreutils
%{?systemd_requires}
BuildRequires:  systemd-rpm-macros

# NOTE: description wording avoids tokens the EL10 rpmlint spelling check
# flags as errors (zero-error policy) — no protocol or subsystem acronyms,
# and no compound coinages ("prebuilt" is an error there).
%description
Management daemon for Quartz Systems Lumen. Serves the management console
and its programming interface on a single secured port. Sign-in is
verified against the appliance's own local accounts, sessions are browser
cookies that page scripts cannot read, and the console itself is static
content built in advance and served by the daemon, so the appliance needs
no extra runtime. A certificate is created on first start and can be
replaced with operator-provided files.

%prep
# No source archive to unpack; the web console export is unpacked in install.

%build
# Nothing to compile; the daemon binary arrives prebuilt as Source0.

%install
install -D -p -m 0755 %{SOURCE0} %{buildroot}%{_sbindir}/lumen-controlplane
mkdir -p %{buildroot}%{_datadir}/lumen-webui
tar -xzf %{SOURCE1} -C %{buildroot}%{_datadir}/lumen-webui
# The web console export repeats a lot of bytes: every route's index.txt is
# identical to its __next._full.txt, and the per-segment navigation payloads
# are shared verbatim across routes. That is inherent to the export format,
# and it grows with the navigation tree — past a handful of routes it is
# enough waste for rpmlint to fail the build (files-duplicated-waste).
# Collapse the identical files into hardlinks: same bytes served, one copy
# on disk and in the payload.
(
    cd %{buildroot}%{_datadir}/lumen-webui
    prev=
    keep=
    find . -type f -exec sha256sum {} + | sort | while read -r sum path; do
        if [ "$sum" = "$prev" ]; then
            ln -f "$keep" "$path"
        else
            prev=$sum
            keep=$path
        fi
    done
)
install -D -p -m 0644 %{SOURCE2} %{buildroot}%{_sysconfdir}/pam.d/lumen-controlplane
install -D -p -m 0644 %{SOURCE3} %{buildroot}%{_unitdir}/lumen-controlplane.service
install -D -p -m 0644 %{SOURCE4} %{buildroot}%{_prefix}/lib/firewalld/services/lumen-controlplane.xml
install -D -p -m 0644 %{SOURCE5} %{buildroot}%{_datadir}/selinux/packages/%{name}/lumen-controlplane.pp
install -d -m 0700 %{buildroot}%{_sharedstatedir}/lumen-controlplane

%post
%systemd_post lumen-controlplane.service
# Priority 200: above the base policy at 100, which is the range reserved for
# modules a package ships. Run on upgrade as well as on install, so a corrected
# rule reaches a node that already carries the old one.
#
# No "-n" and no separate load_policy: semodule reloads the policy itself, and
# the two-step form would need selinuxenabled from libselinux-utils — a fourth
# package to depend on, and a silent no-op if it were ever missing.
#
# Deliberately not suffixed with "|| :". If this module does not load, three
# separate features fail later with nothing pointing here; a loud scriptlet
# warning at install time is the cheapest place to find that out.
#
# This scriptlet is NOT how the appliance gets the module, and must not be
# relied on as though it were. The installer builds the target with
# `dnf --installroot` from a live environment booted `selinux=0`, where
# semodule has no target store to write and nothing to load into — and RPM
# reports a failed %post as a warning that dnf installs straight past. So the
# appliance would come up enforcing with the module absent and the install
# reporting success. `lumen-installer`'s plan loads it into the chroot itself
# and fails the install if it did not land; see its "Load the security policy
# module" step. What remains here is the other path: installing or upgrading
# this package on a node that is already running, where %post works properly.
%{_sbindir}/semodule -X 200 -i \
    %{_datadir}/selinux/packages/%{name}/lumen-controlplane.pp

%preun
%systemd_preun lumen-controlplane.service

%postun
%systemd_postun_with_restart lumen-controlplane.service
# Only on the last removal. An upgrade runs postun for the old package *after*
# post for the new one, so removing unconditionally here would unload the
# module that was just installed.
if [ "$1" -eq 0 ]; then
    %{_sbindir}/semodule -X 200 -r lumen-controlplane || :
fi

%files
%{_sbindir}/lumen-controlplane
%{_datadir}/lumen-webui/
%config(noreplace) %{_sysconfdir}/pam.d/lumen-controlplane
%{_unitdir}/lumen-controlplane.service
%{_prefix}/lib/firewalld/services/lumen-controlplane.xml
%{_datadir}/selinux/packages/%{name}/
%dir %attr(0700,root,root) %{_sharedstatedir}/lumen-controlplane

%changelog
* Fri Jul 24 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.3.0-1
- Add guest machine management: define, start, stop, and remove machines,
  with their disks created on the node's own pools and their adapters
  attached to its bridges
- Add the console views for machines and for the node's pools, and a second
  navigation column for whichever machine is open
- Report which settings a running machine accepted and which wait for it to
  restart, using the answer the guest service itself gave
- Add the security policy module the daemon needs to hand a command to the
  system service manager while the security system is enforcing; without it
  creating a pool, creating an account, and the restart controls all fail

* Fri Jul 24 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.2.0-1
- Add the network configuration interface: link aggregation, virtual
  networks, tagged interfaces, and per-adapter settings, staged and applied
  with an automatic revert if the operator does not confirm in time
- Add the console views for adapters and for the network summary

* Fri Jul 24 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.1.0-1
- Initial lumen-controlplane package: management daemon, web console
  export, PAM service, systemd unit, firewalld service definition
