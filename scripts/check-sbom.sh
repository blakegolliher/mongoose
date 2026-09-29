#!/usr/bin/env bash
# Validate the release SBOM against the schema, the shipped binary, the Cargo
# graph, and the libnfs and nfs-walker locks.
#
#   check-sbom.sh --sbom FILE --binary FILE --version VERSION
#
# Run from the repository at the release commit. Fails unless:
# - `cyclonedx validate` accepts it as CycloneDX 1.5 JSON;
# - the top-level component is mongoose VERSION with the binary's SHA-256;
# - the components are exactly the crates in `cargo tree -p mongoose -e normal`
#   for the release target, plus libnfs;
# - libnfs matches packaging/libnfs.lock.json (revision, license, archive
#   digest, source URL) and is marked as statically linked;
# - nfs-walker matches packaging/nfs-walker.lock.json (version, revision, URL);
# - every dependency reference resolves, libnfs hangs off both crates that link
#   it, and no build-machine path leaked into the document.
set -euo pipefail

fail() {
    echo "SBOM check failed: $*" >&2
    exit 1
}

target=x86_64-unknown-linux-gnu
sbom=""
binary=""
version=""
while (($#)); do
    test $# -ge 2 || fail "$1 needs a value"
    case "$1" in
        --sbom) sbom="$2" ;;
        --binary) binary="$2" ;;
        --version) version="$2" ;;
        *) fail "unknown argument: $1" ;;
    esac
    shift 2
done
test -f "$sbom" || fail "--sbom must name the SBOM file"
test -f "$binary" || fail "--binary must name the bare binary"
test -n "$version" || fail "--version is required"
repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"
cyclonedx=${CYCLONEDX:-cyclonedx}
for command_name in "$cyclonedx" jq cargo sha256sum; do
    command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done

validation=$("$cyclonedx" validate --input-file "$sbom" --input-format json --input-version v1_5 --fail-on-errors 2>&1) \
    || fail "cyclonedx validate rejected $sbom: $validation"
jq -e '.bomFormat == "CycloneDX" and .specVersion == "1.5"' "$sbom" >/dev/null \
    || fail "$sbom is not a CycloneDX 1.5 document"

binary_sha=$(sha256sum "$binary" | cut -d' ' -f1)
jq -e --arg version "$version" --arg sha "$binary_sha" '
    .metadata.component.name == "mongoose"
    and .metadata.component.version == $version
    and ([.metadata.component.hashes[]? | select(.alg == "SHA-256") | .content] == [$sha])
' "$sbom" >/dev/null || fail "top-level component is not mongoose $version with SHA-256 $binary_sha"

# The crates actually compiled into the binary, as "name version".
tree=$(cargo tree -p mongoose -e normal --target "$target" --prefix none --locked --offline) \
    || fail "cargo tree failed"
expected=$(awk '{ sub(/^v/, "", $2); print $1 " " $2 }' <<<"$tree" | grep -v '^mongoose ' | sort -u)
listed=$(jq -r '.components[] | select(.name != "libnfs") | "\(.name) \(.version)"' "$sbom" | sort)
duplicates=$(uniq -d <<<"$listed")
test -z "$duplicates" || fail "components listed more than once: $duplicates"
if test "$expected" != "$listed"; then
    missing=$(comm -23 <(echo "$expected") <(echo "$listed") | paste -sd' ')
    extra=$(comm -13 <(echo "$expected") <(echo "$listed") | paste -sd' ')
    fail "components differ from cargo tree -p mongoose (missing: ${missing:-none}; extra: ${extra:-none})"
fi

libnfs_sha=$(jq -er .source_git_sha packaging/libnfs.lock.json)
libnfs_url=$(jq -er .source_url packaging/libnfs.lock.json)
libnfs_license=$(jq -er .license packaging/libnfs.lock.json)
libnfs_digest=$(jq -er .static_artifact_sha256 packaging/libnfs.lock.json)
jq -e --arg sha "$libnfs_sha" --arg url "$libnfs_url" --arg license "$libnfs_license" --arg digest "$libnfs_digest" '
    [.components[] | select(.name == "libnfs")] as $found
    | ($found | length) == 1
    and $found[0].version == $sha
    and ([$found[0].licenses[]? | .license.id // .expression] == [$license])
    and ([$found[0].hashes[]? | select(.alg == "SHA-256") | .content] == [$digest])
    and ([$found[0].externalReferences[]? | select(.type == "vcs") | .url] == [$url])
    and ([$found[0].properties[]? | select(.name == "mongoose:linkage") | .value] == ["static"])
' "$sbom" >/dev/null || fail "libnfs component does not match packaging/libnfs.lock.json ($libnfs_sha, $libnfs_license, $libnfs_digest)"

walker_sha=$(jq -er .source_git_sha packaging/nfs-walker.lock.json)
walker_url=$(jq -er .source_url packaging/nfs-walker.lock.json)
walker_version=$(jq -er .version packaging/nfs-walker.lock.json)
walker_version=${walker_version#nfs-walker }
jq -e --arg sha "$walker_sha" --arg url "$walker_url" --arg version "$walker_version" '
    [.components[] | select(.name == "nfs-walker")] as $found
    | ($found | length) == 1
    and $found[0].version == $version
    and ($found[0]["bom-ref"] | startswith("git+" + $url + "?rev=" + $sha + "#"))
    and ($found[0].purl | startswith("pkg:cargo/nfs-walker@" + $version + "?"))
    and ($found[0].purl | contains($url | ltrimstr("https://")))
    and ($found[0].purl | contains($sha))
' "$sbom" >/dev/null || fail "nfs-walker component does not match packaging/nfs-walker.lock.json ($walker_version at $walker_sha)"

jq -e '
    . as $doc
    | ([$doc.metadata.component["bom-ref"]] + [$doc.components[]["bom-ref"]] | map({(.): true}) | add) as $known
    | ([$doc.components[] | select(.name == "libnfs") | .["bom-ref"]][0]) as $libnfs
    | [$doc.components[] | select(.name == "nfs-walker" or .name == "migration-mover") | .["bom-ref"]] as $linkers
    | ($doc.dependencies | length) > 0
    and all($doc.dependencies[]; $known[.ref] == true)
    and all($doc.dependencies[] | .dependsOn[]?; $known[.] == true)
    and ($linkers | length) == 2
    and all($linkers[]; . as $l | any($doc.dependencies[]; .ref == $l and any(.dependsOn[]?; . == $libnfs)))
' "$sbom" >/dev/null || fail "dependency graph has dangling references or libnfs is not linked from nfs-walker and migration-mover"

if grep -Eq 'file:///(home|tmp|root|Users|runner)|/home/runner/|/tmp/' "$sbom"; then
    fail "$sbom contains a build-machine path"
fi

echo "SBOM check passed: $(jq '.components | length' "$sbom") components (cargo graph plus libnfs), mongoose $version sha256 $binary_sha"
