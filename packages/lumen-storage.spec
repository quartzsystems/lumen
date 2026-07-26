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

Requires:       systemd
# The pool tooling the management daemon reads through, and which the
# installed appliance already roots its own filesystem on.
Requires:       zfs
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

%files
%{_prefix}/lib/systemd/system-preset/50-lumen-storage.preset

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

%changelog
* Fri Jul 24 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.3.0-1
- Initial lumen-storage package: pulls in the pool tooling and ships the
  policy that starts the node's pool services, event reporting included
