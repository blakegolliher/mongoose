#!/usr/bin/env bash
set -euo pipefail

fail() {
    echo "LGPL COMPLIANCE BLOCK: $*" >&2
    exit 1
}

need_file() {
    test -f "$1" || fail "required file is missing: $1"
}

need_text() {
    local path="$1"
    local pattern="$2"
    grep -Eq -- "$pattern" "$path" || fail "$path does not contain required policy text: $pattern"
}

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

mode=""
release_dir=""
version=""
binary=""
while (($#)); do
    case "$1" in
        --repo-only)
            mode="repo"
            shift
            ;;
        --release-dir)
            test $# -ge 2 || fail "--release-dir needs a value"
            mode="release"
            release_dir="$2"
            shift 2
            ;;
        --version)
            test $# -ge 2 || fail "--version needs a value"
            version="$2"
            shift 2
            ;;
        --binary)
            test $# -ge 2 || fail "--binary needs a value"
            binary="$2"
            shift 2
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

test -n "$mode" || fail "use --repo-only or --release-dir DIR --version VERSION --binary PATH"

for path in \
    AGENTS.md \
    about.toml \
    docs/LGPL_COMPLIANCE.md \
    docs/CORRECTNESS_RULES.md \
    packaging/libnfs.lock.json \
    packaging/nfs-walker.lock.json \
    packaging/licenses/LGPL-2.1.txt \
    packaging/licenses/BSD-2-Clause-libnfs.txt \
    packaging/relink-kit/verify-relink.sh \
    scripts/build-libnfs-static.sh \
    scripts/build-lgpl-release-materials.sh \
    scripts/check-cargo-source-map.sh; do
    need_file "$path"
done

command -v jq >/dev/null 2>&1 || fail "jq is required to validate packaging/libnfs.lock.json"
libnfs_sha=$(jq -er '.source_git_sha' packaging/libnfs.lock.json) || fail "libnfs source_git_sha is missing"
libnfs_static_sha=$(jq -er '.static_artifact_sha256' packaging/libnfs.lock.json) || fail "libnfs static digest is missing"
libnfs_license=$(jq -er '.license' packaging/libnfs.lock.json) || fail "libnfs license is missing"
libnfs_linkage=$(jq -er '.release_linkage' packaging/libnfs.lock.json) || fail "libnfs release linkage is missing"
libnfs_schema=$(jq -er '.schema_version' packaging/libnfs.lock.json) || fail "libnfs schema version is missing"
nfs_walker_sha=$(jq -er '.source_git_sha' packaging/nfs-walker.lock.json) || fail "nfs-walker source_git_sha is missing"
[[ "$libnfs_sha" =~ ^[0-9a-f]{40}$ ]] || fail "libnfs source_git_sha must be 40 lowercase hex characters"
[[ "$libnfs_static_sha" =~ ^[0-9a-f]{64}$ ]] || fail "libnfs static digest must be 64 lowercase hex characters"
[[ "$nfs_walker_sha" =~ ^[0-9a-f]{40}$ ]] || fail "nfs-walker source_git_sha must be 40 lowercase hex characters"
test "$libnfs_license" = "LGPL-2.1-or-later" || fail "unexpected libnfs license: $libnfs_license"
test "$libnfs_linkage" = "static" || fail "release linkage does not match the owner-approved static policy"
test "$libnfs_schema" = "2" || fail "libnfs lock must use the static-only schema version 2"

need_text AGENTS.md 'Never publish.*releasable'
need_text docs/CORRECTNESS_RULES.md 'intentionally statically linked'
need_text docs/LGPL_COMPLIANCE.md 'No mongoose binary containing libnfs may be published'
need_text Makefile 'check-lgpl-compliance\.sh --release-dir'
need_text Makefile 'build-lgpl-release-materials\.sh'
need_text crates/migration-mover/build.rs 'rustc-link-lib=static=nfs'
need_text packaging/mongoose.spec '^%define __brp_strip %\{nil\}$'
need_text packaging/mongoose.spec '^%define __brp_strip_static_archive %\{nil\}$'
need_text packaging/mongoose.spec '^%define __brp_strip_comment_note %\{nil\}$'
need_text packaging/licenses/LGPL-2.1.txt 'GNU (LESSER|LIBRARY) GENERAL PUBLIC LICENSE'

if test "$mode" = "repo"; then
    echo "LGPL repository policy check passed (static libnfs $libnfs_sha)"
    exit 0
fi

test -n "$release_dir" || fail "release mode requires --release-dir"
test -n "$version" || fail "release mode requires --version"
test -n "$binary" || fail "release mode requires --binary"
test -d "$release_dir" || fail "release directory is missing: $release_dir"
need_file "$binary"

