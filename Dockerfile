# syntax=docker/dockerfile:1.4

# This is intended **DEVELOPMENT ONLY**, i.e. so that proxy developers can
# easily test the proxy in the context of the larger `linkerd2` project.

ARG RUST_IMAGE=ghcr.io/linkerd/dev:v42-rust

# Use an arbitrary ~recent edge release image to get the proxy
# identity-initializing and linkerd-await wrappers.
# Currently pinned to a build off of edge-23.11.1 + dev:v42
ARG LINKERD2_IMAGE=ghcr.io/olix0r/l2-proxy:git-04283611

# Build the proxy.
FROM --platform=$BUILDPLATFORM $RUST_IMAGE as build

ARG PROXY_FEATURES=""
RUN apt-get update && \
    apt-get install -y time && \
    if [[ "$PROXY_FEATURES" =~ .*meshtls-boring.* ]] ; then \
      apt-get install -y golang ; \
    fi && \
    rm -rf /var/lib/apt/lists/*

ENV CARGO_NET_RETRY=10
ENV RUSTUP_MAX_RETRIES=10

WORKDIR /usr/src/linkerd2-proxy
COPY . .
RUN --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    just fetch
ENV CARGO_INCREMENTAL=0
# -C split-debuginfo=unpacked 
ENV RUSTFLAGS="-D warnings -A deprecated -C debuginfo=2"
ARG TARGETARCH="amd64"
ARG PROFILE="release"
ARG LINKERD2_PROXY_VERSION=""
ARG LINKERD2_PROXY_VENDOR=""
RUN --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    just arch="$TARGETARCH" features="$PROXY_FEATURES" profile="$PROFILE" build && \
    mkdir -p /out && \
    mv $(just --evaluate profile="$PROFILE" _target_bin) /out/linkerd2-proxy

FROM $LINKERD2_IMAGE as linkerd2

# Install the proxy binary into a base image that we can at least get a shell to
# debug on.
FROM docker.io/library/debian:bookworm-slim as runtime
WORKDIR /linkerd
COPY --from=linkerd2 /usr/lib/linkerd/* /usr/lib/linkerd/
COPY --from=build /out/linkerd2-proxy /usr/lib/linkerd/linkerd2-proxy
ENTRYPOINT ["/usr/lib/linkerd/linkerd2-proxy-identity"]
