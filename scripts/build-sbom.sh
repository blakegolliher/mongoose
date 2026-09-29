#!/usr/bin/env bash
# Generate the release SBOM (CycloneDX 1.5 JSON) for the shipped binary.
#
#   build-sbom.sh --binary FILE --version VERSION --output FILE
#
# cargo-cyclonedx runs on a copy of the committed tree (`git archive HEAD`), so
# the checkout stays clean. It resolves features across the whole workspace,
# which would list crates mongoose never links (the AWS SDK, for one), so the
# result is cut down to exactly `cargo tree -p mongoose -e normal` for the
# release target. The script then adds libnfs, which Cargo does not know
# about: it is statically linked, and its identity comes from
# packaging/libnfs.lock.json. It records the binary's SHA-256 on the top-level
# component, removes build-machine paths, and validates the result with
# check-sbom.sh.
set -euo pipefail

fail() {
    echo "SBOM build failed: $*" >&2
    exit 1
}

target=x86_64-unknown-linux-gnu
repository=https://github.com/blakegolliher/mongoose
binary=""
version=""
output=""
while (($#)); do
    test $# -ge 2 || fail "$1 needs a value"
    case "$1" in
        --binary) binary="$2" ;;
        --version) version="$2" ;;
        --output) output="$2" ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift 2
done
test -f "$binary" || fail "--binary must name the bare binary"
test -n "$version" || fail "--version is required"
test -n "$output" || fail "--output is required"
repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
binary=$(realpath "$binary")
output=$(realpath -m "$output")
cd "$repo_root"
for command_name in cargo cargo-cyclonedx git jq sha256sum; do
    command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done

work=$(mktemp -d "${TMPDIR:-/tmp}/mongoose-sbom.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
src="$work/src"
mkdir -p "$src"
git archive HEAD | tar -x -C "$src"
commit=$(git rev-parse HEAD)

(cd "$src" && CARGO_NET_OFFLINE=true cargo cyclonedx --quiet \
    --manifest-path crates/mongoose/Cargo.toml --describe binaries \
    --target "$target" --spec-version 1.5 --no-build-deps --format json) \
    || fail "cargo cyclonedx failed"
raw="$src/crates/mongoose/mongoose_bin.cdx.json"
test -f "$raw" || fail "cargo cyclonedx did not write $raw"
cmp -s Cargo.lock "$src/Cargo.lock" || fail "cargo cyclonedx changed Cargo.lock"

tree=$(cargo tree --manifest-path "$src/Cargo.toml" -p mongoose -e normal --target "$target" \
    --prefix none --locked --offline) || fail "cargo tree failed"
keep=$(awk '{ sub(/^v/, "", $2); print $1 "@" $2 }' <<<"$tree" | grep -v '^mongoose@' | sort -u | jq -R . | jq -s .)

libnfs_sha=$(jq -er .source_git_sha packaging/libnfs.lock.json)
libnfs_url=$(jq -er .source_url packaging/libnfs.lock.json)
libnfs_license=$(jq -er .license packaging/libnfs.lock.json)
libnfs_digest=$(jq -er .static_artifact_sha256 packaging/libnfs.lock.json)
libnfs_purl="pkg:github/${libnfs_url#https://github.com/}@$libnfs_sha"
binary_sha=$(sha256sum "$binary" | cut -d' ' -f1)

jq --argjson keep "$keep" --arg root "file://$src" \
    --arg binary_sha "$binary_sha" --arg commit "$commit" --arg repository "$repository" \
    --arg libnfs_sha "$libnfs_sha" --arg libnfs_url "$libnfs_url" --arg libnfs_license "$libnfs_license" \
    --arg libnfs_digest "$libnfs_digest" --arg libnfs_purl "$libnfs_purl" '
    ($keep | map({(.): true}) | add) as $wanted
    | .components |= map(select($wanted[.name + "@" + .version] == true))
    | ([.metadata.component["bom-ref"]] + [.components[]["bom-ref"]] | map({(.): true}) | add) as $kept
    | .dependencies |= map(select($kept[.ref] == true) | .dependsOn = ((.dependsOn // []) | map(select($kept[.] == true))))
    | ("libnfs@" + $libnfs_sha) as $libnfs_ref
    | .components += [{
        "type": "library",
        "bom-ref": $libnfs_ref,
        "name": "libnfs",
        "version": $libnfs_sha,
        "description": "NFS client library, statically linked into mongoose",
        "scope": "required",
        "hashes": [{"alg": "SHA-256", "content": $libnfs_digest}],
        "licenses": [{"license": {"id": $libnfs_license}}],
        "purl": $libnfs_purl,
        "externalReferences": [{"type": "vcs", "url": $libnfs_url}],
        "properties": [
            {"name": "mongoose:linkage", "value": "static"},
            {"name": "mongoose:artifact", "value": "libnfs.a"}
        ]
      }]
    | ([.components[] | select(.name == "nfs-walker" or .name == "migration-mover") | .["bom-ref"]]) as $linkers
    | .dependencies |= map(if (.ref as $r | $linkers | index($r)) != null then .dependsOn += [$libnfs_ref] else . end)
    | .dependencies += [{"ref": $libnfs_ref, "dependsOn": []}]
    | .metadata.component.hashes = [{"alg": "SHA-256", "content": $binary_sha}]
    | .metadata.component.externalReferences = [{"type": "vcs", "url": $repository}]
    | .metadata.component.properties = [{"name": "mongoose:git_commit", "value": $commit}]
    | walk(if type == "string" then (split($root) | join("file://.")) else . end)
' "$raw" >"$work/sbom.json" || fail "could not rewrite the SBOM"

mkdir -p "$(dirname -- "$output")"
install -m0644 "$work/sbom.json" "$output"
./scripts/check-sbom.sh --sbom "$output" --binary "$binary" --version "$version" \
    || { rm -f -- "$output"; fail "the generated SBOM did not validate"; }
