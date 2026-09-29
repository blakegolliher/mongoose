#!/usr/bin/env bash
# Verify a downloaded mongoose release on a clean machine.
#
#   verify-release.sh --tag vX.Y.Z [--dir DIR] [--repo OWNER/NAME]
#
# DIR holds the release assets, including SHA256SUMS and SHA256SUMS.sigstore.json.
# Needs cosign (v3) and the GitHub CLI (gh 2.49 or newer, logged in or with
# GH_TOKEN set). These are the commands docs/BUILDING.md documents, and the
# release workflow runs this script on a fresh runner before anything is
# published:
#
# 1. The Sigstore signature on SHA256SUMS was made by this repository's release
#    workflow, for this tag.
# 2. Every file SHA256SUMS lists is present, and its SHA-256 matches.
# 3. Every listed file has a GitHub build-provenance attestation from that
#    workflow, at that tag, built on a GitHub-hosted runner.
# 4. The binary has an SBOM attestation from the same workflow.
set -euo pipefail

fail() {
    echo "release verification failed: $*" >&2
    exit 1
}

tag=""
dir=.
repo=blakegolliher/mongoose
while (($#)); do
    test $# -ge 2 || fail "$1 needs a value"
    case "$1" in
        --tag) tag="$2" ;;
        --dir) dir="$2" ;;
        --repo) repo="$2" ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift 2
done
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || fail "--tag vX.Y.Z is required"
for command_name in cosign gh sha256sum; do
    command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done
cd "$dir"
test -f SHA256SUMS || fail "SHA256SUMS is missing from $dir"
test -f SHA256SUMS.sigstore.json || fail "SHA256SUMS.sigstore.json is missing from $dir"

workflow="$repo/.github/workflows/release.yml"
identity="https://github.com/$workflow@refs/tags/$tag"
issuer=https://token.actions.githubusercontent.com

echo "== 1. Sigstore signature on SHA256SUMS"
cosign verify-blob SHA256SUMS \
    --bundle SHA256SUMS.sigstore.json \
    --certificate-identity "$identity" \
    --certificate-oidc-issuer "$issuer"

echo "== 2. Checksums"
sha256sum --check --strict SHA256SUMS

echo "== 3. Build provenance"
files=$(sed -E 's/^[0-9a-f]{64}  //' SHA256SUMS)
test -n "$files" || fail "SHA256SUMS lists no files"
for file in SHA256SUMS $files; do
    gh attestation verify "$file" --repo "$repo" \
        --signer-workflow "$workflow" --source-ref "refs/tags/$tag" \
        --deny-self-hosted-runners >/dev/null \
        || fail "no valid build-provenance attestation for $file"
    echo "  $file: provenance verified"
done

echo "== 4. SBOM attestation"
binary=$(grep -m1 -E '^mongoose-linux-[a-z0-9_]+$' <<<"$files" || true)
test -n "$binary" || fail "SHA256SUMS lists no bare binary"
gh attestation verify "$binary" --repo "$repo" \
    --signer-workflow "$workflow" --source-ref "refs/tags/$tag" \
    --predicate-type https://cyclonedx.org/bom >/dev/null \
    || fail "no valid SBOM attestation for $binary"
echo "  $binary: SBOM attestation verified"

echo "release $tag verified: signature, $(wc -l <<<"$files") checksums, provenance, and SBOM"
