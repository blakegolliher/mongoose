# Build and package mongoose.
#
#   make build     host release binary (target/release/mongoose)
#   make rpm       binary RPM + man page        -> dist/
#   make deb       binary DEB + man page        -> dist/
#   make tarball   plain tar.gz (binary + man)  -> dist/
#   make binary    bare binary + SHA256SUMS      -> dist/
#   make libnfs-stage  reproducibly build pinned static libnfs
#   make release-materials  source/relink/license assets + relink proof
#   make release   rpm + deb + tarball + binary, SBOM, SHA256SUMS, then the
#                  fail-closed LGPL and artifact gates (the release workflow
#                  signs and publishes; see docs/BUILDING.md)
#   make toolchain-check  verify every tool against the release lock
#   make compliance-check  validate checked-in LGPL release policy
#   make clean     remove dist/ (cargo clean is separate)
#
# Packages and the tarball are PORTABLE by default: built with
# cargo-zigbuild against glibc $(GLIBC) and the digest-pinned libnfs
# stage (see packaging/libnfs.lock.json), then gated on the binary's
# max GLIBC_* symbol version. `make rpm PORTABLE=0` opts out and
# packages a host build instead (links whatever libnfs pkg-config
# finds — do not ship those).
#
# The stage dir must hold the pinned archive from the lock file:
#   libnfs.a    both native consumers link this exact static archive
# Default location is packaging/libnfs-stage (gitignored); override
# with LIBNFS_STAGE=/path.
#
# Requires: rpmbuild (`rpm`), dpkg-deb (`deb`), and for portable
# builds cargo-zigbuild + zig (versions pinned in
# packaging/release-toolchain.lock.json and checked by `toolchain-check`).
# `release` also needs podman for the package install smoke tests.

VERSION  ?= $(shell sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -n1)

GLIBC        := 2.34
TRIPLE       := x86_64-unknown-linux-gnu
LIBNFS_STAGE ?= $(CURDIR)/packaging/libnfs-stage
LIBNFS_LOCK  := packaging/libnfs.lock.json
LIBNFS_SOURCE ?= $(abspath ../libnfs)
NFS_WALKER_SOURCE ?= $(abspath ../nfs-walker)
ZIG ?= $(shell if [ -x /snap/zig/current/zig ]; then echo /snap/zig/current/zig; else command -v zig; fi)

PORTABLE ?= 1
ifeq ($(PORTABLE),1)
BIN          := target/$(TRIPLE)/release/mongoose
BUILD_RULE   := build-portable
BUILD_TRIPLE := $(TRIPLE)
else
BIN          := target/release/mongoose
BUILD_RULE   := build
BUILD_TRIPLE := $(shell rustc -vV | sed -n 's/^host: //p')
endif

# Package and asset architecture labels come from the triple the packaged
# binary was built for, never from the machine that happens to run make.
TARGET_ARCH := $(firstword $(subst -, ,$(BUILD_TRIPLE)))
RPM_ARCH    := $(TARGET_ARCH)
DEB_ARCH    := $(if $(filter x86_64,$(TARGET_ARCH)),amd64,$(if $(filter aarch64,$(TARGET_ARCH)),arm64,$(error no Debian architecture for $(TARGET_ARCH))))

PKGDIR   := packaging
DIST     := dist

RPM_OUT  := $(DIST)/mongoose-$(VERSION)-1.$(RPM_ARCH).rpm
DEB_OUT  := $(DIST)/mongoose_$(VERSION)-1_$(DEB_ARCH).deb
TAR_OUT  := $(DIST)/mongoose-$(VERSION)-linux-$(TARGET_ARCH).tar.gz
# Unversioned on purpose: the README's install snippet fetches it via
# releases/latest/download/, which needs a stable asset name.
BIN_OUT  := $(DIST)/mongoose-linux-$(TARGET_ARCH)
SUMS_OUT := $(DIST)/SHA256SUMS
SBOM_OUT := $(DIST)/mongoose-$(VERSION)-sbom.cdx.json
SOURCE_OUT := $(DIST)/mongoose-$(VERSION)-source.tar.gz
RELINK_OUT := $(DIST)/mongoose-$(VERSION)-relink-kit.tar.gz
LIBNFS_SHORT := $(shell jq -r .source_git_sha $(LIBNFS_LOCK) | cut -c1-12)
LIBNFS_SOURCE_OUT := $(DIST)/libnfs-$(LIBNFS_SHORT)-source.tar.gz
COMPLIANCE_ASSETS := \
	$(SOURCE_OUT) $(RELINK_OUT) $(LIBNFS_SOURCE_OUT) \
	$(DIST)/LICENSES.txt $(DIST)/THIRD_PARTY_LICENSES.md \
	$(DIST)/LIBNFS_SOURCE.md $(DIST)/RELINK-VERIFICATION.txt \
	$(DIST)/LICENSE-MIT $(DIST)/LICENSE-LGPL-2.1.txt \
	$(DIST)/LICENSE-BSD-2-Clause-libnfs.txt

