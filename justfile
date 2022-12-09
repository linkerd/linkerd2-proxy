# See https://just.systems/man/en

#
# Configuration
#

export RUST_BACKTRACE := env_var_or_default("RUST_BACKTRACE", "short")

# Use the dev environment's version of protoc.
export PROTOC_NO_VENDOR := "1"

# By default we compile in development mode mode because it's faster.
profile := if env_var_or_default("RELEASE", "") == "" { "debug" } else { "release" }
toolchain := ""
_cargo := "just-cargo profile=" + profile + " toolchain=" + toolchain

features := ""

# The version name to use for packages.
package_version := `git rev-parse --short HEAD`

# Docker image name & tag.
docker_repo := "localhost/linkerd/proxy"
docker_tag := `git rev-parse --abbrev-ref HEAD | sed 's|/|.|'` + "." + `git rev-parse --short HEAD`
docker_image := docker_repo + ":" + docker_tag

# The architecture name to use for packages. Either 'amd64', 'arm64', or 'arm'.
package_arch := "amd64"

# If a `package_arch` is specified, then we change the default cargo `--target`
# to support cross-compilation. Otherwise, we use `rustup` to find the default.
cargo_target := if package_arch == "arm64" {
        "aarch64-unknown-linux-gnu"
    } else if package_arch == "arm" {
        "armv7-unknown-linux-gnueabihf"
    } else {
        `rustup show | sed -n 's/^Default host: \(.*\)/\1/p'`
    }

# Support cross-compilation when `package_arch` changes.
strip := if package_arch == "arm64" { "aarch64-linux-gnu-strip" } else if package_arch == "arm" { "arm-linux-gnueabihf-strip" } else { "strip" }

target_dir := join("target", cargo_target, profile)
target_bin := join(target_dir, "linkerd2-proxy")
package_name := "linkerd2-proxy-" + package_version + "-" + package_arch
package_dir := join("target/package", package_name)
shasum := "shasum -a 256"

_features := if features == "all" {
        "--all-features"
    } else if features != "" {
        "--no-default-features --features=" + features
    } else { "" }

#
# Recipes
#

# Run all lints
lint: sh-lint md-lint clippy doc action-lint action-dev-check

# Fetch dependencies
fetch:
    @{{ _cargo }} fetch --locked

fmt:
    @{{ _cargo }} fmt

# Fails if the code does not match the expected format (via rustfmt).
check-fmt:
    @{{ _cargo }} fmt -- --check

check *flags:
    @{{ _cargo }} check --workspace --all-targets --frozen {{ flags }}

check-crate crate *flags:
    @{{ _cargo }} check --package={{ crate }} --all-targets --frozen {{ _features }} {{ flags }}

clippy *flags:
    @{{ _cargo }} clippy --workspace --all-targets --frozen {{ _features }} {{ flags }}

clippy-crate crate *flags:
    @{{ _cargo }} clippy --package={{ crate }} --all-targets --frozen {{ _features }} {{ flags }}

clippy-dir dir *flags:
    cd {{ dir }} && {{ _cargo }} clippy --all-targets --frozen {{ _features }} {{ flags }}

doc *flags:
    @{{ _cargo }} doc --no-deps --workspace --frozen {{ _features }} {{ flags }}

doc-crate crate *flags:
    @{{ _cargo }} doc --package={{ crate }} --all-targets --frozen {{ _features }} {{ flags }}

# Build all tests
test-build *flags:
    @{{ _cargo }} test-build --workspace --frozen {{ _features }} {{ flags }}

# Run all tests
test *flags:
    @{{ _cargo }} test --workspace --frozen {{ _features }} {{ flags }}

test-crate crate *flags:
    @{{ _cargo }} test --package={{ crate }} --frozen {{ _features }} {{ flags }}

test-dir dir *flags:
    cd {{ dir }} && {{ _cargo }} test --frozen {{ _features }} {{ flags }}

# Build the proxy
build:
    @{{ _cargo }} build --frozen --package=linkerd2-proxy --target={{ cargo_target }} {{ _features }}

_package_bin := package_dir / "bin" / "linkerd2-proxy"

# Build a package (i.e. for a release)
package: build
    mkdir -p {{ package_dir }}/bin
    cp LICENSE {{ package_dir }}/
    cp {{ target_dir }}/linkerd2-proxy {{ _package_bin }}
    {{ strip }} {{ _package_bin }}
    checksec --output=json --file='{{ _package_bin }}' \
        | jq '.["{{ _package_bin }}"] | del(."fortify-able") | del(.fortified)' \
        > target/package/{{ package_name }}-checksec.json
    jq -S '.'  target/package/{{ package_name }}-checksec.json \
        | diff -u .checksec-expected.json - >&2
    cd target/package \
        && (tar -czvf {{ package_name }}.tar.gz {{ package_name }} >/dev/null) \
        && ({{ shasum }} {{ package_name }}.tar.gz > {{ package_name }}.txt)
    @rm -rf {{ package_dir }}
    @du -h target/package/{{ package_name }}*

# Build all of the fuzzers (SLOW).
fuzzers:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "{{ toolchain }}" != "nightly" ]; then
        echo "fuzzers must be run with nightly" >&2
        exit 1
    fi

    for dir in $(find . -type d -name fuzz); do
        echo "cd $dir && {{ _cargo }} fuzz build"
        (
            cd $dir
            @{{ _cargo }} fuzz build --target={{ cargo_target }} \
                {{ if profile == "release" { "--release" } else { "" } }}
        )
    done

# Build a docker image (FOR TESTING ONLY)
docker mode='load':
    docker buildx build . \
        --tag={{ docker_image }} \
        {{ if profile != 'release' { "--build-arg PROXY_UNOPTIMIZED=1" } else { "" } }} \
        {{ if features != "" { "--build-arg PROXY_FEATURES=" + features } else { "" } }} \
        {{ if mode == 'push' { "--push" } else { "--load" } }}

# Lints all shell scripts in the repo.
sh-lint:
    @just-sh

md-lint:
    @just-md

# Lints all GitHub Actions workflows
action-lint:
    @just-dev lint-actions

action-dev-check:
    @just-dev check-action-images
