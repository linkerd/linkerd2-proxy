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

features := ""

export LINKERD2_PROXY_VERSION := env_var_or_default("LINKERD2_PROXY_VERSION", "0.0.0-dev" + `git rev-parse --short HEAD`)
export LINKERD2_PROXY_VENDOR := env_var_or_default("LINKERD2_PROXY_VENDOR", `whoami` + "@" + `hostname`)

# The version name to use for packages.
package_version := "v" + LINKERD2_PROXY_VERSION

# Docker image name & tag.
docker-repo := "localhost/linkerd/proxy"
docker-tag := `git rev-parse --abbrev-ref HEAD | sed 's|/|.|g'` + "." + `git rev-parse --short HEAD`
docker-image := docker-repo + ":" + docker-tag

# The architecture name to use for packages. Either 'amd64', 'arm64', or 'arm'.
arch := "amd64"
# The OS name to use for packages. Either 'linux' or 'windows'.
os := "linux"

libc := 'gnu'

# If a `arch` is specified, then we change the default cargo `--target`
# to support cross-compilation. Otherwise, we use `rustup` to find the default.
_target := if os + '-' + arch == "linux-amd64" {
        "x86_64-unknown-linux-" + libc
    } else if os + '-' + arch == "linux-arm64" {
        "aarch64-unknown-linux-" + libc
    } else if os + '-' + arch == "linux-arm" {
        "armv7-unknown-linux-" + libc + "eabihf"
    } else if os + '-' + arch == "windows-amd64" {
        "x86_64-pc-windows-" + libc
    } else {
        error("unsupported: os=" + os + " arch=" + arch + " libc=" + libc)
    }

_cargo := 'just-cargo profile=' + profile + ' target=' + _target + ' toolchain=' + toolchain

_target_dir := "target" / _target / profile
_target_bin := _target_dir / "linkerd2-proxy" + if os == 'windows' { '.exe' } else { '' }
_package_name := "linkerd2-proxy-" + package_version + "-" + os + "-" + arch + if libc == 'musl' { '-static' } else { '' }
_package_dir := "target/package" / _package_name
shasum := "shasum -a 256"

_features := if features == "all" {
        "--all-features"
    } else if features != "" {
        "--no-default-features --features=" + features
    } else { "" }

wait-timeout := env_var_or_default("WAIT_TIMEOUT", "1m")

export CXX := 'clang++-19'

#
# Recipes
#

rustup:
    @{{ _cargo }} _target-installed

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
# XXX(ver) checksec doesn't work when profile=debug, so skip it.
build: _build _strip

# Build the proxy without stripping debug symbols
build-debug: _build

_build:
    @rm -f {{ _target_bin }} {{ _target_bin }}.dbg
    @{{ _cargo }} build --frozen --package=linkerd2-proxy {{ _features }}

_strip:
    {{ _objcopy }} --only-keep-debug {{ _target_bin }} {{ _target_bin }}.dbg
    {{ _objcopy }} --strip-unneeded {{ _target_bin }}
    {{ _objcopy }} --add-gnu-debuglink={{ _target_bin }}.dbg {{ _target_bin }}

_package_bin := _package_dir / "bin" / "linkerd2-proxy"

# XXX {aarch64,arm}-musl builds do not enable PIE, so we use target-specific
# files to document those differences.
_expected_checksec := '.checksec' / arch + '-' + libc + '.json'

# Check the security properties of the proxy binary.
checksec:
    checksec --output=json --file='{{ _target_bin }}' \
        | jq '.' | tee /dev/stderr \
        | jq -S '.[] | del(."fortify_source") | del(."fortify-able") | del(.fortified) | del(.symbols)' \
        | diff -u {{ _expected_checksec }} - >&2

_objcopy := 'llvm-objcopy-' + `just-cargo --evaluate _llvm-version`

