#!/usr/bin/env bash
# Prove the release checks fail when they should, using this release's own
# artifacts. Every case must be rejected; the untouched inputs must pass.
#
#   test-release-gates.sh --set DIR --version VERSION
#
# DIR is a frozen release set: the files SHA256SUMS lists, plus SHA256SUMS.
# Nothing in DIR is modified; each case works on a copy.
set -euo pipefail

fail() {
    echo "release gate self-test failed: $*" >&2
    exit 1
}

set_dir=""
version=""
while (($#)); do
    test $# -ge 2 || fail "$1 needs a value"
    case "$1" in
        --set) set_dir="$2" ;;
        --version) version="$2" ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift 2
done
test -f "$set_dir/SHA256SUMS" || fail "--set must name a frozen release set"
test -n "$version" || fail "--version is required"
set_dir=$(realpath "$set_dir")
repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"
binary="$set_dir/mongoose-linux-x86_64"
sbom="$set_dir/mongoose-$version-sbom.cdx.json"
test -f "$binary" || fail "missing $binary"
test -f "$sbom" || fail "missing $sbom"
sums_sha256=$(sha256sum "$set_dir/SHA256SUMS" | cut -d' ' -f1)

work=$(mktemp -d "${TMPDIR:-/tmp}/mongoose-gate-test.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
passed=0

# expect_failure NAME COMMAND...: the command must exit non-zero.
expect_failure() {
    local name="$1"
    shift
    if "$@" >"$work/out" 2>&1; then
        cat "$work/out" >&2
        fail "$name: the check accepted it"
    fi
    echo "  rejected: $name ($(tail -n1 "$work/out" | cut -c1-100))"
    passed=$((passed + 1))
}

echo "== SBOM"
./scripts/check-sbom.sh --sbom "$sbom" --binary "$binary" --version "$version" >/dev/null \
    || fail "the untouched SBOM does not pass"
sbom_case() {
    local name="$1"
    local filter="$2"
    jq "$filter" "$sbom" >"$work/sbom.json"
    expect_failure "$name" ./scripts/check-sbom.sh --sbom "$work/sbom.json" --binary "$binary" --version "$version"
}
sbom_case "SBOM without libnfs" '.components |= map(select(.name != "libnfs"))'
sbom_case "SBOM without a Cargo crate" '.components |= map(select(.name != "clap"))'
sbom_case "SBOM with an unlinked crate" '.components += [{"type": "library", "bom-ref": "x", "name": "aws-sdk-s3", "version": "1.0.0"}]'
sbom_case "SBOM with another libnfs digest" '(.components[] | select(.name == "libnfs") | .hashes[0].content) |= ("0" * 64)'
sbom_case "SBOM with another nfs-walker revision" '(.components[] | select(.name == "nfs-walker") | .["bom-ref"]) |= sub("rev=[0-9a-f]{7}"; "rev=0000000")'
sbom_case "SBOM with another binary digest" '.metadata.component.hashes[0].content = ("0" * 64)'

echo "== Frozen release set"
./scripts/check-release-set.sh --dir "$set_dir" --sums-sha256 "$sums_sha256" >/dev/null \
    || fail "the untouched release set does not pass"
set_case() {
    local name="$1"
    local change="$2"
    rm -rf "$work/set"
    cp -a "$set_dir" "$work/set"
    (cd "$work/set" && eval "$change")
    expect_failure "$name" ./scripts/check-release-set.sh --dir "$work/set" --sums-sha256 "$sums_sha256"
}
set_case "an altered artifact" 'printf x >>LICENSE-MIT'
set_case "a missing artifact" 'rm LICENSES.txt'
set_case "an extra file" 'echo stray >stray.txt'
set_case "a rewritten SHA256SUMS" 'sha256sum LICENSE-MIT >SHA256SUMS'
expect_failure "a set checked against another digest" \
    ./scripts/check-release-set.sh --dir "$set_dir" --sums-sha256 "$(printf '%064d' 0)"

echo "== Release tag"
git clone -q --no-hardlinks "$repo_root" "$work/repo"
cp scripts/check-release-ref.sh "$work/repo/scripts/"
(
    cd "$work/repo"
    git config user.name gate-test
    git config user.email gate-test@example.invalid
    head=$(git rev-parse HEAD)
    git update-ref refs/remotes/origin/main "$head"
    git tag -f "v$version" "$head" >/dev/null
    ./scripts/check-release-ref.sh --tag "v$version" --commit "$head" --main origin/main >/dev/null
) || fail "a matching tag on main does not pass"
ref_case() {
    local name="$1"
    shift
    expect_failure "$name" bash -c "cd '$work/repo' && $*"
}
ref_case "a tag that differs from the Cargo version" \
    'git tag -f v0.0.0-gate-test HEAD >/dev/null && ./scripts/check-release-ref.sh --tag v0.0.0-gate-test --commit HEAD --main origin/main'
ref_case "a tagged commit that is not on main" \
    "git commit -q --allow-empty -m off-main && git tag -f 'v$version' HEAD >/dev/null && ./scripts/check-release-ref.sh --tag 'v$version' --commit HEAD --main origin/main"

echo "release gate self-test passed: $passed bad inputs rejected, untouched inputs accepted"
