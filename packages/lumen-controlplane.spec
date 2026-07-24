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
URL:            https://www.quartzsystems.net

Source0:        lumen-controlplane
Source1:        lumen-webui.tar.gz
Source2:        lumen-controlplane.pam
Source3:        lumen-controlplane.service
Source4:        lumen-controlplane.xml

Requires:       pam
Requires:       systemd
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
install -d -m 0700 %{buildroot}%{_sharedstatedir}/lumen-controlplane

%post
%systemd_post lumen-controlplane.service

%preun
%systemd_preun lumen-controlplane.service

%postun
%systemd_postun_with_restart lumen-controlplane.service

%files
%{_sbindir}/lumen-controlplane
%{_datadir}/lumen-webui/
%config(noreplace) %{_sysconfdir}/pam.d/lumen-controlplane
%{_unitdir}/lumen-controlplane.service
%{_prefix}/lib/firewalld/services/lumen-controlplane.xml
%dir %attr(0700,root,root) %{_sharedstatedir}/lumen-controlplane

%changelog
* Fri Jul 24 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.2.0-1
- Add the network configuration interface: link aggregation, virtual
  networks, tagged interfaces, and per-adapter settings, staged and applied
  with an automatic revert if the operator does not confirm in time
- Add the console views for adapters and for the network summary

* Fri Jul 24 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.1.0-1
- Initial lumen-controlplane package: management daemon, web console
  export, PAM service, systemd unit, firewalld service definition
