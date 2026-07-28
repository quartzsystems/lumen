# lumen-repo — where a Lumen appliance gets its updates.
#
# Repository configuration is its own package, not part of lumen-release, for
# the same reason the distributions split theirs: the address an appliance
# fetches from and the identity it reports are unrelated decisions, and one of
# them is replaced by a site that runs its own mirror while the other never is.
#
# It is also the only package that carries key material, which is what makes a
# separate package worth having: this one cannot be built without the public
# signing key, and nothing else should inherit that requirement. See
# packages/build-rpms.sh, which skips this package — loudly — when the key is
# not in the checkout.
#
# The version is injected by the build tooling from the top-level VERSION
# file; see lumen-release.spec for details.
%{!?lumen_version:%global lumen_version 0.1.0}

Name:           lumen-repo
Version:        %{lumen_version}
Release:        1%{?dist}
Summary:        Quartz Systems Lumen package repository configuration
License:        MIT
URL:            https://quartz.systems/
BuildArch:      noarch

Source0:        lumen.repo
Source1:        RPM-GPG-KEY-lumen

# NOTE: description wording avoids tokens the EL10 rpmlint spelling check
# flags as errors (zero-error policy).
%description
Package repository configuration for Quartz Systems Lumen. Provides the
repository definition the appliance installs its own updates from, and the
public key those packages and the repository index are verified against.
Both the packages and the index are checked, so an appliance will not
install anything this key did not sign.

%prep
# No source archive to unpack.

%build
# Nothing to compile.

%install
install -D -p -m 0644 %{SOURCE0} %{buildroot}%{_sysconfdir}/yum.repos.d/lumen.repo
install -D -p -m 0644 %{SOURCE1} %{buildroot}%{_sysconfdir}/pki/rpm-gpg/RPM-GPG-KEY-lumen

# No %post key import. The obvious thing — rpmkeys --import here — cannot work
# and never did: a scriptlet runs inside the transaction that installs this
# package, that transaction holds the rpm database lock for its whole duration
# including %posttrans, and rpmkeys needs the same lock to write a key. It fails
# with "can't create transaction lock", the "|| :" that made it look tolerable
# swallowed the exit status, and every node ended up without the key while the
# install reported success.
#
# The key is shipped to the path the repository definition names as its gpgkey,
# which is where dnf imports it from, on its own terms and outside any lock we
# hold. That import is a prompt, and an appliance has nobody to answer it, so
# both of the update service's dnf invocations pass --assumeyes: the check as
# well as the upgrade. The check is the one that matters — it runs first, and a
# check that cannot import the key reads a repository it then silently skips.
# See lumen-system/lumen-update/src/backend/dnf.rs.

%files
# noreplace: an operator who pointed this node at their own mirror, or turned
# the testing channel on, keeps that across upgrades of this package.
%config(noreplace) %{_sysconfdir}/yum.repos.d/lumen.repo
%{_sysconfdir}/pki/rpm-gpg/RPM-GPG-KEY-lumen

%changelog
* Mon Jul 27 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.3.0-1
- Initial lumen-repo package: the repository an appliance installs its own
  updates from, and the public key its packages and index are verified against