.PHONY: all build build-portable toolchain-check libnfs-stage stage-check compliance-check release-materials rpm deb tarball binary release clean

all: build

build:
	cargo build --release -p mongoose

# --- portable build --------------------------------------------------
# Both env vars point at the stage: VAMOOSE_LIBNFS_DIR for the mover's
# build.rs, NFS_WALKER_LIBNFS_DIR for the embedded walker's. After the
# build, refuse any binary whose glibc requirement exceeds $(GLIBC).
build-portable: toolchain-check stage-check
	VAMOOSE_LIBNFS_DIR=$(LIBNFS_STAGE) \
	NFS_WALKER_LIBNFS_DIR=$(LIBNFS_STAGE) \
	CARGO_ZIGBUILD_ZIG_PATH=$(ZIG) \
	ZIG_GLOBAL_CACHE_DIR=$(CURDIR)/target/zig-global-cache \
	ZIG_LOCAL_CACHE_DIR=$(CURDIR)/target/zig-local-cache \
	cargo zigbuild --release --target $(TRIPLE).$(GLIBC) -p mongoose
	@max=$$(objdump -T target/$(TRIPLE)/release/mongoose \
		| grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -n1); \
	echo "max glibc symbol: $$max (ceiling GLIBC_$(GLIBC))"; \
	highest=$$(printf '%s\nGLIBC_%s\n' "$$max" "$(GLIBC)" | sort -V | tail -n1); \
	if [ "$$highest" != "GLIBC_$(GLIBC)" ]; then \
		echo "ERROR: binary requires $$max, exceeds GLIBC_$(GLIBC) — not portable"; \
		exit 1; \
	fi

# Every tool version in packaging/release-toolchain.lock.json, verified.
toolchain-check:
	ZIG=$(ZIG) ./scripts/check-release-toolchain.sh

libnfs-stage:
	mkdir -p $(LIBNFS_STAGE)
	ZIG=$(ZIG) \
	EXPECTED_ZIG_VERSION=$$(jq -r .zig packaging/release-toolchain.lock.json) \
	EXPECTED_LIBNFS_SHA256=$$(jq -r .static_artifact_sha256 $(LIBNFS_LOCK)) \
	./scripts/build-libnfs-static.sh --source "$(LIBNFS_SOURCE)" --output "$(LIBNFS_STAGE)"

stage-check:
	@[ -f "$(LIBNFS_STAGE)/libnfs.a" ] || { \
		echo "ERROR: $(LIBNFS_STAGE) must contain the pinned libnfs.a"; \
		echo "Build it from the source pinned in $(LIBNFS_LOCK) with"; \
		echo "  make libnfs-stage LIBNFS_SOURCE=/path/to/libnfs"; \
		echo "or point LIBNFS_STAGE at an existing stage."; \
		exit 1; }
	@want_a=$$(jq -r .static_artifact_sha256 $(LIBNFS_LOCK)); \
	got_a=$$(sha256sum "$(LIBNFS_STAGE)/libnfs.a" | cut -d' ' -f1); \
	[ "$$got_a" = "$$want_a" ] || { echo "ERROR: libnfs.a sha256 $$got_a != pinned $$want_a"; exit 1; }; \
	echo "libnfs stage verified against $(LIBNFS_LOCK)"

# Fast repository-policy check for local development and CI. The stricter
# release-mode invocation below validates the exact built artifact set.
compliance-check:
	./scripts/check-lgpl-compliance.sh --repo-only

$(DIST):
	mkdir -p $(DIST)

# Generate exact corresponding source, a vendored offline relink kit, license
# notices, and evidence from an actual modified-libnfs relink. The generator
# requires clean source trees at the pinned commits and fails before packaging.
release-materials: binary
	./scripts/build-lgpl-release-materials.sh \
		--release-dir "$(DIST)" \
		--version "$(VERSION)" \
		--binary "$(BIN_OUT)" \
		--libnfs-source "$(LIBNFS_SOURCE)" \
		--nfs-walker-source "$(NFS_WALKER_SOURCE)"

# --- RPM -------------------------------------------------------------
# The spec packages the SOURCES-staged mongoose + mongoose.1; version
# is injected with --define so the spec never drifts from Cargo.toml.
rpm: release-materials | $(DIST)
	rm -rf $(DIST)/rpmbuild
	mkdir -p $(DIST)/rpmbuild/SOURCES $(DIST)/rpmbuild/tmp
	cp $(BIN) $(PKGDIR)/mongoose.1 \
		$(DIST)/LICENSE-MIT $(DIST)/LICENSE-LGPL-2.1.txt \
		$(DIST)/LICENSE-BSD-2-Clause-libnfs.txt \
		$(DIST)/THIRD_PARTY_LICENSES.md $(DIST)/LIBNFS_SOURCE.md \
		$(DIST)/rpmbuild/SOURCES/
	rpmbuild -bb $(PKGDIR)/mongoose.spec \
		--define "_topdir $(CURDIR)/$(DIST)/rpmbuild" \
		--define "_tmppath $(CURDIR)/$(DIST)/rpmbuild/tmp" \
		--define "pkg_version $(VERSION)" \
		--target $(RPM_ARCH)
	cp $(DIST)/rpmbuild/RPMS/$(RPM_ARCH)/mongoose-$(VERSION)-1*.rpm $(RPM_OUT)

