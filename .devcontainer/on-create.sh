#!/usr/bin/env bash

set -euo pipefail

rustup toolchain install --profile=minimal nightly
cargo +nightly install cargo-fuzz

scurl https://run.linkerd.io/install-edge | sh
mkdir -p "$HOME/bin"
(cd "$HOME/bin" && ln -s "$HOME/.linkerd2/bin/linkerd" .)
