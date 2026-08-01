# lumen-fsd — the Lumen pooled storage daemon.
#
# Its own package, and deliberately not part of lumen-controlplane, for one
# reason that decides it: lumen-controlplane.spec carries
# `%%systemd_postun_with_restart`, because restarting the management daemon
# costs nothing — a console reconnects. Restarting *this* daemon takes every
# ublk device down, and every guest disk with it. Shipping the two together
# would mean a routine management update dropped the disks out from under
# running machines.
#
# So: separate package, no restart scriptlet, and a unit that says
# `Restart=no` out loud. Coming back needs the machines drained first, which
# is what the rolling-update path already does (docs/updates.md).
#
# The binary is built by packages/build-rpms.sh before rpmbuild runs and
# handed in as a plain source, exactly as lumen-controlplane's is — cargo
# needs crates.io, which does not belong inside the rpmbuild step.
#
# The version is injected by the build tooling from the top-level VERSION
# file; see lumen-release.spec for details.
%{!?lumen_version:%global lumen_version 0.1.0}

# The source is a prebuilt, already-stripped binary; no debuginfo to extract.
%global debug_package %{nil}

Name:           lumen-fsd
Version:        %{lumen_version}
Release:        1%{?dist}
Summary:        Quartz Systems Lumen pooled storage daemon
License:        MIT
URL:            https://quartz.systems/

Source0:        lumen-fsd
Source1:        lumen-fsd.service
Source2:        50-lumen-pool.preset

Requires:       systemd
# The guest device interface. In-tree on the EL10 kernel this appliance
# ships, so this is a module to load rather than a package to install — the
# unit requires modprobe@ublk_drv.service and says so.
#
# The io_uring the ublk interface is built on is refused by EL10's default
# sysctl; the drop-in that permits it for privileged callers ships in
# lumen-storage, which every node with storage already carries.
Requires:       lumen-storage

%description
The data-plane daemon for LumenFS, Lumen's pooled storage: it serves each
virtual disk as a block device the hypervisor opens, and replicates every
acknowledged write to the pool's other member before acknowledging it. A
write is durable when both members hold it, or when the cluster has
confirmed the other member fenced.

Installed on every node that can carry storage and running on none of them
until a pool is created: the daemon needs a brick to serve and a peer to
replicate to, and a workflow supplies both.

%prep
# No source archive to unpack.

%build
# Nothing to compile; the daemon binary arrives prebuilt as Source0.

%install
install -D -p -m 0755 %{SOURCE0} %{buildroot}%{_sbindir}/lumen-fsd
install -D -p -m 0644 %{SOURCE1} \
    %{buildroot}%{_prefix}/lib/systemd/system/lumen-fsd.service
# Vendor preset directory, not %%{_sysconfdir}: an operator's override is a
# higher-sorting file they add, so this never becomes a modified config file,
# and a node where the daemon was enabled by hand keeps it enabled.
install -D -p -m 0644 %{SOURCE2} \
    %{buildroot}%{_prefix}/lib/systemd/system-preset/50-lumen-pool.preset
# The configuration directory the unit reads from. The file itself is written
# by the workflow that creates a pool, because its contents are per-node.
install -d -m 0755 %{buildroot}%{_sysconfdir}/lumen

%files
%{_sbindir}/lumen-fsd
%{_prefix}/lib/systemd/system/lumen-fsd.service
%{_prefix}/lib/systemd/system-preset/50-lumen-pool.preset
%dir %{_sysconfdir}/lumen

%post
# Registers the unit with systemd and applies the vendor preset on first
# install, which is what keeps it disabled until a pool exists.
%systemd_post lumen-fsd.service

%preun
%systemd_preun lumen-fsd.service

%postun
# `%%systemd_postun` and NOT `%%systemd_postun_with_restart`. This is the
# whole reason the package is separate: an update must never restart a daemon
# that guest disks are open through. A node takes this daemon down by being
# drained, not by being patched.
%systemd_postun lumen-fsd.service

%changelog
* Wed Jul 29 2026 Quartz Systems Engineering <engineering@quartz.systems> - %{lumen_version}-1
- Initial package: the pooled storage daemon, installed everywhere and
  enabled nowhere until a pool exists.
