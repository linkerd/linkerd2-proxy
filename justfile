# See https://just.systems/man/en

#
# Configuration
#

export RUST_BACKTRACE := env_var_or_default("RUST_BACKTRACE", "short")
export PROTOC_NO_VENDOR := "1"

export DOCKER_BUILDKIT := "1"

# By default we compile in development mode mode because it's faster.
build_type := if env_var_or_default("RELEASE", "") == "" { "debug" } else { "release" }

toolchain := ""
cargo := "cargo" + if toolchain != "" { " +" + toolchain } else { "" }

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
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER := "aarch64-linux-gnu-gcc"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER := "arm-linux-gnueabihf-gcc"
strip := if package_arch == "arm64" { "aarch64-linux-gnu-strip" } else if package_arch == "arm" { "arm-linux-gnueabihf-strip" } else { "strip" }

target_dir := join("target", cargo_target, build_type)
target_bin := join(target_dir, "linkerd2-proxy")
package_name := "linkerd2-proxy-" + package_version + "-" + package_arch
package_dir := join("target/package", package_name)
shasum := "shasum -a 256"

# If we're running in Github Actions and cargo-action-fmt is installed, then add
# a command suffix that formats errors.
_fmt := if env_var_or_default("GITHUB_ACTIONS", "") != "true" { "" } else {
    ```
    if command -v cargo-action-fmt >/dev/null 2>&1; then
        echo "--message-format=json | cargo-action-fmt"
    fi
    ```
}

_features := if features == "all" {
        "--all-features"
    } else if features != "" {
        "--no-default-features --features=" + features
    } else { "" }


# Use nextest if it's available (and we're not in CI). Mostly to work around
# https://github.com/nextest-rs/nextest/issues/422
_test := if env_var_or_default("GITHUB_ACTIONS", "") == "true" { "test" } else {
    ```
    if command -v cargo-nextest >/dev/null 2>&1; then
        echo "nextest run"
    else
        echo "test"
    fi
    ```
}

#
# Recipes
#

# Run all lints
lint: sh-lint md-lint clippy doc action-lint action-dev-check

# Fetch dependencies
fetch:
    {{ cargo }} fetch --locked

fmt:
    {{ cargo }} fmt

# Fails if the code does not match the expected format (via rustfmt).
check-fmt:
    {{ cargo }} fmt -- --check

check *flags:
    {{ cargo }} check --workspace --all-targets --frozen {{ flags }} {{ _fmt }}

check-crate crate *flags:
    {{ cargo }} check --package={{ crate }} --all-targets --frozen {{ _features }} {{ flags }} {{ _fmt }}

clippy *flags:
    {{ cargo }} clippy --workspace --all-targets --frozen {{ _features }} {{ flags }} {{ _fmt }}

clippy-crate crate *flags:
    {{ cargo }} clippy --package={{ crate }} --all-targets --frozen {{ _features }} {{ flags }} {{ _fmt }}

clippy-dir dir *flags:
    cd {{ dir }} && {{ cargo }} clippy --all-targets --frozen {{ _features }} {{ flags }} {{ _fmt }}

doc *flags:
    {{ cargo }} doc --no-deps --workspace --frozen {{ _features }} {{ flags }} {{ _fmt }}

doc-crate crate *flags:
    {{ cargo }} doc --package={{ crate }} --all-targets --frozen {{ _features }} {{ flags }} {{ _fmt }}

# Run all tests
test *flags:
    {{ cargo }} {{ _test }} --workspace --frozen {{ _features }} \
        {{ if build_type == "release" { "--release" } else { "" } }} \
        {{ flags }}

test-crate crate *flags:
    {{ cargo }} {{ _test }} --package={{ crate }} --frozen {{ _features }} \
        {{ if build_type == "release" { "--release" } else { "" } }} \
        {{ flags }}

test-dir dir *flags:
    cd {{ dir }} && {{ cargo }} {{ _test }} --frozen {{ _features }} \
            {{ if build_type == "release" { "--release" } else { "" } }} \
            {{ flags }}

# Build the proxy
build:
    {{ cargo }} build --frozen --package=linkerd2-proxy --target={{ cargo_target }} \
        {{ if build_type == "release" { "--release" } else { "" } }} \
        {{ _features }} {{ _fmt }}

# Build a package (i.e. for a release)
package: build
    mkdir -p {{ package_dir }}/bin
    cp LICENSE {{ package_dir }}/
    cp {{ target_dir }}/linkerd2-proxy {{ package_dir }}/bin/
    {{ strip }} {{ package_dir }}/bin/linkerd2-proxy ; \
    ./checksec.sh {{ package_dir }}/bin/linkerd2-proxy \
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
        echo "cd $dir && {{ cargo }} fuzz build"
        (
            cd $dir
            {{ cargo }} fuzz build --target={{ cargo_target }} \
                {{ if build_type == "release" { "--release" } else { "" } }}
        )
    done

# Build a docker image (FOR TESTING ONLY)
docker mode='load':
    docker buildx build . \
        --tag={{ docker_image }} \
        {{ if build_type != 'release' { "--build-arg PROXY_UNOPTIMIZED=1" } else { "" } }} \
        {{ if features != "" { "--build-arg PROXY_FEATURES=" + features } else { "" } }} \
        {{ if mode == 'push' { "--push" } else { "--load" } }}

# Lints all shell scripts in the repo.
sh-lint:
    #!/usr/bin/env bash
    set -euo pipefail
    files=$(while IFS= read -r f ; do
        if [ "$(file -b --mime-type "$f")" = text/x-shellscript ]; then echo "$f"; fi
    done < <(find . -type f ! \( -path ./.git/\* -or -path \*/target/\* \)) | xargs)
    echo "shellcheck $files" >&2
    shellcheck $files

md-lint:
    markdownlint-cli2 '**/*.md' '!target' '!.direnv'

# Format actionlint output for Github Actions if running in CI.
_actionlint-fmt := if env_var_or_default("GITHUB_ACTIONS", "") != "true" { "" } else {
  '{{range $err := .}}::error file={{$err.Filepath}},line={{$err.Line}},col={{$err.Column}}::{{$err.Message}}%0A```%0A{{replace $err.Snippet "\\n" "%0A"}}%0A```\n{{end}}'
}

# Lints all GitHub Actions workflows
action-lint:
    actionlint \
        {{ if _actionlint-fmt != '' { "-format '" + _actionlint-fmt + "'" } else { "" } }} \
        .github/workflows/*

action-dev-check:
    action-dev-check

# Display the git history minus dependabot updates
history *paths='.':
    @-git log --oneline --graph --invert-grep --author="dependabot" -- {{ paths }}

# vim: set ft=make :