sha_short=${libnfs_sha:0:12}
source_bundle="$release_dir/mongoose-$version-source.tar.gz"
relink_bundle="$release_dir/mongoose-$version-relink-kit.tar.gz"
libnfs_bundle="$release_dir/libnfs-$sha_short-source.tar.gz"
for path in \
    "$source_bundle" \
    "$relink_bundle" \
    "$libnfs_bundle" \
    "$release_dir/LICENSES.txt" \
    "$release_dir/THIRD_PARTY_LICENSES.md" \
    "$release_dir/LIBNFS_SOURCE.md" \
    "$release_dir/RELINK-VERIFICATION.txt" \
    "$release_dir/LICENSE-MIT" \
    "$release_dir/LICENSE-LGPL-2.1.txt" \
    "$release_dir/LICENSE-BSD-2-Clause-libnfs.txt"; do
    need_file "$path"
done

need_text "$release_dir/LICENSES.txt" 'LGPL-2\.1-or-later'
need_text "$release_dir/LICENSES.txt" "$libnfs_sha"
need_text "$release_dir/LICENSES.txt" 'GNU (LESSER|LIBRARY) GENERAL PUBLIC LICENSE'
need_text "$release_dir/THIRD_PARTY_LICENSES.md" 'libnfs'
need_text "$release_dir/THIRD_PARTY_LICENSES.md" 'LGPL-2\.1-or-later'
need_text "$release_dir/THIRD_PARTY_LICENSES.md" "$libnfs_sha"
need_text "$release_dir/LIBNFS_SOURCE.md" "$libnfs_sha"
need_text "$release_dir/LIBNFS_SOURCE.md" 'relink'
need_text "$release_dir/RELINK-VERIFICATION.txt" "$libnfs_sha"
need_text "$release_dir/RELINK-VERIFICATION.txt" 'PASS'
need_text "$release_dir/RELINK-VERIFICATION.txt" 'isolated, empty CARGO_HOME'

command -v readelf >/dev/null 2>&1 || fail "readelf is required for the release artifact check"
dynamic_section=$(readelf -d "$binary" 2>/dev/null) || fail "cannot inspect dynamic dependencies for $binary"
if grep -q 'Shared library: \[libnfs' <<<"$dynamic_section"; then
    fail "$binary dynamically links libnfs but the approved release model is static"
fi

license_output=$($binary licenses --component libnfs 2>&1) || fail "$binary does not provide the required offline libnfs license command"
grep -Fq 'LGPL-2.1-or-later' <<<"$license_output" || fail "binary license output omits LGPL-2.1-or-later"
grep -Fq "$libnfs_sha" <<<"$license_output" || fail "binary license output omits exact libnfs source revision"
grep -Eq 'statically linked|static linking' <<<"$license_output" || fail "binary license output omits static-link notice"
grep -Eq 'GNU (LESSER|LIBRARY) GENERAL PUBLIC LICENSE' <<<"$license_output" || fail "binary license output omits the LGPL text"
grep -Fq "mongoose-$version-relink-kit.tar.gz" <<<"$license_output" || fail "binary license output omits the exact relink-kit asset"
grep -Fq 'END OF TERMS AND CONDITIONS' <<<"$license_output" || fail "binary license output truncates the LGPL text"

binary_sha=$(sha256sum "$binary" | cut -d' ' -f1)
grep -Fq "Official mongoose SHA-256: $binary_sha" "$release_dir/RELINK-VERIFICATION.txt" \
    || fail "relink evidence does not identify the release binary"

source_listing=$(tar -tzf "$source_bundle") || fail "cannot list source archive: $source_bundle"
relink_listing=$(tar -tzf "$relink_bundle") || fail "cannot list relink archive: $relink_bundle"
libnfs_listing=$(tar -tzf "$libnfs_bundle") || fail "cannot list libnfs source archive: $libnfs_bundle"
grep -Eq '(^|/)Cargo\.lock$' <<<"$source_listing" || fail "mongoose source bundle omits Cargo.lock"
grep -Eq '(^|/)packaging/libnfs\.lock\.json$' <<<"$source_listing" || fail "mongoose source bundle omits libnfs lock"
grep -Eq "(^|/)third-party/nfs-walker-${nfs_walker_sha:0:12}/Cargo\.toml$" <<<"$source_listing" \
    || fail "mongoose source bundle omits exact nfs-walker source"
grep -Eq '(^|/)SOURCE-PROVENANCE\.txt$' <<<"$source_listing" || fail "mongoose source bundle omits provenance"
grep -Eq '(^|/)RELINKING\.md$' <<<"$relink_listing" || fail "relink kit omits RELINKING.md"
grep -Eq '(^|/)build-libnfs-static\.sh$' <<<"$relink_listing" || fail "relink kit omits libnfs build script"
grep -Eq '(^|/)verify-relink\.sh$' <<<"$relink_listing" || fail "relink kit omits verification script"
grep -Eq '(^|/)mongoose/vendor/[^/]+/Cargo\.toml$' <<<"$relink_listing" || fail "relink kit omits vendored Rust dependencies"
grep -Eq '(^|/)mongoose/Cargo\.lock$' <<<"$relink_listing" || fail "relink kit omits mongoose Cargo.lock"
grep -Eq '(^|/)libnfs/CMakeLists\.txt$' <<<"$relink_listing" || fail "relink kit omits libnfs source"

