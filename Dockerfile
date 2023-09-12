# syntax=docker/dockerfile:1.4

# This is intended **DEVELOPMENT ONLY**, i.e. so that proxy developers can
# easily test the proxy in the context of the larger `linkerd2` project.

ARG RUST_IMAGE=ghcr.io/linkerd/dev:v40-rust

# Use an arbitrary ~recent edge release image to get the proxy
# identity-initializing and linkerd-await wrappers.
ARG RUNTIME_IMAGE=ghcr.io/linkerd/proxy:edge-22.12.1

# Build the proxy.
FROM --platform=$BUILDPLATFORM $RUST_IMAGE as build

ARG PROXY_FEATURES=""
RUN apt-get update && \
    apt-get install -y time && \
    if [[ "$PROXY_FEATURES" =~ .*meshtls-boring.* ]] ; then \
      apt-get install -y golang ; \
    fi && \
    rm -rf /var/lib/apt/lists/*

ENV CARGO_INCREMENTAL=0

ENV CARGO_NET_RETRY=10
ENV RUSTFLAGS="-D warnings -A deprecated"
ENV RUSTUP_MAX_RETRIES=10

WORKDIR /usr/src/linkerd2-proxy
COPY . .
RUN --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    just fetch
ARG TARGETARCH="amd64"
ARG PROFILE="release"
ARG LINKERD2_PROXY_VERSION=""
ARG LINKERD2_PROXY_VENDOR=""
RUN --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    just arch="$TARGETARCH" features="$PROXY_FEATURES" profile="$PROFILE" build && \
    mkdir -p /out && \
    mv $(just --evaluate profile="$PROFILE" _target_bin) /out/linkerd2-proxy

## Install the proxy binary into the base runtime image.
FROM $RUNTIME_IMAGE as runtime
WORKDIR /linkerd
COPY --from=build /out/linkerd2-proxy /usr/lib/linkerd/linkerd2-proxy
ENV LINKERD2_PROXY_LOG=warn,linkerd=info,trust_dns=error
# Inherits the ENTRYPOINT from the runtime image.
