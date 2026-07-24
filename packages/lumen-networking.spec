# lumen-networking — Quartz Systems Lumen network tooling.
#
# Version is injected by the build tooling from the top-level VERSION file;
# see lumen-release.spec for details.
%{!?lumen_version:%global lumen_version 0.1.0}

Name:           lumen-networking
Version:        %{lumen_version}
Release:        1%{?dist}
Summary:        Quartz Systems Lumen network tooling
License:        MIT
URL:            https://www.quartzsystems.net
BuildArch:      noarch

Source0:        lumen-nicnames

Requires:       bash
Requires:       iproute
Requires:       systemd

# NOTE: description wording avoids tokens the EL10 rpmlint spelling check
# flags as errors (zero-error policy) — no literal tool or interface names.
%description
Network tooling for Quartz Systems Lumen. Provides a boot-time link-file
generator that pins deterministic NIC names in PCI order, so the installed
appliance keeps exactly the interface names the operator saw during
installation. Run it again after adding or replacing network hardware;
existing name pins are preserved.

%prep
# No source archive to unpack.

%build
# Nothing to compile.

%install
install -D -p -m 0755 %{SOURCE0} %{buildroot}%{_sbindir}/lumen-nicnames

%files
%{_sbindir}/lumen-nicnames

%changelog
* Thu Jul 23 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.1.0-1
- Initial lumen-networking package: lumen-nicnames deterministic NIC
  naming (nic0..nicN by PCI order, systemd .link pins)
