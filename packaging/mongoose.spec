# Binary is built by the Makefile (cargo build --release) and handed in
# via the rpmbuild SOURCES dir; this spec only stages and packages it.
%define debug_package %{nil}
%define _build_id_links none

Name:           mongoose
Version:        %{pkg_version}
Release:        1%{?dist}
Summary:        Single-host NFS-to-NFS data mover with incremental resync
License:        AGPL-3.0-only
URL:            https://github.com/blakegolliher/mongoose

%description
mongoose migrates one NFS export to another from a single host: scan
the source with an embedded parallel walker, build a canonical parquet
index, copy over raw NFSv3 with libnfs, then resync incrementally while
the source stays live and finish with a verified cutover. One binary,
no external tools, no coordinator, no S3.

%install
install -D -m0755 %{_sourcedir}/mongoose %{buildroot}%{_bindir}/mongoose
install -D -m0644 %{_sourcedir}/mongoose.1 %{buildroot}%{_mandir}/man1/mongoose.1

%files
%{_bindir}/mongoose
%{_mandir}/man1/mongoose.1*

%changelog
* Tue Sep 01 2026 Blake Golliher <blakegolliher@gmail.com> - 0.1.0-1
- Initial package: mongoose binary and man page.