# The packaged kit must build from its own vendor/: its source map must cover
# every locked source and package, and it must resolve with an empty Cargo
# home (no registry index, crate cache, or git checkout) and no network.
command -v cargo >/dev/null 2>&1 || fail "cargo is required to resolve the packaged relink kit"
kit_check=$(mktemp -d "${TMPDIR:-/tmp}/mongoose-kit-check.XXXXXX")
trap 'rm -rf -- "$kit_check"' EXIT
tar -xzf "$relink_bundle" -C "$kit_check" || fail "cannot extract relink kit: $relink_bundle"
kit_mongoose="$kit_check/mongoose-$version-relink-kit/mongoose"
test -d "$kit_mongoose" || fail "relink kit is not rooted at mongoose-$version-relink-kit/"
./scripts/check-cargo-source-map.sh "$kit_mongoose" \
    || fail "packaged relink kit has an incomplete offline source map"
(cd "$kit_mongoose" && CARGO_HOME="$kit_check/empty-cargo-home" CARGO_NET_OFFLINE=true \
    cargo metadata --locked --offline --format-version 1 >/dev/null) \
    || fail "packaged relink kit does not resolve offline from its vendored sources"
grep -Eq '(^|/)LICEN[CS]E-LGPL-2\.1\.txt$' <<<"$libnfs_listing" || fail "libnfs source bundle omits LGPL 2.1 text"
grep -Eq '(^|/)LICEN[CS]E-BSD\.txt$' <<<"$libnfs_listing" || fail "libnfs source bundle omits BSD text"
grep -Eq '(^|/)COPYING$' <<<"$libnfs_listing" || fail "libnfs source bundle omits its license map"

shopt -s nullglob
runtime_tars=("$release_dir"/mongoose-"$version"-linux-*.tar.gz)
rpms=("$release_dir"/mongoose-"$version"-*.rpm)
debs=("$release_dir"/mongoose_"$version"-*.deb)
test ${#runtime_tars[@]} -eq 1 || fail "expected exactly one runtime tarball for mongoose $version"
test ${#rpms[@]} -eq 1 || fail "expected exactly one RPM for mongoose $version"
test ${#debs[@]} -eq 1 || fail "expected exactly one DEB for mongoose $version"

runtime_listing=$(tar -tzf "${runtime_tars[0]}") || fail "cannot list runtime tarball"
for required in LICENSE-MIT LICENSE-LGPL-2.1.txt LICENSE-BSD-2-Clause-libnfs.txt THIRD_PARTY_LICENSES.md LIBNFS_SOURCE.md; do
    grep -Eq "(^|/)$required$" <<<"$runtime_listing" || fail "runtime tarball omits $required"
done
runtime_binary_entry=$(grep -E '(^|/)mongoose$' <<<"$runtime_listing" | head -n1)
test -n "$runtime_binary_entry" || fail "runtime tarball omits mongoose binary"
runtime_binary_sha=$(tar -xOzf "${runtime_tars[0]}" "$runtime_binary_entry" | sha256sum | cut -d' ' -f1)
test "$runtime_binary_sha" = "$binary_sha" || fail "runtime tarball binary differs from bare binary"

command -v rpm >/dev/null 2>&1 || fail "rpm is required to inspect the release package"
rpm_listing=$(rpm -qpl "${rpms[0]}") || fail "cannot list RPM contents"
for required in LICENSE-MIT LICENSE-LGPL-2.1.txt LICENSE-BSD-2-Clause-libnfs.txt THIRD_PARTY_LICENSES.md LIBNFS_SOURCE.md; do
    grep -Eq "/$required$" <<<"$rpm_listing" || fail "RPM omits $required"
done
rpm_license=$(rpm -qp --qf '%{LICENSE}' "${rpms[0]}") || fail "cannot read RPM license metadata"
grep -Fq 'LGPL-2.1-or-later' <<<"$rpm_license" || fail "RPM license metadata omits LGPL-2.1-or-later"
rpm_binary_sha=$(rpm -qp --dump "${rpms[0]}" | awk '$1 == "/usr/bin/mongoose" { print $4 }')
test "$rpm_binary_sha" = "$binary_sha" || fail "RPM binary differs from bare binary"

command -v dpkg-deb >/dev/null 2>&1 || fail "dpkg-deb is required to inspect the release package"
deb_listing=$(dpkg-deb --contents "${debs[0]}") || fail "cannot list DEB contents"
for required in LICENSE-MIT LICENSE-LGPL-2.1.txt LICENSE-BSD-2-Clause-libnfs.txt THIRD_PARTY_LICENSES.md LIBNFS_SOURCE.md copyright; do
    grep -Eq "/$required$" <<<"$deb_listing" || fail "DEB omits $required"
done
deb_binary_sha=$(dpkg-deb --fsys-tarfile "${debs[0]}" | tar -xOf - ./usr/bin/mongoose | sha256sum | cut -d' ' -f1)
test "$deb_binary_sha" = "$binary_sha" || fail "DEB binary differs from bare binary"

echo "LGPL release artifact check passed for mongoose $version (static libnfs $libnfs_sha)"
