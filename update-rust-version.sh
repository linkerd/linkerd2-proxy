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
sed -i'' -Ee "s/rust:[0-9]+\.[0-9]+\.[0-9]+/rust:$VERSION/" Dockerfile

find .github -name \*.yml -or -name Dockerfile\* \
    -exec sed -i'' -Ee "s|rust:[0-9]+\.[0-9]+\.[0-9]+|rust:$VERSION|" '{}' \;
