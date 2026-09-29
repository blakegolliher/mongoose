#!/usr/bin/env bash
# Re-verify a frozen release set after it moved between workflow jobs.
#
#   check-release-set.sh --dir DIR --sums-sha256 SHA [--extra NAME]...
#
# Fails unless DIR/SHA256SUMS has digest SHA (the one the build job froze),
# every file SHA256SUMS lists verifies, and DIR holds exactly those files,
# SHA256SUMS itself, and each --extra (the signature bundle, which is metadata
# outside the checksum list).
set -euo pipefail

fail() {
    echo "release set check failed: $*" >&2
    exit 1
}

dir=""
sums_sha256=""
extras=()
while (($#)); do
    test $# -ge 2 || fail "$1 needs a value"
    case "$1" in
        --dir) dir="$2" ;;
        --sums-sha256) sums_sha256="$2" ;;
        --extra) extras+=("$2") ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift 2
done
test -d "$dir" || fail "--dir must name the release set"
[[ "$sums_sha256" =~ ^[0-9a-f]{64}$ ]] || fail "--sums-sha256 must be a SHA-256"
cd "$dir"
test -f SHA256SUMS || fail "SHA256SUMS is missing"

actual=$(sha256sum SHA256SUMS | cut -d' ' -f1)
test "$actual" = "$sums_sha256" || fail "SHA256SUMS is $actual, but the build job froze $sums_sha256"
sha256sum --check --strict --quiet SHA256SUMS || fail "a file does not match SHA256SUMS"

listed=$( (sed -E 's/^[0-9a-f]{64}  //' SHA256SUMS; echo SHA256SUMS; printf '%s\n' "${extras[@]}") | sed '/^$/d' | sort)
present=$(find . -maxdepth 1 -type f -printf '%f\n' | sort)
if test "$listed" != "$present"; then
    diff <(echo "$listed") <(echo "$present") >&2 || true
    fail "the set is not exactly the frozen files${extras[*]:+ plus ${extras[*]}}"
fi
echo "release set verified: $(wc -l <SHA256SUMS) artifacts match SHA256SUMS $sums_sha256${extras[*]:+, plus ${extras[*]}}"
