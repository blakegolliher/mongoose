#!/usr/bin/env bash
# Release artifact gate, run by `make release` after the LGPL gate and
# SHA256SUMS. It checks the exact files that would be published:
#
# - the bare binary: ELF x86-64, only libc/libm and the standard loader as
#   dynamic dependencies, glibc symbols no newer than the declared floor, and
#   working --version and --help;
# - SHA256SUMS: every file in the release directory is listed and verifies;
# - packages: the RPM installs, runs, and uninstalls cleanly on Rocky Linux 9
#   (glibc 2.34, the oldest supported platform), the DEB does the same on
#   Debian 12, and the tarball and bare binary run on both;
# - the AES-NI guard: the binary refuses to start on an emulated CPU without
#   AES-NI and starts on one with it.
#
# Container images are pinned by digest. Needs podman, plus network access to
# pull the images and qemu-user.
set -euo pipefail

fail() {
    echo "release artifact check failed: $*" >&2
    exit 1
}

# Rocky Linux 9.8 (glibc 2.34) and Debian 12 (glibc 2.36), multi-arch digests.
rocky_image=docker.io/rockylinux/rockylinux:9@sha256:8101994123cf3d0a8fee517bee7f39e555c7d92bd2d9eb3303cc988a0eeed00f
debian_image=docker.io/library/debian:12@sha256:f37a335e82bca302e955fa39f9dfe28f1be618f016f8a2b56318e5a5111afc26
glibc_floor=2.34
allowed_needed="libc.so.6 libm.so.6"
interpreter=/lib64/ld-linux-x86-64.so.2

release_dir=""
version=""
binary=""
rpm=""
deb=""
tarball=""
while (($#)); do
    test $# -ge 2 || fail "$1 needs a value"
    case "$1" in
        --release-dir) release_dir="$2" ;;
        --version) version="$2" ;;
        --binary) binary="$2" ;;
        --rpm) rpm="$2" ;;
        --deb) deb="$2" ;;
        --tarball) tarball="$2" ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift 2
done
test -d "$release_dir" || fail "--release-dir must name the release directory"
test -n "$version" || fail "--version is required"
for file in "$binary" "$rpm" "$deb" "$tarball"; do
    test -f "$file" || fail "missing artifact: ${file:-(unset)}"
done
for command_name in readelf objdump sha256sum podman; do
    command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done

echo "== bare binary: $binary"
header=$(readelf -h "$binary") || fail "$binary is not an ELF file"
grep -Eq 'Class:[[:space:]]+ELF64' <<<"$header" || fail "$binary is not ELF64"
grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64' <<<"$header" || fail "$binary is not x86-64"
program_headers=$(readelf -lW "$binary") || fail "cannot read program headers of $binary"
grep -Fq "[Requesting program interpreter: $interpreter]" <<<"$program_headers" \
    || fail "$binary does not use $interpreter"
dynamic=$(readelf -dW "$binary") || fail "cannot read dynamic section of $binary"
needed=$(sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\]/\1/p' <<<"$dynamic" | sort | paste -sd' ')
for library in $needed; do
    [[ " $allowed_needed " == *" $library "* ]] || fail "$binary needs $library (allowed: $allowed_needed)"
done
echo "  dynamic dependencies: ${needed:-none} (interpreter $interpreter)"
symbols=$(objdump -T "$binary") || fail "cannot read dynamic symbols of $binary"
max_glibc=$(grep -oE 'GLIBC_[0-9.]+' <<<"$symbols" | sort -Vu | tail -n1)
highest=$(printf '%s\nGLIBC_%s\n' "$max_glibc" "$glibc_floor" | sort -V | tail -n1)
test "$highest" = "GLIBC_$glibc_floor" || fail "$binary requires $max_glibc, newer than GLIBC_$glibc_floor"
echo "  max glibc symbol: $max_glibc (floor GLIBC_$glibc_floor)"
actual_version=$("$binary" --version) || fail "$binary --version failed"
test "$actual_version" = "mongoose $version" || fail "$binary --version printed '$actual_version', expected 'mongoose $version'"
help=$("$binary" --help) || fail "$binary --help failed"
for command_name in copy sync; do
    grep -Eq "^[[:space:]]+${command_name}[[:space:]]" <<<"$help" || fail "$binary --help does not list '$command_name'"
done
echo "  --version: $actual_version; --help lists copy and sync"

