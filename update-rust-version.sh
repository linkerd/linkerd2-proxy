#!/bin/sh

set -eu

if [ $# -ne 1 ]; then
    echo "usage: $0 VERSION" >&2
    exit 64
fi

VERSION="$1"
if ! echo "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$' ; then
    echo "Expected M.N.P; got '$VERSION'" >&2
    exit 64
fi

echo "$VERSION" > rust-toolchain
find . -name Dockerfile \
    -exec sed -i'' -Ee "s|RUST_VERSION=[0-9]+\.[0-9]+\.[0-9]+|RUST_VERSION=$VERSION|" '{}' \;
find .github -name \*.yml \
    -exec sed -i'' -Ee "s|rust:[0-9]+\.[0-9]+\.[0-9]+|rust:$VERSION|" '{}' \;
