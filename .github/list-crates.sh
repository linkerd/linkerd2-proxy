#!/usr/bin/env bash

set -eu

if [ $# -eq 0 ]; then
    echo '[]'
    exit 0
fi

# Find the nearest Cargo.toml (except the root).
find_manifest() {
    p=${1%/*}
    if [ -f "$p/Cargo.toml" ]; then
        realpath "$p/Cargo.toml"
    else
        find_manifest "$p"
    fi
}

# Build an expression to match all changed manifests.
manifest_expr() {
    printf '%s' 'false'
    for file in "$@" ; do
        m=$(find_manifest "$file")
        if [ "$m" != "Cargo.toml" ]; then
            printf ' or (. == "%s")' "$m"
        fi
    done
    printf '\n'
}

expr=$(manifest_expr "$@")

# Get the crate names for all changed manifest directories.
crates=$(cargo metadata --locked --format-version=1 \
    | jq -cr "[.packages[] | select(.manifest_path | $expr) | .name]")

echo "crates=$crates" >> "$GITHUB_OUTPUT"
echo "$crates" | jq .
