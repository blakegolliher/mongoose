#!/usr/bin/env bash
# Verify that the tools on PATH (and $ZIG) are exactly the versions recorded in
# packaging/release-toolchain.lock.json. Recording versions is not enough: a
# release built with anything else is not the release the lock describes.
set -euo pipefail

fail() {
    echo "release toolchain check failed: $*" >&2
    exit 1
}

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
lock="$repo_root/packaging/release-toolchain.lock.json"
command -v jq >/dev/null 2>&1 || fail "jq is required"
test -f "$lock" || fail "missing $lock"

zig_bin=${ZIG:-}
if test -z "$zig_bin" && test -x /snap/zig/current/zig; then
    zig_bin=/snap/zig/current/zig
elif test -z "$zig_bin"; then
    zig_bin=$(command -v zig || true)
fi
test -n "$zig_bin" || fail "Zig is required; set ZIG=/path/to/zig"

mismatches=0
# check NAME LOCK_KEY COMMAND...: the first line the command prints must contain
# the locked version as a whole word (a "+build" suffix is allowed).
check() {
    local name="$1"
    local key="$2"
    shift 2
    local want output line
    want=$(jq -er --arg key "$key" '.[$key]' "$lock") || fail "$lock has no $key"
    # Capture everything: `| head -n1` under pipefail can fail on SIGPIPE.
    if ! output=$("$@" 2>/dev/null) || test -z "$output"; then
        echo "  $name: not runnable ($*)" >&2
        mismatches=$((mismatches + 1))
        return
    fi
    line=${output%%$'\n'*}
    local pattern="(^|[[:space:]])${want//./\\.}([[:space:]+]|$)"
    if [[ "$line" =~ $pattern ]]; then
        echo "  $name $want"
    else
        echo "  $name: locked $want, found: $line" >&2
        mismatches=$((mismatches + 1))
    fi
}

echo "release toolchain (packaging/release-toolchain.lock.json):"
check rustc rustc rustc --version
check cargo cargo cargo --version
check cargo-zigbuild cargo_zigbuild cargo-zigbuild --version
check zig zig "$zig_bin" version
check cmake cmake cmake --version
for tool in ar ranlib objdump readelf; do
    check "$tool" binutils "$tool" --version
done
check tar gnu_tar tar --version
check gzip gzip gzip --version
check cargo-about cargo_about cargo about --version
check cargo-cyclonedx cargo_cyclonedx cargo cyclonedx --version
check cyclonedx-cli cyclonedx_cli "${CYCLONEDX:-cyclonedx}" --version

test "$mismatches" -eq 0 || fail "$mismatches tool(s) differ from the lock"
