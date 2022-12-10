# syntax=docker/dockerfile:experimental

# Proxy build and runtime
#
# This is intended **DEVELOPMENT ONLY**, i.e. so that proxy developers can
# easily test the proxy in the context of the larger `linkerd2` project.
#
# This Dockerfile requires expirmental features to be enabled in both the
# Docker client and daemon. You MUST use buildkit to build this image. The
# simplest way to do this is to set the `DOCKER_BUILDKIT` environment variable:
#
#     :; DOCKER_BUILDKIT=1 docker build .
#
# Alternatively, you can use `buildx`, which supports more complicated build
# configurations:
#
#     :; docker buildx build . --load

ARG RUST_IMAGE=ghcr.io/linkerd/dev:v38-rust

# Use an arbitrary ~recent edge release image to get the proxy
# identity-initializing and linkerd-await wrappers.
ARG RUNTIME_IMAGE=ghcr.io/linkerd/proxy:edge-22.12.1

# Build the proxy, leveraging (new, experimental) cache mounting.
#
# See: https://github.com/moby/buildkit/blob/master/frontend/dockerfile/docs/experimental.md#run---mounttypecache
FROM --platform=$BUILDPLATFORM $RUST_IMAGE as build

ARG PROXY_FEATURES=""
RUN apt-get update && \
    apt-get install -y time && \
    if [[ "$PROXY_FEATURES" =~ .*meshtls-boring.* ]] ; then \
      apt-get install -y golang ; \
    fi && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/linkerd2-proxy
COPY . .
RUN --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    just fetch
ARG TARGETARCH="amd64"
ARG PROFILE="release"
RUN --mount=type=cache,id=target,target=target \
    --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    just arch=$TARGETARCH features=$PROXY_FEATURES profile=$PROFILE build && \
    bin=$(just --evaluate profile="$PROFILE" _target_bin) ; \
    mkdir -p /out && mv $bin /out/linkerd2-proxy

## Install the proxy binary into the base runtime image.
FROM $RUNTIME_IMAGE as runtime
WORKDIR /linkerd
COPY --from=build /out/linkerd2-proxy /usr/lib/linkerd/linkerd2-proxy
ENV LINKERD2_PROXY_LOG=warn,linkerd=info
# Inherits the ENTRYPOINT from the runtime image.