# Build a package (i.e. for a release)
package: build
    @mkdir -p {{ _package_dir }}/bin
    cp LICENSE {{ _package_dir }}/
    cp {{ _target_bin }} {{ _target_bin }}.dbg {{ _package_dir }}/
    tar -czvf target/package/{{ _package_name }}.tar.gz  -C target/package {{ _package_name }} >/dev/null
    cd target/package && ({{ shasum }} {{ _package_name }}.tar.gz | tee {{ _package_name }}.txt)
    @rm -rf {{ _package_dir }}
    @du -h target/package/{{ _package_name }}.tar.gz
    @tar tzvf target/package/{{ _package_name }}.tar.gz

# Build all of the fuzzers (SLOW).
fuzzers:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "{{ toolchain }}" != "nightly" ]; then
        echo "fuzzers must be run with nightly" >&2
        exit 1
    fi
    for dir in $(find ./linkerd -type d -name fuzz); do
        echo "cd $dir && cargo +nightly fuzz build"
        ( cd $dir ; cargo +nightly fuzz build \
                {{ if profile == "release" { "--release" } else { "" } }} )
    done

export DOCKER_BUILDX_CACHE_DIR := env_var_or_default('DOCKER_BUILDX_CACHE_DIR', '')

# Build a docker image (FOR TESTING ONLY)
docker *args='--output=type=docker': && _clean-cache
    docker buildx build . \
        --pull \
        --tag={{ docker-image }} \
        --build-arg PROFILE='{{ profile }}' \
        --build-arg LINKERD2_PROXY_VENDOR='{{ LINKERD2_PROXY_VENDOR }}' \
        --build-arg LINKERD2_PROXY_VERSION='{{ LINKERD2_PROXY_VERSION }}' \
        --no-cache-filter=runtime \
        {{ if linkerd-tag == '' { '' } else { '--build-arg=RUNTIME_IMAGE=ghcr.io/linkerd/proxy:' + linkerd-tag } }} \
        {{ if features != "" { "--build-arg PROXY_FEATURES=" + features } else { "" } }} \
        {{ if DOCKER_BUILDX_CACHE_DIR == '' { '' } else { '--cache-from=type=local,src=' + DOCKER_BUILDX_CACHE_DIR + ' --cache-to=type=local,dest=' + DOCKER_BUILDX_CACHE_DIR } }} \
        {{ args }}

_clean-cache:
    @{{ if DOCKER_BUILDX_CACHE_DIR == '' { 'true' } else { 'just-dev prune-action-cache ' + DOCKER_BUILDX_CACHE_DIR } }}

# Lints all shell scripts in the repo.
sh-lint:
    @just-sh

md-lint:
    @just-md

# Lints all GitHub Actions workflows
action-lint:
    @just-dev lint-actions

action-dev-check:
    #!/usr/bin/env bash
    # TODO(ver) consolidate this again with just-dev
    #@just-dev check-action-images
    set -euo pipefail
    VERSION=$(j5j .devcontainer/devcontainer.json |jq -r '.build.args["DEV_VERSION"]')
    EX=0
    while IFS= read filelineimg ; do
        # Parse lines in the form `file:line img:tag`
        fileline="${filelineimg%% *}"
        file="${fileline%%:*}"
        line="${fileline##*:}"
        img="${filelineimg##* }"
        name="${img%%:*}"
        # Tag may be in the form of `version[-variant]`
        tag="${img##*:}"
        version="${tag%%-*}"
        if [ "$name" = 'ghcr.io/linkerd/dev' ] && [ "$version" != "$VERSION" ]; then
            if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
                echo "::error file=${file},line=${line}::Expected image 'ghcr.io/linkerd/dev:$VERSION'; found '${img}'" >&2
            else
                echo "${file}:${line}: Expected image 'ghcr.io/linkerd/dev:$VERSION'; found '${img}'" >&2
            fi
            EX=$(( EX+1 ))
        fi
    done < <( /usr/local/bin/action-images )
    exit $EX

##
## Linkerd
##

