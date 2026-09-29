#!/usr/bin/env bash
# Validate a relink kit's offline Cargo source map.
#
#   check-cargo-source-map.sh KIT/mongoose
#
# The directory must hold Cargo.lock, .cargo/config.toml, and vendor/. Every
# non-path source in Cargo.lock must be replaced by the `vendored-sources`
# directory source, and every locked package from such a source must have its
# vendored copy. Anything this script does not understand fails the check.
set -euo pipefail

fail() {
    echo "Cargo source map check failed: $*" >&2
    exit 1
}

test $# -eq 1 || fail "usage: $0 KIT/mongoose"
root="$1"
config="$root/.cargo/config.toml"
lockfile="$root/Cargo.lock"
test -f "$config" || fail "missing $config"
test -f "$lockfile" || fail "missing $lockfile"
test -d "$root/vendor" || fail "missing $root/vendor"

# Print KEY's quoted string value from the section whose header line is exactly
# HEADER, or nothing.
section_value() {
    local header="$1"
    local key="$2"
    awk -v header="$header" -v key="$key" '
        /^\[/ { in_section = ($0 == header); next }
        in_section && $0 ~ "^" key "[ \t]*=[ \t]*\"" {
            value = $0
            sub(/^[^=]*=[ \t]*"/, "", value)
            sub(/".*$/, "", value)
            print value
            exit
        }
    ' "$config"
}

require_value() {
    local header="$1"
    local key="$2"
    local expected="$3"
    local actual
    grep -Fxq -- "$header" "$config" || fail "$config has no $header section"
    actual=$(section_value "$header" "$key")
    test "$actual" = "$expected" \
        || fail "$config: $header $key is \"$actual\", expected \"$expected\""
}

duplicates=$(grep -E '^\[' "$config" | sort | uniq -d)
test -z "$duplicates" || fail "$config repeats section headers: $duplicates"

require_value '[source.vendored-sources]' directory vendor

sources=$(sed -n 's/^source = "\(.*\)"$/\1/p' "$lockfile" | sort -u)
test -n "$sources" || fail "$lockfile records no package sources"
while IFS= read -r source; do
    case "$source" in
        registry+https://github.com/rust-lang/crates.io-index)
            require_value '[source.crates-io]' replace-with vendored-sources
            ;;
        git+*\?rev=*#*)
            key=${source%%#*}
            url=${key#git+}
            url=${url%%\?*}
            rev=${key#*\?rev=}
            header="[source.\"$key\"]"
            require_value "$header" replace-with vendored-sources
            require_value "$header" git "$url"
            require_value "$header" rev "$rev"
            ;;
        *)
            fail "unsupported source in $lockfile: $source"
            ;;
    esac
done <<<"$sources"

packages=$(awk '
    function emit() { if (name != "" && source != "") print name "-" version }
    /^\[\[package\]\]$/ { emit(); name = ""; version = ""; source = ""; next }
    /^name = "/ { name = $3; gsub(/"/, "", name) }
    /^version = "/ { version = $3; gsub(/"/, "", version) }
    /^source = "/ { source = $3 }
    END { emit() }
' "$lockfile")
count=0
while IFS= read -r package; do
    test -f "$root/vendor/$package/Cargo.toml" || fail "vendor/ omits locked package $package"
    count=$((count + 1))
done <<<"$packages"

echo "Cargo source map covers $(wc -l <<<"$sources") source(s) and $count vendored package(s)"
