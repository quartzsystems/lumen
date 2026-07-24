# lumen-logos — Quartz Systems Lumen logos and artwork.
#
# Version is injected by the build tooling from the top-level VERSION file;
# see lumen-release.spec for details.
%{!?lumen_version:%global lumen_version 0.1.0}

Name:           lumen-logos
Version:        %{lumen_version}
Release:        1%{?dist}
Summary:        Quartz Systems Lumen logos and artwork
License:        MIT
URL:            https://www.quartzsystems.net
BuildArch:      noarch

Source0:        lumen-mark.svg
Source1:        lumen-mark-light-bg.svg
Source2:        lumen-favicon.svg
Source3:        lumen-lockup-dark-bg.svg
Source4:        lumen-lockup-light-bg.svg
Source5:        lumen-favicon-64.png
Source6:        lumen-lockup-dark-bg.png
Source7:        lumen-lockup-light-bg.png
Source8:        lumen-mark-1024.png
Source9:        README.md
Source10:       lumen-profile.conf
Source11:       lumen.plymouth

# %%{?el10}: filesystem guarantees /usr/share/pixmaps ownership on EL10.
%if 0%{?el10}
Requires:       filesystem
%endif

%description
Logos and artwork for Quartz Systems Lumen: the layered mark, horizontal
lockups for dark and light backgrounds, a square icon, and raster
renditions, staged for later Anaconda installer and Plymouth boot-splash
branding. The lockup vector files set the product name in JetBrains Mono;
use the raster renditions where that font is not available.

%prep
# No source archive to unpack.

%build
# Nothing to compile.

%install
install -d -m 0755 %{buildroot}%{_datadir}/lumen-logos/png
install -d -m 0755 %{buildroot}%{_datadir}/lumen-logos/anaconda
install -d -m 0755 %{buildroot}%{_datadir}/lumen-logos/plymouth
install -d -m 0755 %{buildroot}%{_datadir}/pixmaps

install -p -m 0644 %{SOURCE0} %{buildroot}%{_datadir}/lumen-logos/lumen-mark.svg
install -p -m 0644 %{SOURCE1} %{buildroot}%{_datadir}/lumen-logos/lumen-mark-light-bg.svg
install -p -m 0644 %{SOURCE2} %{buildroot}%{_datadir}/lumen-logos/lumen-favicon.svg
install -p -m 0644 %{SOURCE3} %{buildroot}%{_datadir}/lumen-logos/lumen-lockup-dark-bg.svg
install -p -m 0644 %{SOURCE4} %{buildroot}%{_datadir}/lumen-logos/lumen-lockup-light-bg.svg
install -p -m 0644 %{SOURCE5} %{buildroot}%{_datadir}/lumen-logos/png/lumen-favicon-64.png
install -p -m 0644 %{SOURCE6} %{buildroot}%{_datadir}/lumen-logos/png/lumen-lockup-dark-bg.png
install -p -m 0644 %{SOURCE7} %{buildroot}%{_datadir}/lumen-logos/png/lumen-lockup-light-bg.png
install -p -m 0644 %{SOURCE8} %{buildroot}%{_datadir}/lumen-logos/png/lumen-mark-1024.png
install -p -m 0644 %{SOURCE9} %{buildroot}%{_datadir}/lumen-logos/README.md
install -p -m 0644 %{SOURCE10} %{buildroot}%{_datadir}/lumen-logos/anaconda/lumen-profile.conf
install -p -m 0644 %{SOURCE11} %{buildroot}%{_datadir}/lumen-logos/plymouth/lumen.plymouth

ln -s ../lumen-logos/lumen-mark.svg %{buildroot}%{_datadir}/pixmaps/lumen.svg

%files
%dir %{_datadir}/lumen-logos
%dir %{_datadir}/lumen-logos/png
%dir %{_datadir}/lumen-logos/anaconda
%dir %{_datadir}/lumen-logos/plymouth
%{_datadir}/lumen-logos/lumen-mark.svg
%{_datadir}/lumen-logos/lumen-mark-light-bg.svg
%{_datadir}/lumen-logos/lumen-favicon.svg
%{_datadir}/lumen-logos/lumen-lockup-dark-bg.svg
%{_datadir}/lumen-logos/lumen-lockup-light-bg.svg
%{_datadir}/lumen-logos/png/lumen-favicon-64.png
%{_datadir}/lumen-logos/png/lumen-lockup-dark-bg.png
%{_datadir}/lumen-logos/png/lumen-lockup-light-bg.png
%{_datadir}/lumen-logos/png/lumen-mark-1024.png
%{_datadir}/lumen-logos/README.md
%{_datadir}/lumen-logos/anaconda/lumen-profile.conf
%{_datadir}/lumen-logos/plymouth/lumen.plymouth
%{_datadir}/pixmaps/lumen.svg

%changelog
* Thu Jul 23 2026 Quartz Systems Engineering <engineering@quartz.systems> - 0.1.0-1
- Initial lumen-logos package: layered mark, lockups, favicon, raster
  renditions, Anaconda and Plymouth staging directories