linkerd-tag := env_var_or_default('LINKERD_TAG', '')
_controller-image := 'ghcr.io/linkerd/controller'
_policy-image := 'ghcr.io/linkerd/policy-controller'
_init-image := 'ghcr.io/linkerd/proxy-init'
_init-tag := 'v2.4.0'

_kubectl := 'just-k3d kubectl'
_linkerd := 'linkerd --context=k3d-$(just-k3d --evaluate K3D_CLUSTER_NAME)'

_tag-set:
    #!/usr/bin/env bash
    if [ -z '{{ linkerd-tag }}' ]; then
        echo "linkerd-tag must be set" >&2
        exit 1
    fi

_k3d-ready:
    @just-k3d ready

export K3D_CLUSTER_NAME := "l5d-proxy"
export K3D_CREATE_FLAGS := "--no-lb"
export K3S_DISABLE := "local-storage,traefik,servicelb,metrics-server@server:*"
k3d-create: && _k3d-ready
    @just-k3d create

k3d-load-linkerd: _tag-set _k3d-ready
    for i in \
        '{{ _controller-image }}:{{ linkerd-tag }}' \
        '{{ _policy-image }}:{{ linkerd-tag }}' \
        '{{ _init-image }}:{{ _init-tag }}' \
    ; do \
        docker pull -q "$i" ; \
    done
    @just-k3d import \
        '{{ docker-image }}' \
        '{{ _controller-image }}:{{ linkerd-tag }}' \
        '{{ _policy-image }}:{{ linkerd-tag }}' \
        '{{ _init-image }}:{{ _init-tag }}'

# Install crds on the test cluster.
_linkerd-crds-install: _k3d-ready
    {{ _linkerd }} install --crds --set installGatewayAPI=true \
        | {{ _kubectl }} apply -f -
    {{ _kubectl }} wait crd --for condition=established \
        --selector='linkerd.io/control-plane-ns' \
        --timeout={{ wait-timeout }}

# Install linkerd on the test cluster using test images.
linkerd-install *args='': _tag-set k3d-load-linkerd _linkerd-crds-install && _linkerd-ready
    {{ _linkerd }} install \
            --set='imagePullPolicy=Never' \
            --set='controllerImage={{ _controller-image }}' \
            --set='linkerdVersion={{ linkerd-tag }}' \
            --set='policyController.image.name={{ _policy-image }}' \
            --set='policyController.image.version={{ linkerd-tag }}' \
            --set='proxy.image.name={{ docker-repo }}' \
            --set='proxy.image.version={{ docker-tag }}' \
            --set='proxy.logLevel=linkerd=debug\,info' \
            --set='proxyInit.image.name={{ _init-image }}' \
            --set='proxyInit.image.version={{ _init-tag }}' \
            {{ args }} \
        | {{ _kubectl }} apply -f -

linkerd-uninstall:
    {{ _linkerd }} uninstall \
        | {{ _kubectl }} delete -f -

linkerd-check-control-plane-proxy:
    #!/usr/bin/env bash
    set -euo pipefail
    check=$(mktemp --tmpdir check-XXXX.json)
    {{ _linkerd }} check -o json > "$check"
    result=$(jq -r \
        '.categories[] | select(.categoryName == "linkerd-control-plane-proxy") | .checks[] | select(.description == "control plane proxies are healthy") | .result' \
        "$check")
    if [ "$result" != "success" ]; then
        jq '.categories[] | .checks[] | select(.result != "success") | select(.hint | contains("-version") | not)' \
            "$check" >&2
        {{ _kubectl }} describe po -n linkerd >&2
        exit 1
    fi
    rm "$check"

_linkerd-ready:
    {{ _kubectl }} wait pod --for=condition=ready \
        --namespace=linkerd --selector='linkerd.io/control-plane-component' \
        --timeout={{ wait-timeout }}

#
# Dev Container
#

devcontainer-up:
    devcontainer.js up --workspace-folder=.

devcontainer-exec container-id *args:
    devcontainer.js exec --container-id={{ container-id }} {{ args }}
