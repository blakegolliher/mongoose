# Build and package mongoose.
#
#   make build     host release binary (target/release/mongoose)
#   make rpm       binary RPM + man page        -> dist/
#   make deb       binary DEB + man page        -> dist/
#   make tarball   plain tar.gz (binary + man)  -> dist/
#   make release   rpm + deb + tarball
#   make clean     remove dist/ (cargo clean is separate)
#
# Packages and the tarball are PORTABLE by default: built with
# cargo-zigbuild against glibc $(GLIBC) and the digest-pinned libnfs
# stage (see packaging/libnfs.lock.json), then gated on the binary's
# max GLIBC_* symbol version. `make rpm PORTABLE=0` opts out and
# packages a host build instead (links whatever libnfs pkg-config
# finds — do not ship those).
#
# The stage dir must hold the pinned pair from the lock file:
#   libnfs.a    walker's static link (zig cc -target x86_64-linux-gnu.$(GLIBC))
#   libnfs.so   mover's link probe (dropped by --as-needed)
# Default location is packaging/libnfs-stage (gitignored); override
# with LIBNFS_STAGE=/path.
#
# Requires: rpmbuild (`rpm`), dpkg-deb (`deb`), and for portable
# builds cargo-zigbuild + zig (versions pinned in
# packaging/release-toolchain.lock.json).

VERSION  ?= $(shell sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -n1)
UNAME_M  := $(shell uname -m)
RPM_ARCH := $(UNAME_M)
DEB_ARCH := $(shell dpkg --print-architecture 2>/dev/null || echo amd64)

GLIBC        := 2.34
TRIPLE       := x86_64-unknown-linux-gnu
LIBNFS_STAGE ?= $(CURDIR)/packaging/libnfs-stage
LIBNFS_LOCK  := packaging/libnfs.lock.json

PORTABLE ?= 1
ifeq ($(PORTABLE),1)
BIN        := target/$(TRIPLE)/release/mongoose
BUILD_RULE := build-portable
else
BIN        := target/release/mongoose
BUILD_RULE := build
endif

PKGDIR   := packaging
DIST     := dist

RPM_OUT  := $(DIST)/mongoose-$(VERSION)-1.$(RPM_ARCH).rpm
DEB_OUT  := $(DIST)/mongoose_$(VERSION)-1_$(DEB_ARCH).deb
TAR_OUT  := $(DIST)/mongoose-$(VERSION)-linux-$(UNAME_M).tar.gz

.PHONY: all build build-portable stage-check rpm deb tarball release clean

all: build

build:
	cargo build --release -p mongoose

# --- portable build --------------------------------------------------
# Both env vars point at the stage: VAMOOSE_LIBNFS_DIR for the mover's
# build.rs, NFS_WALKER_LIBNFS_DIR for the embedded walker's. After the
# build, refuse any binary whose glibc requirement exceeds $(GLIBC).
build-portable: stage-check
	VAMOOSE_LIBNFS_DIR=$(LIBNFS_STAGE) \
	NFS_WALKER_LIBNFS_DIR=$(LIBNFS_STAGE) \
	cargo zigbuild --release --target $(TRIPLE).$(GLIBC) -p mongoose
	@max=$$(objdump -T target/$(TRIPLE)/release/mongoose \
		| grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -n1); \
	echo "max glibc symbol: $$max (ceiling GLIBC_$(GLIBC))"; \
	highest=$$(printf '%s\nGLIBC_%s\n' "$$max" "$(GLIBC)" | sort -V | tail -n1); \
	if [ "$$highest" != "GLIBC_$(GLIBC)" ]; then \
		echo "ERROR: binary requires $$max, exceeds GLIBC_$(GLIBC) — not portable"; \
		exit 1; \
	fi

stage-check:
	@[ -f "$(LIBNFS_STAGE)/libnfs.a" ] && [ -f "$(LIBNFS_STAGE)/libnfs.so" ] || { \
		echo "ERROR: $(LIBNFS_STAGE) must contain libnfs.a and libnfs.so"; \
		echo "Build both from the source pinned in $(LIBNFS_LOCK) with"; \
		echo "  zig cc -target x86_64-linux-gnu.$(GLIBC)"; \
		echo "or point LIBNFS_STAGE at an existing stage."; \
		exit 1; }
	@want_so=$$(jq -r .artifact_sha256 $(LIBNFS_LOCK)); \
	want_a=$$(jq -r .static_artifact_sha256 $(LIBNFS_LOCK)); \
	got_so=$$(sha256sum "$(LIBNFS_STAGE)/libnfs.so" | cut -d' ' -f1); \
	got_a=$$(sha256sum "$(LIBNFS_STAGE)/libnfs.a" | cut -d' ' -f1); \
	[ "$$got_so" = "$$want_so" ] || { echo "ERROR: libnfs.so sha256 $$got_so != pinned $$want_so"; exit 1; }; \
	[ "$$got_a" = "$$want_a" ] || { echo "ERROR: libnfs.a sha256 $$got_a != pinned $$want_a"; exit 1; }; \
	echo "libnfs stage verified against $(LIBNFS_LOCK)"

$(DIST):
	mkdir -p $(DIST)

# --- RPM -------------------------------------------------------------
# The spec packages the SOURCES-staged mongoose + mongoose.1; version
# is injected with --define so the spec never drifts from Cargo.toml.
rpm: $(BUILD_RULE) | $(DIST)
	rm -rf $(DIST)/rpmbuild
	mkdir -p $(DIST)/rpmbuild/SOURCES
	cp $(BIN) $(PKGDIR)/mongoose.1 $(DIST)/rpmbuild/SOURCES/
	rpmbuild -bb $(PKGDIR)/mongoose.spec \
		--define "_topdir $(CURDIR)/$(DIST)/rpmbuild" \
		--define "pkg_version $(VERSION)" \
		--target $(RPM_ARCH)
	cp $(DIST)/rpmbuild/RPMS/$(RPM_ARCH)/mongoose-$(VERSION)-1*.rpm $(RPM_OUT)

# --- DEB -------------------------------------------------------------
deb: $(BUILD_RULE) | $(DIST)
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
tarball: $(BUILD_RULE) | $(DIST)
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
