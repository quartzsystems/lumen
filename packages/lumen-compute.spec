# lumen-compute — Quartz Systems Lumen machine-hosting integration.
#
# System integration only. The machine logic itself compiles into the
# lumen-controlplane binary (the lumen-virt crate, exactly as lumen-net does),
# so this package stays noarch and carries the appliance policy that makes a
# freshly installed node able to host machines at all.
#
# Version is injected by the build tooling from the top-level VERSION file;
# see lumen-release.spec for details.
%{!?lumen_version:%global lumen_version 0.1.0}

Name:           lumen-compute
Version:        %{lumen_version}
Release:        1%{?dist}
Summary:        Quartz Systems Lumen machine-hosting integration
License:        MIT
URL:            https://www.quartzsystems.net
BuildArch:      noarch

Source0:        50-lumen-compute.preset

Requires:       systemd
# The privileged daemons the management daemon delegates every machine
# operation to. They run in their own processes, which is what lets the
# management daemon keep its own restrictions.
#
# The floor is a real requirement, not a formality: the machine documents this
# release writes ask the service to select boot firmware for itself, which
# needs 5.2 or newer, and the appliance enables the per-driver socket units,
# which are only production-ready from 7.0. 8.0.0 clears both comfortably and
# is far below what the target distribution ships, so it never blocks an
# install. It also keeps the automatic dependency checker from mistaking a
# service package for a shared library on the strength of its name.
Requires:       libvirt-daemon-kvm >= 8.0.0
# Implied by the package above, but named because this is the driver every
# machine operation actually goes to.
Requires:       libvirt-daemon-driver-qemu >= 8.0.0
Requires:       qemu-kvm
# Firmware images for machines that start the modern way. The management
# daemon defines every machine to use them by default.
Requires:       edk2-ovmf
# The guest description database. A data package, not a library: the
# management daemon reads its files directly, so nothing links against it and
# nothing generates bindings for it. Without it the console has no list of
# guest systems to offer and falls back to a free-text identifier, which works
# but is a worse answer than the one the distribution already ships.
Requires:       osinfo-db
%{?systemd_requires}
BuildRequires:  systemd-rpm-macros

# NOTE: description wording avoids tokens the EL10 rpmlint spelling check
# flags as errors (zero-error policy) — no product, tool, or interface names,
# and no compound coinages.
%description
Machine-hosting integration for Quartz Systems Lumen. Pulls in the privileged
services that create and run guest machines on a node, and ships the policy
that has them start on a fresh install. The services are started on demand by
their sockets, so a node with no guests on it runs nothing extra. All guest
operations are performed by these services in their own processes, which is
what allows the management daemon to keep its own restrictions in place.

%prep
# No source archive to unpack.

%build
# Nothing to compile; the machine logic ships inside the management daemon.

%install
# Vendor preset directory, not %%{_sysconfdir}: a local override is a
# higher-sorting file the operator adds, so this one never becomes a modified
# config file, and an operator who turns a service off keeps it off.
install -D -p -m 0644 %{SOURCE0} \
    %{buildroot}%{_prefix}/lib/systemd/system-preset/50-lumen-compute.preset

%files
%{_prefix}/lib/systemd/system-preset/50-lumen-compute.preset

# A preset file is only advice until something acts on it, and nothing on an
# installed node ever runs `systemctl preset-all` again. This is that something:
# on first install it applies the preset above to exactly these units, which is
# how the vendor default gets honoured while an operator who later turns one off
# keeps it off. The units belong to the daemon packages this one requires, so
# they are on disk before this scriptlet runs.
#
# Only %%post. Removing this package must not stop a node's running guests, so
# there is deliberately no %%preun that disables what it enabled.
%post
%systemd_post virtqemud.socket virtqemud-ro.socket virtqemud-admin.socket \
    virtstoraged.socket virtnetworkd.socket virtnodedevd.socket \
    virtproxyd.socket

%changelog
* Fri Jul 24 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.3.0-1
- Initial lumen-compute package: pulls in the guest services and the firmware
  images a node needs, and ships the policy that starts them on demand
