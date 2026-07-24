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
Source1:        lumen-nm-00-lumen.conf

Requires:       bash
Requires:       iproute
Requires:       systemd
# The management daemon configures links through this service over the system
# bus, and the appliance policy fragment below is read by it.
Requires:       NetworkManager

# NOTE: description wording avoids tokens the EL10 rpmlint spelling check
# flags as errors (zero-error policy) — no literal tool or interface names.
%description
Network tooling for Quartz Systems Lumen. Provides a boot-time link-file
generator that pins deterministic NIC names in PCI order, so the installed
appliance keeps exactly the interface names the operator saw during
installation. Run it again after adding or replacing network hardware;
existing name pins are preserved. Also carries the appliance policy that
keeps the system from inventing settings for hardware nobody has configured.

%prep
# No source archive to unpack.

%build
# Nothing to compile.

%install
install -D -p -m 0755 %{SOURCE0} %{buildroot}%{_sbindir}/lumen-nicnames
# Vendor directory, not %%{_sysconfdir}: a local override is a higher-sorting
# file the operator adds, so this one never becomes a modified config file.
install -D -p -m 0644 %{SOURCE1} \
    %{buildroot}%{_prefix}/lib/NetworkManager/conf.d/00-lumen.conf

%files
%{_sbindir}/lumen-nicnames
%{_prefix}/lib/NetworkManager/conf.d/00-lumen.conf

%changelog
* Fri Jul 24 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.2.0-1
- Add the appliance policy fragment that stops automatic profiles being
  created for unconfigured hardware
- Depend on the service the management daemon configures links through

* Thu Jul 23 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.1.0-1
- Initial lumen-networking package: lumen-nicnames deterministic NIC
  naming (nic0..nicN by PCI order, systemd .link pins)
