#!/usr/bin/env bash

set -eu

if [ $# -eq 0 ]; then
    echo "Usage: $0 <changed-files>"
    exit 1
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
    expr=false

    for file in "$@" ; do
        # If the workflow changes or root Cargo.toml changes, run checks for all crates in the repo.
        if [[ "$file" = .github/* ]] || [ "$file" = "Cargo.toml" ]; then
            expr="startswith(\"$PWD\")"
            break
        fi

        # Otherwise, only run checks for changes to subcrates (and not the top-level crate).
        m=$(find_manifest "$file")
        if [ "$m" != "Cargo.toml" ]; then
            expr="$expr or (. == \"$m\")"
        fi
    done

    echo "$expr"
}

# Get the crate names for all changed manifest directories.
crates=$(cargo metadata --locked --format-version=1 \
    | jq -cr "[.packages[] | select(.manifest_path | $(manifest_expr "$@")) | .name]")

echo "crates=$crates" >> "$GITHUB_OUTPUT"
echo "$crates" | jq .
