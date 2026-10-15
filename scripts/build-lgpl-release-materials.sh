#!/usr/bin/env bash
set -euo pipefail

fail() {
    echo "LGPL release-material build failed: $*" >&2
    exit 1
}

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

release_dir=""
version=""
binary=""
libnfs_source=""
nfs_walker_source=""
while (($#)); do
    case "$1" in
        --release-dir)
            test $# -ge 2 || fail "--release-dir needs a value"
            release_dir="$2"
            shift 2
            ;;
        --version)
            test $# -ge 2 || fail "--version needs a value"
            version="$2"
            shift 2
            ;;
        --binary)
            test $# -ge 2 || fail "--binary needs a value"
            binary="$2"
            shift 2
            ;;
        --libnfs-source)
            test $# -ge 2 || fail "--libnfs-source needs a value"
            libnfs_source="$2"
            shift 2
            ;;
        --nfs-walker-source)
            test $# -ge 2 || fail "--nfs-walker-source needs a value"
            nfs_walker_source="$2"
            shift 2
            ;;
        *)
            fail "unknown argument: $1"
            ;;
    esac
done

test -n "$release_dir" || fail "--release-dir is required"
test -n "$version" || fail "--version is required"
test -f "$binary" || fail "release binary is missing: $binary"
test -d "$libnfs_source/.git" || fail "libnfs source checkout is missing: $libnfs_source"
test -d "$nfs_walker_source/.git" || fail "nfs-walker source checkout is missing: $nfs_walker_source"

for command_name in cargo cargo-about git gzip jq sed sha256sum tar; do
    command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done

project_version=$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -n1)
test "$version" = "$project_version" || fail "requested version $version != Cargo.toml $project_version"

# Source archives must describe the exact committed work, never an accidental
# mixture of HEAD plus local edits.
test -z "$(git status --porcelain --untracked-files=normal)" || fail "mongoose worktree must be clean"
git -C "$libnfs_source" diff --quiet || fail "libnfs checkout has tracked changes"
git -C "$libnfs_source" diff --cached --quiet || fail "libnfs checkout has staged changes"
git -C "$nfs_walker_source" diff --quiet || fail "nfs-walker checkout has tracked changes"
git -C "$nfs_walker_source" diff --cached --quiet || fail "nfs-walker checkout has staged changes"

mongoose_sha=$(git rev-parse HEAD)
libnfs_sha=$(jq -er .source_git_sha packaging/libnfs.lock.json)
libnfs_url=$(jq -er .source_url packaging/libnfs.lock.json)
libnfs_static_sha=$(jq -er .static_artifact_sha256 packaging/libnfs.lock.json)
nfs_walker_sha=$(jq -er .source_git_sha packaging/nfs-walker.lock.json)
nfs_walker_url=$(jq -er .source_url packaging/nfs-walker.lock.json)
test "$(git -C "$libnfs_source" rev-parse HEAD)" = "$libnfs_sha" || fail "libnfs checkout is not at $libnfs_sha"
test "$(git -C "$nfs_walker_source" rev-parse HEAD)" = "$nfs_walker_sha" || fail "nfs-walker checkout is not at $nfs_walker_sha"

libnfs_short=${libnfs_sha:0:12}
nfs_walker_short=${nfs_walker_sha:0:12}
source_epoch=$(git show -s --format=%ct HEAD)
rustc_version=$(jq -er .rustc packaging/release-toolchain.lock.json)
cargo_version=$(jq -er .cargo packaging/release-toolchain.lock.json)
cargo_zigbuild_version=$(jq -er .cargo_zigbuild packaging/release-toolchain.lock.json)
zig_version=$(jq -er .zig packaging/release-toolchain.lock.json)
zig_llvm_version=$(jq -er .zig_llvm packaging/release-toolchain.lock.json)
zig_tarball_sha=$(jq -er .zig_x86_64_linux_tarball_sha256 packaging/release-toolchain.lock.json)
cmake_version=$(jq -er .cmake packaging/release-toolchain.lock.json)
binutils_version=$(jq -er .binutils packaging/release-toolchain.lock.json)

