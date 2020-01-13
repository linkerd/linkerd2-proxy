#!/bin/bash
set -o errexit
set -o nounset
set -o pipefail
PROFDIR=$(dirname "$0")
cd "$PROFDIR"
rm ../target/release/profile* || true
RUSTFLAGS="-C debuginfo=2 -C lto=off" cargo build --release --bin profile  # try lto=thin or =fat if they don't make perf miss calls
[[ $(/bin/ls -1 ../target/release/profile* | grep -vc '.d' ) -gt 1 ]] && (echo "There are multiple profiling binaries under ../target/release. Please clean up."; exit 1)
mv ../target/release/profile ../target/release/profile-opt-and-dbg-symbols # ignore .d folder
