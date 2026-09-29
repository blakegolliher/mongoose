#!/usr/bin/env bash
set -euo pipefail

fail() {
    echo "LGPL relink verification failed: $*" >&2
    exit 1
}

kit_root=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
original=""
evidence=""
while (($#)); do
    case "$1" in
        --original)
            test $# -ge 2 || fail "--original needs a value"
            original="$2"
            shift 2
            ;;
        --evidence)
            test $# -ge 2 || fail "--evidence needs a value"
            evidence="$2"
            shift 2
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

test -f "$original" || fail "use --original /path/to/mongoose-linux-x86_64"
test -n "$evidence" || fail "use --evidence /path/to/RELINK-VERIFICATION.txt"
test -f "$kit_root/mongoose/Cargo.lock" || fail "kit omits mongoose source"
test -d "$kit_root/mongoose/vendor" || fail "kit omits vendored Cargo dependencies"
test -f "$kit_root/libnfs/lib/libnfs.c" || fail "kit omits libnfs source"

for command_name in cargo cargo-zigbuild cmake grep jq readelf sha256sum; do
    command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done

zig_bin=${ZIG:-}
if test -z "$zig_bin" && test -x /snap/zig/current/zig; then
    zig_bin=/snap/zig/current/zig
elif test -z "$zig_bin"; then
    zig_bin=$(command -v zig || true)
fi
test -n "$zig_bin" || fail "Zig is required; set ZIG=/path/to/zig"
export ZIG="$zig_bin"

work_root=$(mktemp -d "${TMPDIR:-/tmp}/mongoose-relink-proof.XXXXXX")
cleanup() {
    rm -rf -- "$work_root"
}
trap cleanup EXIT

cp -a "$kit_root/libnfs" "$work_root/libnfs"
marker='mongoose LGPL relink proof'
sed -i \
    's/static const char \*oom = "out of memory";/static const char *oom = "out of memory [mongoose LGPL relink proof]";/' \
    "$work_root/libnfs/lib/init.c"
grep -Fq "$marker" "$work_root/libnfs/lib/init.c" || fail "could not apply proof modification"

"$kit_root/build-libnfs-static.sh" \
    --source "$work_root/libnfs" \
    --output "$work_root/modified-stage"
modified_lib_sha=$(sha256sum "$work_root/modified-stage/libnfs.a" | cut -d' ' -f1)

export VAMOOSE_LIBNFS_DIR="$work_root/modified-stage"
export NFS_WALKER_LIBNFS_DIR="$work_root/modified-stage"
export CARGO_NET_OFFLINE=true
export CARGO_ZIGBUILD_ZIG_PATH="$zig_bin"
export CARGO_ZIGBUILD_CACHE_DIR="$work_root/cargo-zigbuild-cache"
export ZIG_GLOBAL_CACHE_DIR="$work_root/zig-global-cache"
export ZIG_LOCAL_CACHE_DIR="$work_root/zig-local-cache"
# An empty Cargo home has no registry index, crate cache, or git checkout to
# fall back on, so the build can only use vendor/ through the kit's source map.
export CARGO_HOME="$work_root/cargo-home"
mkdir -p "$CARGO_HOME"

(
    cd "$kit_root/mongoose"
    CARGO_TARGET_DIR="$work_root/target-smoke" \
        cargo test --locked --offline -p mongoose --lib
    CARGO_TARGET_DIR="$work_root/target-portable" \
        cargo zigbuild --release --locked --offline \
            --target x86_64-unknown-linux-gnu.2.34 -p mongoose
)

candidate="$work_root/target-portable/x86_64-unknown-linux-gnu/release/mongoose"
test -x "$candidate" || fail "portable replacement binary was not produced"
# No pipelines here: under pipefail, `strings | grep -q` fails whenever grep
# stops reading before strings finishes (SIGPIPE), even on a match.
LC_ALL=C grep -Faq -- "$marker" "$candidate" || fail "modified libnfs marker is absent from replacement binary"
dynamic_section=$(readelf -d "$candidate") || fail "cannot inspect dynamic dependencies of the replacement binary"
if grep -q 'Shared library: \[libnfs' <<<"$dynamic_section"; then
    fail "replacement binary unexpectedly depends on shared libnfs"
fi
"$candidate" licenses --component libnfs >/dev/null
"$candidate" --help >/dev/null

original_sha=$(sha256sum "$original" | cut -d' ' -f1)
candidate_sha=$(sha256sum "$candidate" | cut -d' ' -f1)
test "$candidate_sha" != "$original_sha" || fail "replacement binary is identical to the official binary"

libnfs_sha=$(jq -er .source_git_sha "$kit_root/mongoose/packaging/libnfs.lock.json")
mongoose_version=$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' "$kit_root/mongoose/Cargo.toml" | head -n1)
verified_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')

evidence_tmp="$evidence.tmp"
cat >"$evidence_tmp" <<EOF
mongoose $mongoose_version LGPL static-relink verification
Status: PASS
Verified at: $verified_at
libnfs source revision: $libnfs_sha
Modified libnfs.a SHA-256: $modified_lib_sha
Official mongoose SHA-256: $original_sha
Modified-relink mongoose SHA-256: $candidate_sha

PASS: clean extracted source and vendored dependencies built offline with an isolated, empty CARGO_HOME
PASS: mongoose library smoke tests linked and ran with modified libnfs
PASS: portable replacement executable linked with modified libnfs
PASS: deliberate marker "$marker" is present in the replacement executable
PASS: replacement executable has no dynamic libnfs dependency
PASS: replacement license and help commands run successfully
EOF
mv "$evidence_tmp" "$evidence"
echo "LGPL relink verification PASS: $evidence"