mkdir -p "$release_dir"
release_dir=$(CDPATH='' cd -- "$release_dir" && pwd)
binary=$(CDPATH='' cd -- "$(dirname -- "$binary")" && pwd)/$(basename -- "$binary")
work_root=$(mktemp -d "${TMPDIR:-/tmp}/mongoose-release-materials.XXXXXX")
cleanup() {
    rm -rf -- "$work_root"
}
trap cleanup EXIT

render() {
    sed \
        -e "s|@VERSION@|$version|g" \
        -e "s|@MONGOOSE_SHA@|$mongoose_sha|g" \
        -e "s|@LIBNFS_URL@|$libnfs_url|g" \
        -e "s|@LIBNFS_SHA@|$libnfs_sha|g" \
        -e "s|@LIBNFS_SHORT_SHA@|$libnfs_short|g" \
        -e "s|@LIBNFS_STATIC_SHA@|$libnfs_static_sha|g" \
        -e "s|@NFS_WALKER_URL@|$nfs_walker_url|g" \
        -e "s|@NFS_WALKER_SHA@|$nfs_walker_sha|g" \
        -e "s|@NFS_WALKER_SHORT_SHA@|$nfs_walker_short|g" \
        -e "s|@RUSTC_VERSION@|$rustc_version|g" \
        -e "s|@CARGO_VERSION@|$cargo_version|g" \
        -e "s|@CARGO_ZIGBUILD_VERSION@|$cargo_zigbuild_version|g" \
        -e "s|@ZIG_VERSION@|$zig_version|g" \
        -e "s|@ZIG_LLVM_VERSION@|$zig_llvm_version|g" \
        -e "s|@ZIG_TARBALL_SHA256@|$zig_tarball_sha|g" \
        -e "s|@CMAKE_VERSION@|$cmake_version|g" \
        -e "s|@BINUTILS_VERSION@|$binutils_version|g" \
        "$1"
}

make_tarball() {
    local parent="$1"
    local entry="$2"
    local output="$3"
    local pending="$output.pending"
    tar --sort=name --mtime="@$source_epoch" --owner=0 --group=0 --numeric-owner \
        -C "$parent" -cf - "$entry" | gzip -n >"$pending"
    mv "$pending" "$output"
}

# Release-level notices. LICENSES.txt deliberately contains the complete texts
# so the bare-binary download has a single obvious companion asset.
render packaging/LICENSES.txt.in >"$work_root/LICENSES.txt"
{
    cat LICENSE
    printf '\n\n--- GNU LGPL 2.1 ---\n'
    cat packaging/licenses/LGPL-2.1.txt
    printf '\n\n--- libnfs BSD-2-Clause license ---\n'
    cat packaging/licenses/BSD-2-Clause-libnfs.txt
} >>"$work_root/LICENSES.txt"

render packaging/THIRD_PARTY_LICENSES.header.md.in >"$work_root/THIRD_PARTY_LICENSES.md"
cargo about generate packaging/third-party-licenses.hbs \
    --config about.toml \
    --manifest-path crates/mongoose/Cargo.toml \
    --locked --offline --fail \
    --output-file "$work_root/rust-third-party.md"
cat "$work_root/rust-third-party.md" >>"$work_root/THIRD_PARTY_LICENSES.md"
render packaging/LIBNFS_SOURCE.md.in >"$work_root/LIBNFS_SOURCE.md"

install -m0644 "$work_root/LICENSES.txt" "$release_dir/LICENSES.txt"
install -m0644 "$work_root/THIRD_PARTY_LICENSES.md" "$release_dir/THIRD_PARTY_LICENSES.md"
install -m0644 "$work_root/LIBNFS_SOURCE.md" "$release_dir/LIBNFS_SOURCE.md"
install -m0644 LICENSE "$release_dir/LICENSE-MIT"
install -m0644 packaging/licenses/LGPL-2.1.txt "$release_dir/LICENSE-LGPL-2.1.txt"
install -m0644 packaging/licenses/BSD-2-Clause-libnfs.txt "$release_dir/LICENSE-BSD-2-Clause-libnfs.txt"

