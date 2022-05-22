#!/usr/bin/env bash

set -eu

if [ -z "$PACKAGE_VERSION" ]; then
    echo "PACKAGE_VERSION is not set" >&2
    exit 1
fi
if [ -z "$ARCH" ]; then
    echo "ARCH is not set" >&2
    exit 1
fi


if [ "$ARCH" = "amd64" ]; then
    export CARGO_TARGET=x86_64-unknown-linux-gnu
    export STRIP=strip
elif [ "$ARCH" = "arm64" ]; then
    apt-get install -y --no-install-recommends g++-aarch64-linux-gnu libc6-dev-arm64-cross
    rustup target add aarch64-unknown-linux-gnu
    export CARGO_TARGET=aarch64-unknown-linux-gnu
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
    export STRIP=aarch64-linux-gnu-strip
elif [ "$ARCH" = "arm" ]; then
    apt-get install -y --no-install-recommends g++-arm-linux-gnueabihf libc6-dev-armhf-cross
    rustup target add armv7-unknown-linux-gnueabihf
    export CARGO_TARGET=armv7-unknown-linux-gnueabihf
    export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc
    export STRIP=arm-linux-gnueabihf-strip
else
    echo "Unsupported architecture: $ARCH" >&2
    exit 1
fi

CARGO_RELEASE=1 make package

# Compare the generated checksec output with the expected output. `diff` exits
# with an error code if the files do not match.
jq -S '.' "target/${CARGO_TARGET}/release/package/linkerd2-proxy-${PACKAGE_VERSION}-${ARCH}-checksec.json" \
    | diff -u /linkerd/expected-checksec.json - >&2
