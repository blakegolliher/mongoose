#!/usr/bin/env bash
# Refuse to sign or publish unless the release tag, the Cargo version, and the
# checked-out commit agree, and the commit is on protected main.
#
#   check-release-ref.sh --tag vX.Y.Z --commit SHA --main REF
#
# REF is the fetched main branch (origin/main in CI).
set -euo pipefail

fail() {
    echo "release ref check failed: $*" >&2
    exit 1
}

tag=""
commit=""
main=""
while (($#)); do
    test $# -ge 2 || fail "$1 needs a value"
    case "$1" in
        --tag) tag="$2" ;;
        --commit) commit="$2" ;;
        --main) main="$2" ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift 2
done
test -n "$tag" || fail "--tag is required"
test -n "$commit" || fail "--commit is required"
test -n "$main" || fail "--main is required"
repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

[[ "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?)$ ]] \
    || fail "tag $tag is not vMAJOR.MINOR.PATCH[-PRERELEASE]"
tag_version=${BASH_REMATCH[1]}

head=$(git rev-parse HEAD)
tagged=$(git rev-parse --verify --quiet "refs/tags/$tag^{commit}") || fail "tag $tag does not exist in this checkout"
full_commit=$(git rev-parse --verify --quiet "$commit^{commit}") || fail "commit $commit does not exist in this checkout"
test "$head" = "$tagged" || fail "checked-out commit $head is not what $tag points to ($tagged)"
test "$full_commit" = "$tagged" || fail "workflow commit $full_commit is not what $tag points to ($tagged)"

cargo_version=$(cargo metadata --no-deps --format-version 1 --offline \
    | jq -er '.packages[] | select(.name == "mongoose") | .version') \
    || fail "cannot read the mongoose version from Cargo metadata"
test "$cargo_version" = "$tag_version" || fail "tag $tag does not match the Cargo version $cargo_version"

git rev-parse --verify --quiet "$main^{commit}" >/dev/null || fail "$main does not exist in this checkout"
git merge-base --is-ancestor "$tagged" "$main" || fail "$tag ($tagged) is not on $main"

echo "release ref check passed: $tag = mongoose $cargo_version = $tagged, on $main"