# Exact mongoose source plus the pinned git dependency's source.
source_parent="$work_root/source"
source_name="mongoose-$version"
mkdir -p "$source_parent/$source_name/third-party/nfs-walker-$nfs_walker_short"
git archive HEAD | tar -x -C "$source_parent/$source_name"
git -C "$nfs_walker_source" archive "$nfs_walker_sha" \
    | tar -x -C "$source_parent/$source_name/third-party/nfs-walker-$nfs_walker_short"
cat >"$source_parent/$source_name/SOURCE-PROVENANCE.txt" <<EOF
mongoose revision: $mongoose_sha
nfs-walker revision: $nfs_walker_sha
Cargo.lock SHA-256: $(sha256sum Cargo.lock | cut -d' ' -f1)
EOF
make_tarball "$source_parent" "$source_name" \
    "$release_dir/mongoose-$version-source.tar.gz"

# Complete source for the exact Library revision, including its generated
# protocol inputs/outputs and all original notices.
libnfs_parent="$work_root/libnfs-source"
libnfs_name="libnfs-$libnfs_short-source"
mkdir -p "$libnfs_parent/$libnfs_name"
git -C "$libnfs_source" archive "$libnfs_sha" | tar -x -C "$libnfs_parent/$libnfs_name"
make_tarball "$libnfs_parent" "$libnfs_name" \
    "$release_dir/$libnfs_name.tar.gz"

# Self-contained relink kit. cargo vendor emits the exact source replacement
# stanza needed for both crates.io and the pinned nfs-walker git dependency
# on stdout. Never pass --quiet: Cargo 1.98 then prints no stanza at all, and
# the kit silently falls back to the builder's own Cargo caches.
kit_parent="$work_root/relink"
kit_name="mongoose-$version-relink-kit"
kit="$kit_parent/$kit_name"
mkdir -p "$kit/mongoose" "$kit/libnfs"
git archive HEAD | tar -x -C "$kit/mongoose"
git -C "$libnfs_source" archive "$libnfs_sha" | tar -x -C "$kit/libnfs"
(cd "$kit/mongoose" && cargo vendor --locked --offline --versioned-dirs vendor \
    >"$work_root/vendor-config.toml" 2>"$work_root/cargo-vendor.log") || {
    cat "$work_root/cargo-vendor.log" >&2
    fail "cargo vendor failed"
}
test -s "$work_root/vendor-config.toml" || fail "cargo vendor printed no offline source map"
printf '\n# Offline source map generated by cargo vendor for this release.\n' \
    >>"$kit/mongoose/.cargo/config.toml"
cat "$work_root/vendor-config.toml" >>"$kit/mongoose/.cargo/config.toml"
./scripts/check-cargo-source-map.sh "$kit/mongoose"
# The kit must resolve from its own vendor/ alone: an empty Cargo home has no
# registry index, crate cache, or git checkout to fall back on.
(cd "$kit/mongoose" && CARGO_HOME="$work_root/empty-cargo-home" CARGO_NET_OFFLINE=true \
    cargo metadata --locked --offline --format-version 1 >/dev/null) \
    || fail "relink kit does not resolve offline from its vendored sources"

install -m0755 scripts/build-libnfs-static.sh "$kit/build-libnfs-static.sh"
install -m0755 packaging/relink-kit/verify-relink.sh "$kit/verify-relink.sh"
render packaging/relink-kit/RELINKING.md.in >"$kit/RELINKING.md"
install -m0644 "$work_root/LICENSES.txt" "$kit/LICENSES.txt"
install -m0644 "$work_root/THIRD_PARTY_LICENSES.md" "$kit/THIRD_PARTY_LICENSES.md"
install -m0644 "$work_root/LIBNFS_SOURCE.md" "$kit/LIBNFS_SOURCE.md"
sha256sum "$binary" >"$kit/OFFICIAL-BINARY.sha256"

# This is the substantive section-6 proof: a fresh offline build with an
# intentionally changed libnfs must pass before evidence or a kit is emitted.
"$kit/verify-relink.sh" \
    --original "$binary" \
    --evidence "$release_dir/RELINK-VERIFICATION.txt"
make_tarball "$kit_parent" "$kit_name" \
    "$release_dir/$kit_name.tar.gz"

echo "built LGPL release materials for mongoose $version"
