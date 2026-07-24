# lumen-release — Quartz Systems Lumen release identification and branding.
#
# The authoritative version lives in the top-level VERSION file; the build
# tooling (Makefile / packages/build-rpms.sh) injects it with
#   rpmbuild --define "lumen_version X.Y.Z"
# The fallback below only exists so a bare `rpmbuild -bb` still works.
%{!?lumen_version:%global lumen_version 0.1.0}

Name:           lumen-release
Version:        %{lumen_version}
Release:        1%{?dist}
Summary:        Quartz Systems Lumen release and branding files
License:        MIT
URL:            https://www.quartzsystems.net
BuildArch:      noarch

Source0:        lumen-release.in
Source1:        os-release-lumen.conf
Source2:        issue.in
Source3:        motd.in

# %%{?el10}: only pull the AlmaLinux base release package when building for
# EL10; other build targets (future rebases) may provide their own.
%if 0%{?el10}
Requires:       almalinux-release
%endif

%description
Release identification and branding files for Quartz Systems Lumen, a
light-weight KVM orchestration appliance built on AlmaLinux. Provides the
/etc/lumen-release version file, Lumen VARIANT keys for operating system
identification, and branded login banner content.

%prep
# No source archive to unpack.

%build
# Nothing to compile; files are rendered in %%install.

%install
install -d -m 0755 %{buildroot}%{_sysconfdir}
install -d -m 0755 %{buildroot}%{_datadir}/lumen-release

sed 's/@VERSION@/%{version}/g' %{SOURCE0} > %{buildroot}%{_sysconfdir}/lumen-release
sed 's/@VERSION@/%{version}/g' %{SOURCE2} > %{buildroot}%{_datadir}/lumen-release/issue
# @ESC@ keeps the template readable in git; render it to a real escape byte
# so the motd shows the Lumen mark in its brand greens (Quartz tokens
# #4dffb2 / #00d992 at 55%% and 28%% -> 256-color 85 / 36 / 23).
sed -e 's/@VERSION@/%{version}/g' \
    -e "s/@ESC@/$(printf '\033')/g" \
    %{SOURCE3} > %{buildroot}%{_datadir}/lumen-release/motd
install -p -m 0644 %{SOURCE1} %{buildroot}%{_datadir}/lumen-release/os-release-lumen.conf
chmod 0644 %{buildroot}%{_sysconfdir}/lumen-release \
           %{buildroot}%{_datadir}/lumen-release/issue \
           %{buildroot}%{_datadir}/lumen-release/motd

%post
# /etc/os-release is stock a symlink to /usr/lib/os-release (owned by
# almalinux-release). Materialize a real /etc/os-release — the documented
# override path — with Lumen's VARIANT/HOME_URL keys, instead of editing
# the almalinux-release-owned target.
if [ -r /usr/lib/os-release ]; then
    {
        grep -Ev '^(VARIANT|VARIANT_ID|HOME_URL)=' /usr/lib/os-release
        cat %{_datadir}/lumen-release/os-release-lumen.conf
    } > /etc/os-release.lumen-new && mv -f /etc/os-release.lumen-new /etc/os-release
fi

# /etc/issue and /etc/motd are owned by almalinux-release / setup;
# overwrite their content in scriptlets rather than co-owning the paths
# (co-ownership would be an RPM file conflict).
cp -f %{_datadir}/lumen-release/issue /etc/issue
cp -f %{_datadir}/lumen-release/motd /etc/motd

%postun
if [ "$1" -eq 0 ]; then
    # Erase (not upgrade): restore stock identity files.
    ln -snf ../usr/lib/os-release /etc/os-release
    {
        echo '\S'
        echo 'Kernel \r on an \m'
        echo ''
    } > /etc/issue
    : > /etc/motd
fi

%files
%{_sysconfdir}/lumen-release
%dir %{_datadir}/lumen-release
%{_datadir}/lumen-release/os-release-lumen.conf
%{_datadir}/lumen-release/issue
%{_datadir}/lumen-release/motd

%changelog
* Thu Jul 23 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.1.0-1
- Initial lumen-release package: /etc/lumen-release, os-release VARIANT
  additions, branded issue and motd
