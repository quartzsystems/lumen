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
Source1:        os-release-lumen.conf.in
Source2:        issue.in
Source3:        motd.in
Source4:        theme.txt
Source5:        lumen-grub-bg.png
Source6:        lumen-console-banner
Source7:        lumen-console-banner.service
Source8:        50-lumen-banner

# The console banner reads addresses with ip(8).
Requires:       iproute
%{?systemd_requires}
BuildRequires:  systemd-rpm-macros

# %%{?el10}: only pull the AlmaLinux base release package when building for
# EL10; other build targets (future rebases) may provide their own.
%if 0%{?el10}
Requires:       almalinux-release
%endif

%description
Release identification and branding files for Quartz Systems Lumen, a
light-weight KVM orchestration appliance built on AlmaLinux. Provides the
/etc/lumen-release version file, Lumen VARIANT keys for operating system
identification, branded login banner content, and the boot console banner
that shows the appliance's host name, current address, and management
console location.

%prep
# No source archive to unpack.

%build
# Nothing to compile; files are rendered in %%install.

%install
install -d -m 0755 %{buildroot}%{_sysconfdir}
install -d -m 0755 %{buildroot}%{_datadir}/lumen-release
install -d -m 0755 %{buildroot}%{_datadir}/lumen-release/grub

sed 's/@VERSION@/%{version}/g' %{SOURCE0} > %{buildroot}%{_sysconfdir}/lumen-release
sed 's/@VERSION@/%{version}/g' %{SOURCE1} > %{buildroot}%{_datadir}/lumen-release/os-release-lumen.conf
sed 's/@VERSION@/%{version}/g' %{SOURCE2} > %{buildroot}%{_datadir}/lumen-release/issue
# @ESC@ keeps the template readable in git; render it to a real escape byte
# so the motd shows the Lumen mark in its brand greens (Quartz tokens
# #4dffb2 / #00d992 at 55%% and 28%% -> 256-color 85 / 36 / 23).
sed -e 's/@VERSION@/%{version}/g' \
    -e "s/@ESC@/$(printf '\033')/g" \
    %{SOURCE3} > %{buildroot}%{_datadir}/lumen-release/motd
# GRUB gfxmenu theme for the installed system's boot menu; the installer
# stages these onto /boot/grub2/themes/lumen (same theme the ISO menu uses).
install -p -m 0644 %{SOURCE4} %{buildroot}%{_datadir}/lumen-release/grub/theme.txt
install -p -m 0644 %{SOURCE5} %{buildroot}%{_datadir}/lumen-release/grub/lumen-grub-bg.png
# Boot console banner: renders /etc/issue and the dynamic motd fragment with
# the current management address; re-run by the NetworkManager dispatcher
# hook when addresses change.
install -D -p -m 0755 %{SOURCE6} %{buildroot}%{_sbindir}/lumen-console-banner
install -D -p -m 0644 %{SOURCE7} %{buildroot}%{_unitdir}/lumen-console-banner.service
install -D -p -m 0755 %{SOURCE8} %{buildroot}%{_prefix}/lib/NetworkManager/dispatcher.d/50-lumen-banner
chmod 0644 %{buildroot}%{_sysconfdir}/lumen-release \
           %{buildroot}%{_datadir}/lumen-release/os-release-lumen.conf \
           %{buildroot}%{_datadir}/lumen-release/issue \
           %{buildroot}%{_datadir}/lumen-release/motd

%post
# /etc/os-release is stock a symlink to /usr/lib/os-release (owned by
# almalinux-release). Materialize a real /etc/os-release — the documented
# override path — with Lumen's identity keys, instead of editing the
# almalinux-release-owned target. NAME/PRETTY_NAME/ANSI_COLOR are what
# systemd's "Welcome to ..." boot banner and agetty's \S render; ID and
# VERSION_ID stay AlmaLinux so tooling keyed on them keeps working.
if [ -r /usr/lib/os-release ]; then
    {
        grep -Ev '^(NAME|PRETTY_NAME|ANSI_COLOR|VARIANT|VARIANT_ID|HOME_URL)=' /usr/lib/os-release
        cat %{_datadir}/lumen-release/os-release-lumen.conf
    } > /etc/os-release.lumen-new && mv -f /etc/os-release.lumen-new /etc/os-release
fi

# /etc/issue and /etc/motd are owned by almalinux-release / setup;
# overwrite their content in scriptlets rather than co-owning the paths
# (co-ownership would be an RPM file conflict).
cp -f %{_datadir}/lumen-release/issue /etc/issue
cp -f %{_datadir}/lumen-release/motd /etc/motd

%systemd_post lumen-console-banner.service

%preun
%systemd_preun lumen-console-banner.service

%postun
%systemd_postun lumen-console-banner.service
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
%dir %{_datadir}/lumen-release/grub
%{_datadir}/lumen-release/os-release-lumen.conf
%{_datadir}/lumen-release/issue
%{_datadir}/lumen-release/motd
%{_datadir}/lumen-release/grub/theme.txt
%{_datadir}/lumen-release/grub/lumen-grub-bg.png
%{_sbindir}/lumen-console-banner
%{_unitdir}/lumen-console-banner.service
%{_prefix}/lib/NetworkManager/dispatcher.d/50-lumen-banner

%changelog
* Fri Jul 24 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.1.0-1
- Boot console banner: host name, current address, and console location on
  the pre-login screen, refreshed when addresses change
- Brand the boot banner: os-release NAME/PRETTY_NAME/ANSI_COLOR overrides
- Ship the GRUB gfxmenu theme for the installed system's boot menu

* Thu Jul 23 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.1.0-1
- Initial lumen-release package: /etc/lumen-release, os-release VARIANT
  additions, branded issue and motd