# --- DEB -------------------------------------------------------------
deb: release-materials | $(DIST)
	rm -rf $(DIST)/debroot
	install -D -m0755 $(BIN) $(DIST)/debroot/usr/bin/mongoose
	install -D -m0644 $(PKGDIR)/mongoose.1 $(DIST)/debroot/usr/share/man/man1/mongoose.1
	gzip -9n $(DIST)/debroot/usr/share/man/man1/mongoose.1
	install -D -m0644 $(PKGDIR)/copyright $(DIST)/debroot/usr/share/doc/mongoose/copyright
	install -D -m0644 $(DIST)/LICENSE-MIT $(DIST)/debroot/usr/share/doc/mongoose/LICENSE-MIT
	install -D -m0644 $(DIST)/LICENSE-LGPL-2.1.txt $(DIST)/debroot/usr/share/doc/mongoose/LICENSE-LGPL-2.1.txt
	install -D -m0644 $(DIST)/LICENSE-BSD-2-Clause-libnfs.txt $(DIST)/debroot/usr/share/doc/mongoose/LICENSE-BSD-2-Clause-libnfs.txt
	install -D -m0644 $(DIST)/THIRD_PARTY_LICENSES.md $(DIST)/debroot/usr/share/doc/mongoose/THIRD_PARTY_LICENSES.md
	install -D -m0644 $(DIST)/LIBNFS_SOURCE.md $(DIST)/debroot/usr/share/doc/mongoose/LIBNFS_SOURCE.md
	install -d $(DIST)/debroot/DEBIAN
	sed -e 's/@VERSION@/$(VERSION)-1/' -e 's/@ARCH@/$(DEB_ARCH)/' \
		$(PKGDIR)/deb-control.in > $(DIST)/debroot/DEBIAN/control
	dpkg-deb --build --root-owner-group $(DIST)/debroot $(DEB_OUT)

# --- tarball ---------------------------------------------------------
tarball: release-materials | $(DIST)
	rm -rf $(DIST)/tarroot
	install -D -m0755 $(BIN) $(DIST)/tarroot/mongoose
	install -D -m0644 $(PKGDIR)/mongoose.1 $(DIST)/tarroot/mongoose.1
	install -D -m0644 README.md $(DIST)/tarroot/README.md
	install -D -m0644 $(DIST)/LICENSE-MIT $(DIST)/tarroot/LICENSE-MIT
	install -D -m0644 $(DIST)/LICENSE-LGPL-2.1.txt $(DIST)/tarroot/LICENSE-LGPL-2.1.txt
	install -D -m0644 $(DIST)/LICENSE-BSD-2-Clause-libnfs.txt $(DIST)/tarroot/LICENSE-BSD-2-Clause-libnfs.txt
	install -D -m0644 $(DIST)/THIRD_PARTY_LICENSES.md $(DIST)/tarroot/THIRD_PARTY_LICENSES.md
	install -D -m0644 $(DIST)/LIBNFS_SOURCE.md $(DIST)/tarroot/LIBNFS_SOURCE.md
	tar -C $(DIST)/tarroot -czf $(TAR_OUT) .

# --- bare binary -----------------------------------------------------
binary: $(BUILD_RULE) | $(DIST)
	install -m0755 $(BIN) $(BIN_OUT)

# Order matters and nothing is rebuilt along the way: every artifact, then the
# SBOM, then SHA256SUMS over all of them, then both gates against those exact
# bytes.
release: release-materials rpm deb tarball binary
	./scripts/build-sbom.sh --binary "$(BIN_OUT)" --version "$(VERSION)" --output "$(SBOM_OUT)"
	cd $(DIST) && sha256sum \
		$(notdir $(RPM_OUT) $(DEB_OUT) $(TAR_OUT) $(BIN_OUT) $(SBOM_OUT) $(COMPLIANCE_ASSETS)) \
		> $(notdir $(SUMS_OUT))
	./scripts/check-lgpl-compliance.sh --release-dir "$(DIST)" --version "$(VERSION)" --binary "$(BIN_OUT)"
	./scripts/check-release-artifacts.sh --release-dir "$(DIST)" --version "$(VERSION)" \
		--binary "$(BIN_OUT)" --rpm "$(RPM_OUT)" --deb "$(DEB_OUT)" --tarball "$(TAR_OUT)" \
		--sbom "$(SBOM_OUT)"
	@echo
	@echo "release artifacts:"
	@ls -l $(RPM_OUT) $(DEB_OUT) $(TAR_OUT) $(BIN_OUT) $(SBOM_OUT) $(COMPLIANCE_ASSETS) $(SUMS_OUT)

clean:
	rm -rf $(DIST)
