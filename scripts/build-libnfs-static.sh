#!/usr/bin/env bash
set -euo pipefail

fail() {
    echo "libnfs static build failed: $*" >&2
    exit 1
}

source_dir=""
output_dir=""
while (($#)); do
    case "$1" in
        --source)
            test $# -ge 2 || fail "--source needs a value"
            source_dir="$2"
            shift 2
            ;;
        --output)
            test $# -ge 2 || fail "--output needs a value"
            output_dir="$2"
            shift 2
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

test -n "$source_dir" || fail "use --source DIR --output DIR"
test -n "$output_dir" || fail "use --source DIR --output DIR"
test -f "$source_dir/CMakeLists.txt" || fail "not a libnfs source tree: $source_dir"
test -f "$source_dir/LICENCE-LGPL-2.1.txt" || fail "libnfs LGPL text is missing"

source_dir=$(CDPATH='' cd -- "$source_dir" && pwd)
mkdir -p "$output_dir"
output_dir=$(CDPATH='' cd -- "$output_dir" && pwd)

zig_bin=${ZIG:-}
if test -z "$zig_bin" && test -x /snap/zig/current/zig; then
    zig_bin=/snap/zig/current/zig
elif test -z "$zig_bin"; then
    zig_bin=$(command -v zig || true)
fi
test -n "$zig_bin" || fail "Zig is required; set ZIG=/path/to/zig"

expected_zig=${EXPECTED_ZIG_VERSION:-0.16.0}
actual_zig=$($zig_bin version 2>/dev/null) || fail "cannot execute Zig at $zig_bin"
test "$actual_zig" = "$expected_zig" || fail "Zig $expected_zig required, found $actual_zig"
command -v cmake >/dev/null 2>&1 || fail "cmake is required"

# The libc and compiler headers come from Zig's lib directory, and their
# absolute paths land in the archive's debug info. Map it to a fixed name so
# the archive does not depend on where Zig is installed.
zig_lib_dir=$("$zig_bin" env 2>/dev/null | sed -n 's/^ *\.lib_dir = "\(.*\)",$/\1/p')
test -n "$zig_lib_dir" || fail "cannot read Zig's lib directory from '$zig_bin env'"
test -d "$zig_lib_dir" || fail "Zig's lib directory is missing: $zig_lib_dir"

build_root=$(mktemp -d "${TMPDIR:-/tmp}/mongoose-libnfs-build.XXXXXX")
cleanup() {
    rm -rf -- "$build_root"
}
trap cleanup EXIT
build_dir="$build_root/build"
case "$source_dir$build_dir$zig_lib_dir" in
    *[[:space:]]*) fail "source, build, and Zig lib paths must not contain whitespace" ;;
esac

# Use the archiver shipped with the pinned Zig toolchain so the archive format
# and index do not depend on the host distribution's binutils version. Small
# wrappers let CMake invoke Zig's `ar` and `ranlib` subcommands using its normal
# tool interface.
zig_ar="$build_root/zig-ar"
zig_ranlib="$build_root/zig-ranlib"
printf '#!/usr/bin/env bash\nexec %q ar "$@"\n' "$zig_bin" >"$zig_ar"
printf '#!/usr/bin/env bash\nexec %q ranlib "$@"\n' "$zig_bin" >"$zig_ranlib"
chmod 0755 "$zig_ar" "$zig_ranlib"

c_flags="-target x86_64-linux-gnu.2.34"
c_flags+=" -ffile-prefix-map=$source_dir=/usr/src/libnfs"
c_flags+=" -fdebug-prefix-map=$source_dir=/usr/src/libnfs"
c_flags+=" -fmacro-prefix-map=$source_dir=/usr/src/libnfs"
c_flags+=" -ffile-prefix-map=$build_dir=/usr/src/libnfs-build"
c_flags+=" -fdebug-prefix-map=$build_dir=/usr/src/libnfs-build"
c_flags+=" -fdebug-compilation-dir=/usr/src/libnfs-build"
c_flags+=" -ffile-prefix-map=$zig_lib_dir=/usr/lib/zig"
c_flags+=" -fdebug-prefix-map=$zig_lib_dir=/usr/lib/zig"

export ZIG_GLOBAL_CACHE_DIR="$build_root/zig-global-cache"
export ZIG_LOCAL_CACHE_DIR="$build_root/zig-local-cache"

cmake -S "$source_dir" -B "$build_dir" \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_SHARED_LIBS=OFF \
    -DENABLE_TESTS=OFF \
    -DENABLE_DOCUMENTATION=OFF \
    -DENABLE_UTILS=OFF \
    -DENABLE_EXAMPLES=OFF \
    -DENABLE_MULTITHREADING=OFF \
    -DCMAKE_C_COMPILER="$zig_bin" \
    -DCMAKE_C_COMPILER_ARG1=cc \
    "-DCMAKE_C_FLAGS=$c_flags" \
    -DCMAKE_AR="$zig_ar" \
    -DCMAKE_RANLIB="$zig_ranlib"
cmake --build "$build_dir" --parallel

install -m0644 "$build_dir/lib/libnfs.a" "$output_dir/libnfs.a"
actual_sha=$(sha256sum "$output_dir/libnfs.a" | cut -d' ' -f1)
if test -n "${EXPECTED_LIBNFS_SHA256:-}" && test "$actual_sha" != "$EXPECTED_LIBNFS_SHA256"; then
    fail "libnfs.a SHA-256 $actual_sha does not match $EXPECTED_LIBNFS_SHA256"
fi
echo "built $output_dir/libnfs.a ($actual_sha)"