echo "== SHA256SUMS"
sums="$release_dir/SHA256SUMS"
test -f "$sums" || fail "missing $sums"
(cd "$release_dir" && sha256sum --check --strict --quiet SHA256SUMS) || fail "SHA256SUMS does not verify"
listed=$(sed -E 's/^[0-9a-f]{64}  //' "$sums" | sort)
present=$(find "$release_dir" -maxdepth 1 -type f ! -name SHA256SUMS -printf '%f\n' | sort)
test "$listed" = "$present" || {
    diff <(echo "$listed") <(echo "$present") >&2 || true
    fail "SHA256SUMS does not list exactly the files in $release_dir (build into a clean DIST)"
}
echo "  $(wc -l <<<"$listed") assets listed and verified"

# Run SCRIPT in IMAGE with the artifacts mounted read-only under /pkg. NETWORK
# is "none" or "default".
run_in() {
    local image="$1"
    local network="$2"
    local script="$3"
    local network_args=()
    test "$network" = default || network_args=(--network="$network")
    podman run --rm "${network_args[@]}" \
        -v "$(realpath "$binary")":/pkg/mongoose-bin:ro \
        -v "$(realpath "$rpm")":/pkg/mongoose.rpm:ro \
        -v "$(realpath "$deb")":/pkg/mongoose.deb:ro \
        -v "$(realpath "$tarball")":/pkg/mongoose.tar.gz:ro \
        -e VERSION="$version" \
        "$image" bash -euo pipefail -c "$script"
}

# Shared by both distributions: the installed command, its notices and man
# page, and the tarball and bare binary. Single-quoted on purpose: it expands
# inside the container.
# shellcheck disable=SC2016
smoke='
test "$(mongoose --version)" = "mongoose $VERSION"
mongoose --help >/dev/null
licenses=$(mongoose licenses --component libnfs)
grep -q "LGPL-2.1-or-later" <<<"$licenses"
test -f /usr/share/man/man1/mongoose.1.gz
for doc in LICENSE-MIT LICENSE-LGPL-2.1.txt LICENSE-BSD-2-Clause-libnfs.txt THIRD_PARTY_LICENSES.md LIBNFS_SOURCE.md; do
    test -n "$(find /usr/share/doc /usr/share/licenses -path "*mongoose*/$doc" -print -quit 2>/dev/null)"
done
test "$(/pkg/mongoose-bin --version)" = "mongoose $VERSION"
mkdir /tmp/tarball && tar -xzf /pkg/mongoose.tar.gz -C /tmp/tarball
test "$(/tmp/tarball/mongoose --version)" = "mongoose $VERSION"
'

echo "== RPM on Rocky Linux 9 (glibc 2.34)"
run_in "$rocky_image" none "
ldd --version | sed -n 1p
rpm -i /pkg/mongoose.rpm
$smoke
rpm -e mongoose
! rpm -q mongoose >/dev/null
! command -v mongoose >/dev/null
test ! -e /usr/bin/mongoose
echo '  installed, ran, and uninstalled cleanly'
" || fail "RPM smoke test failed on Rocky Linux 9"

echo "== DEB on Debian 12 (glibc 2.36)"
run_in "$debian_image" none "
ldd --version | sed -n 1p
dpkg -i /pkg/mongoose.deb >/dev/null
$smoke
dpkg -r mongoose >/dev/null
! dpkg -s mongoose >/dev/null 2>&1
! command -v mongoose >/dev/null
test ! -e /usr/bin/mongoose
echo '  installed, ran, and uninstalled cleanly'
" || fail "DEB smoke test failed on Debian 12"

echo "== AES-NI guard under qemu-user (Nehalem: no AES-NI; Westmere: AES-NI)"
# shellcheck disable=SC2016
run_in "$debian_image" default '
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null
apt-get install -y -qq --no-install-recommends qemu-user >/dev/null 2>&1
qemu-x86_64 --version | sed -n 1p
if output=$(qemu-x86_64 -cpu Nehalem /pkg/mongoose-bin --version 2>&1); then
    echo "  started on a CPU without AES-NI: $output" >&2
    exit 1
else
    status=$?
fi
test "$status" -eq 1
grep -q "AES-NI" <<<"$output"
echo "  Nehalem: refused with status $status: $output"
test "$(qemu-x86_64 -cpu Westmere /pkg/mongoose-bin --version)" = "mongoose $VERSION"
echo "  Westmere: mongoose $VERSION"
' || fail "AES-NI guard check failed"

echo "release artifact check passed for mongoose $version"
