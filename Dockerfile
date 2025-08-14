# syntax=docker/dockerfile:1.4

# This is intended **DEVELOPMENT ONLY**, i.e. so that proxy developers can
# easily test the proxy in the context of the larger `linkerd2` project.

ARG RUST_IMAGE=ghcr.io/linkerd/dev:v47-rust

# Use an arbitrary ~recent edge release image to get the proxy
# identity-initializing and linkerd-await wrappers.
ARG LINKERD2_IMAGE=ghcr.io/linkerd/proxy:edge-25.8.2

FROM --platform=$BUILDPLATFORM $RUST_IMAGE as base
ARG PROXY_FEATURES=""
ARG TARGETARCH="amd64"
RUN apt-get update && \
    apt-get install -y time && \
    if [[ "$PROXY_FEATURES" =~ .*meshtls-boring.* ]] ; then \
      apt-get install -y golang ; \
    fi && \
    case "$TARGETARCH" in \
        amd64) true ;; \
        arm64) apt-get install --no-install-recommends -y binutils-aarch64-linux-gnu ;; \
    esac && \
    rm -rf /var/lib/apt/lists/*

FROM base as fetch
ENV CARGO_NET_RETRY=10
ENV RUSTUP_MAX_RETRIES=10
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    just fetch

FROM fetch as build
ENV CARGO_INCREMENTAL=0
ARG RUSTFLAGS="-D warnings -A deprecated --cfg tokio_unstable"
ARG PROFILE="release"
ARG LINKERD2_PROXY_VERSION=""
ARG LINKERD2_PROXY_VENDOR=""
SHELL ["/bin/bash", "-c"]
RUN --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    if [[ "$PROXY_FEATURES" =~ .*pprof.* ]] ; then cmd=build-debug ; else cmd=build ; fi ; \
    /usr/bin/time -v just arch="$TARGETARCH" profile="$PROFILE" features="$PROXY_FEATURES" "$cmd" && \
    ( mkdir -p /out ; \
        mv $(just --evaluate arch="$TARGETARCH" profile="$PROFILE" _target_bin) /out/ ; \
        du -sh /out/* )

FROM scratch as bin
COPY --from=build /out/linkerd2-proxy /

# Install the proxy binary into a base image that we can at least get a shell
# for debugging.
FROM $LINKERD2_IMAGE as linkerd2
FROM docker.io/library/debian:bookworm-slim as runtime
WORKDIR /linkerd
COPY --from=linkerd2 /usr/lib/linkerd/* /usr/lib/linkerd/
COPY --from=build /out/* /usr/lib/linkerd/
ENTRYPOINT ["/usr/lib/linkerd/linkerd2-proxy-identity"]
