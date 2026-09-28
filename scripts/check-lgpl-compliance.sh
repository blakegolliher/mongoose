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
    docs/LGPL_COMPLIANCE.md \
    docs/CORRECTNESS_RULES.md \
    packaging/libnfs.lock.json \
    packaging/licenses/LGPL-2.1.txt; do
    need_file "$path"
done

command -v jq >/dev/null 2>&1 || fail "jq is required to validate packaging/libnfs.lock.json"
libnfs_sha=$(jq -er '.source_git_sha' packaging/libnfs.lock.json) || fail "libnfs source_git_sha is missing"
libnfs_static_sha=$(jq -er '.static_artifact_sha256' packaging/libnfs.lock.json) || fail "libnfs static digest is missing"
libnfs_license=$(jq -er '.license' packaging/libnfs.lock.json) || fail "libnfs license is missing"
libnfs_linkage=$(jq -er '.release_linkage' packaging/libnfs.lock.json) || fail "libnfs release linkage is missing"
[[ "$libnfs_sha" =~ ^[0-9a-f]{40}$ ]] || fail "libnfs source_git_sha must be 40 lowercase hex characters"
[[ "$libnfs_static_sha" =~ ^[0-9a-f]{64}$ ]] || fail "libnfs static digest must be 64 lowercase hex characters"
test "$libnfs_license" = "LGPL-2.1-or-later" || fail "unexpected libnfs license: $libnfs_license"
test "$libnfs_linkage" = "static" || fail "release linkage does not match the owner-approved static policy"

need_text AGENTS.md 'Never publish.*releasable'
need_text docs/CORRECTNESS_RULES.md 'intentionally statically linked'
need_text docs/LGPL_COMPLIANCE.md 'No mongoose binary containing libnfs may be published'
need_text Makefile 'check-lgpl-compliance\.sh --release-dir'
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
    "$release_dir/RELINK-VERIFICATION.txt"; do
    need_file "$path"
done

need_text "$release_dir/LICENSES.txt" 'LGPL-2\.1-or-later'
need_text "$release_dir/LICENSES.txt" "$libnfs_sha"
need_text "$release_dir/THIRD_PARTY_LICENSES.md" 'libnfs'
need_text "$release_dir/THIRD_PARTY_LICENSES.md" 'LGPL-2\.1-or-later'
need_text "$release_dir/LIBNFS_SOURCE.md" "$libnfs_sha"
need_text "$release_dir/LIBNFS_SOURCE.md" 'relink'
need_text "$release_dir/RELINK-VERIFICATION.txt" "$libnfs_sha"
need_text "$release_dir/RELINK-VERIFICATION.txt" 'PASS'

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

source_listing=$(tar -tzf "$source_bundle") || fail "cannot list source archive: $source_bundle"
relink_listing=$(tar -tzf "$relink_bundle") || fail "cannot list relink archive: $relink_bundle"
libnfs_listing=$(tar -tzf "$libnfs_bundle") || fail "cannot list libnfs source archive: $libnfs_bundle"
grep -Eq '(^|/)Cargo\.lock$' <<<"$source_listing" || fail "mongoose source bundle omits Cargo.lock"
grep -Eq '(^|/)packaging/libnfs\.lock\.json$' <<<"$source_listing" || fail "mongoose source bundle omits libnfs lock"
grep -Eq '(^|/)RELINKING\.md$' <<<"$relink_listing" || fail "relink kit omits RELINKING.md"
grep -Eq '(^|/)LICEN[CS]E-LGPL-2\.1\.txt$' <<<"$libnfs_listing" || fail "libnfs source bundle omits LGPL 2.1 text"

shopt -s nullglob
runtime_tars=("$release_dir"/mongoose-"$version"-linux-*.tar.gz)
rpms=("$release_dir"/mongoose-"$version"-*.rpm)
debs=("$release_dir"/mongoose_"$version"-*.deb)
test ${#runtime_tars[@]} -eq 1 || fail "expected exactly one runtime tarball for mongoose $version"
test ${#rpms[@]} -eq 1 || fail "expected exactly one RPM for mongoose $version"
test ${#debs[@]} -eq 1 || fail "expected exactly one DEB for mongoose $version"

runtime_listing=$(tar -tzf "${runtime_tars[0]}") || fail "cannot list runtime tarball"
for required in LICENSE-MIT LICENSE-LGPL-2.1.txt THIRD_PARTY_LICENSES.md LIBNFS_SOURCE.md; do
    grep -Eq "(^|/)$required$" <<<"$runtime_listing" || fail "runtime tarball omits $required"
done

command -v rpm >/dev/null 2>&1 || fail "rpm is required to inspect the release package"
rpm_listing=$(rpm -qpl "${rpms[0]}") || fail "cannot list RPM contents"
for required in LICENSE-MIT LICENSE-LGPL-2.1.txt THIRD_PARTY_LICENSES.md LIBNFS_SOURCE.md; do
    grep -Eq "/$required$" <<<"$rpm_listing" || fail "RPM omits $required"
done

command -v dpkg-deb >/dev/null 2>&1 || fail "dpkg-deb is required to inspect the release package"
deb_listing=$(dpkg-deb --contents "${debs[0]}") || fail "cannot list DEB contents"
for required in LICENSE-MIT LICENSE-LGPL-2.1.txt THIRD_PARTY_LICENSES.md LIBNFS_SOURCE.md; do
    grep -Eq "/$required$" <<<"$deb_listing" || fail "DEB omits $required"
done

echo "LGPL release artifact check passed for mongoose $version (static libnfs $libnfs_sha)"
