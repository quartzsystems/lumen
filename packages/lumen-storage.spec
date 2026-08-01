# lumen-storage — Quartz Systems Lumen pool integration.
#
# System integration only. The pool logic itself compiles into the
# lumen-controlplane binary (the lumen-zfs crate, exactly as lumen-net does),
# so this package stays noarch.
#
# Version is injected by the build tooling from the top-level VERSION file;
# see lumen-release.spec for details.
%{!?lumen_version:%global lumen_version 0.1.0}

Name:           lumen-storage
Version:        %{lumen_version}
Release:        1%{?dist}
Summary:        Quartz Systems Lumen pool integration
License:        MIT
URL:            https://quartz.systems/
BuildArch:      noarch

Source0:        50-lumen-storage.preset
Source1:        50-lumen-cluster.preset
Source2:        lumen-cluster.xml
Source3:        lumen-replication.xml
Source4:        lumen-pool.xml
Source5:        50-lumen-pool.conf
Source6:        lumen-pool.conf

Requires:       systemd
# The pool tooling the management daemon reads through, and which the
# installed appliance already roots its own filesystem on.
Requires:       zfs
# The cluster stack the management daemon drives through its command lines:
# membership and quorum (corosync), fencing and the CIB (pacemaker, pcs, the
# one fence agent this appliance uses). Installed everywhere, running
# nowhere until a cluster exists — the preset below is what keeps that true.
Requires:       corosync
Requires:       pacemaker
Requires:       pcs
Requires:       fence-agents-ipmilan
# The shell helper every OCF agent sources resolves its own tool paths by
# shelling out to a command EL10 no longer installs by default, and the
# package that ships those agents does not ask for it. Without it the lookup
# fails for every tool, so each agent reports itself as not installed and the
# resource never starts — the address one is simply the first an operator
# meets. Asked for here because this is the package that puts those agents on
# a node.
Requires:       which
%{?systemd_requires}
BuildRequires:  systemd-rpm-macros

# NOTE: description wording avoids tokens the EL10 rpmlint spelling check
# flags as errors (zero-error policy) — no product, tool, or interface names,
# and no compound coinages.
%description
Pool integration for Quartz Systems Lumen. Pulls in the tooling the management
daemon reads pools and datasets through, and ships the policy that starts the
node's pool services on a fresh install — including the event service that
notices a device going away and reports a pool as degraded. The management
console reads pools at this release; creating and removing them is done from
the node itself.

%prep
# No source archive to unpack.

%build
# Nothing to compile; the pool logic ships inside the management daemon.

%install
# Vendor preset directory, not %%{_sysconfdir}: a local override is a
# higher-sorting file the operator adds, so this one never becomes a modified
# config file, and an operator who turns a service off keeps it off.
install -D -p -m 0644 %{SOURCE0} \
    %{buildroot}%{_prefix}/lib/systemd/system-preset/50-lumen-storage.preset
install -D -p -m 0644 %{SOURCE1} \
    %{buildroot}%{_prefix}/lib/systemd/system-preset/50-lumen-cluster.preset
# Service definitions only — nothing here opens a port. The cluster
# workflows bind these to the cluster's own interfaces when one is built,
# which is the only place they mean anything.
install -D -p -m 0644 %{SOURCE2} \
    %{buildroot}%{_prefix}/lib/firewalld/services/lumen-cluster.xml
install -D -p -m 0644 %{SOURCE3} \
    %{buildroot}%{_prefix}/lib/firewalld/services/lumen-replication.xml
install -D -p -m 0644 %{SOURCE4} \
    %{buildroot}%{_prefix}/lib/firewalld/services/lumen-pool.xml
# The pool daemon serves vdisks through ublk, whose interface is io_uring —
# refused outright by EL10's default. Vendor sysctl.d, not %%{_sysconfdir},
# for the same reason as the presets: an operator's override is a
# higher-sorting file they add, so this never becomes a modified config file.
install -D -p -m 0644 %{SOURCE5} \
    %{buildroot}%{_prefix}/lib/sysctl.d/50-lumen-pool.conf
# The guest device interface those vdisks are served through. The daemon's
# own unit loads the module when it starts, but the pool-create preflight
# asks each member for /dev/ublk-control before any pool exists — so the
# module has to be a boot-time fact, not a side effect of the first export.
install -D -p -m 0644 %{SOURCE6} \
    %{buildroot}%{_prefix}/lib/modules-load.d/lumen-pool.conf

%files
%{_prefix}/lib/systemd/system-preset/50-lumen-storage.preset
%{_prefix}/lib/systemd/system-preset/50-lumen-cluster.preset
%{_prefix}/lib/firewalld/services/lumen-cluster.xml
%{_prefix}/lib/firewalld/services/lumen-replication.xml
%{_prefix}/lib/firewalld/services/lumen-pool.xml
%{_prefix}/lib/sysctl.d/50-lumen-pool.conf
%{_prefix}/lib/modules-load.d/lumen-pool.conf

# A preset file is only advice until something acts on it, and nothing on an
# installed node ever runs `systemctl preset-all` again. This is that something:
# on first install it applies the preset above to exactly these units, which is
# how the vendor default gets honoured while an operator who later turns one off
# keeps it off. The units belong to the pool package this one requires, so they
# are on disk before this scriptlet runs.
#
# Only %%post. Removing this package must not unmount a node's datasets, so
# there is deliberately no %%preun that disables what it enabled.
%post
%systemd_post zfs-zed.service zfs-import-cache.service zfs-import.target \
    zfs-mount.service zfs-volume-wait.service zfs.target
# The cluster stack's presets say "disable": applying them on first install
# is what makes "installed everywhere, running nowhere" the recorded default
# rather than an accident of nothing having enabled them yet.
%systemd_post corosync.service pacemaker.service pcsd.service
# The modules-load drop-in above speaks at the next boot; a node that takes
# this package as an update should not need one to pass the pool preflight.
# In the installer's chroot there is no running kernel to load into, so
# failure is expected there and ignored.
/usr/sbin/modprobe ublk_drv >/dev/null 2>&1 || :

%changelog
* Fri Jul 31 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.3.8-1
- Load the ublk module at boot: the pool-create preflight asks for
  /dev/ublk-control before any pool exists, so the daemon's own unit
  loading it on start was one workflow too late
- Retire the DRBD engine: LumenFS pooled storage is the appliance's one
  replicated engine, so the drbd9x module and userland requirements go,
  and the replication firewalld service keeps only the migration ports
* Sun Jul 27 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.6.0-1
- Cluster and replication stack: corosync, pacemaker, the one fence agent,
  and the DRBD module and userland, with presets keeping the daemons off
  until a cluster exists, and the two firewalld service definitions the
  cluster networks bind

* Fri Jul 24 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.3.0-1
- Initial lumen-storage package: pulls in the pool tooling and ships the
  policy that starts the node's pool services, event reporting included
