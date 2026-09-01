# Build and package mongoose.
#
#   make build     release binary (target/release/mongoose)
#   make rpm       binary RPM + man page        -> dist/
#   make deb       binary DEB + man page        -> dist/
#   make tarball   plain tar.gz (binary + man)  -> dist/
#   make release   all of the above
#   make clean     remove dist/ (cargo clean is separate)
#
# The RPM/DEB stage the prebuilt release binary; they do not rebuild
# from source inside the package tools. Requires rpmbuild for `rpm`
# and dpkg-deb for `deb`.

VERSION  ?= $(shell sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -n1)
UNAME_M  := $(shell uname -m)
RPM_ARCH := $(UNAME_M)
DEB_ARCH := $(shell dpkg --print-architecture 2>/dev/null || echo amd64)

BIN      := target/release/mongoose
PKGDIR   := packaging
DIST     := dist

RPM_OUT  := $(DIST)/mongoose-$(VERSION)-1.$(RPM_ARCH).rpm
DEB_OUT  := $(DIST)/mongoose_$(VERSION)-1_$(DEB_ARCH).deb
TAR_OUT  := $(DIST)/mongoose-$(VERSION)-linux-$(UNAME_M).tar.gz

.PHONY: all build rpm deb tarball release clean

all: build

build:
	cargo build --release -p mongoose

$(DIST):
	mkdir -p $(DIST)

# --- RPM -------------------------------------------------------------
# The spec packages %{_sourcedir}/mongoose + mongoose.1; version is
# injected with --define so the spec never drifts from Cargo.toml.
rpm: build | $(DIST)
	rm -rf $(DIST)/rpmbuild
	mkdir -p $(DIST)/rpmbuild/SOURCES
	cp $(BIN) $(PKGDIR)/mongoose.1 $(DIST)/rpmbuild/SOURCES/
	rpmbuild -bb $(PKGDIR)/mongoose.spec \
		--define "_topdir $(CURDIR)/$(DIST)/rpmbuild" \
		--define "pkg_version $(VERSION)" \
		--target $(RPM_ARCH)
	cp $(DIST)/rpmbuild/RPMS/$(RPM_ARCH)/mongoose-$(VERSION)-1*.rpm $(RPM_OUT)

# --- DEB -------------------------------------------------------------
deb: build | $(DIST)
	rm -rf $(DIST)/debroot
	install -D -m0755 $(BIN) $(DIST)/debroot/usr/bin/mongoose
	install -D -m0644 $(PKGDIR)/mongoose.1 $(DIST)/debroot/usr/share/man/man1/mongoose.1
	gzip -9n $(DIST)/debroot/usr/share/man/man1/mongoose.1
	install -D -m0644 LICENSE $(DIST)/debroot/usr/share/doc/mongoose/copyright
	install -d $(DIST)/debroot/DEBIAN
	sed -e 's/@VERSION@/$(VERSION)-1/' -e 's/@ARCH@/$(DEB_ARCH)/' \
		$(PKGDIR)/deb-control.in > $(DIST)/debroot/DEBIAN/control
	dpkg-deb --build --root-owner-group $(DIST)/debroot $(DEB_OUT)

# --- tarball ---------------------------------------------------------
tarball: build | $(DIST)
	rm -rf $(DIST)/tarroot
	install -D -m0755 $(BIN) $(DIST)/tarroot/mongoose
	install -D -m0644 $(PKGDIR)/mongoose.1 $(DIST)/tarroot/mongoose.1
	install -D -m0644 README.md $(DIST)/tarroot/README.md
	install -D -m0644 LICENSE $(DIST)/tarroot/LICENSE
	tar -C $(DIST)/tarroot -czf $(TAR_OUT) .

release: rpm deb tarball
	@echo
	@echo "release artifacts:"
	@ls -l $(RPM_OUT) $(DEB_OUT) $(TAR_OUT)

clean:
	rm -rf $(DIST)
