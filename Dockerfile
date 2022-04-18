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

ARG RUST_VERSION=1.60.0
ARG RUST_IMAGE=rust:${RUST_VERSION}-bullseye

# Use an arbitrary ~recent edge release image to get the proxy
# identity-initializing and linkerd-await wrappers.
ARG RUNTIME_IMAGE=ghcr.io/linkerd/proxy:edge-22.2.1

# Build the proxy, leveraging (new, experimental) cache mounting.
#
# See: https://github.com/moby/buildkit/blob/master/frontend/dockerfile/docs/experimental.md#run---mounttypecache
FROM $RUST_IMAGE as build

# When set, causes the proxy to be compiled in development mode.
ARG PROXY_UNOPTIMIZED

# Controls what features are enabled in the proxy.
ARG PROXY_FEATURES="multicore,meshtls-rustls"

RUN --mount=type=cache,target=/var/lib/apt/lists \
    --mount=type=cache,target=/var/tmp \
  apt update && apt install -y time

RUN --mount=type=cache,target=/var/lib/apt/lists \
    --mount=type=cache,target=/var/tmp \
  if $(echo "$PROXY_FEATURES" | grep "meshtls-boring" >/dev/null); then \
    apt install -y cmake clang golang ; \
  fi

WORKDIR /usr/src/linkerd2-proxy
COPY . .
RUN --mount=type=cache,target=target \
    --mount=type=cache,from=rust:1.60.0-bullseye,source=/usr/local/cargo,target=/usr/local/cargo \
  mkdir -p /out && \
  if [ -n "$PROXY_UNOPTIMIZED" ]; then \
    (cd linkerd2-proxy && /usr/bin/time -v cargo build --locked --no-default-features --features="$PROXY_FEATURES") && \
    mv target/debug/linkerd2-proxy /out/linkerd2-proxy ; \
  else \
    (cd linkerd2-proxy && /usr/bin/time -v cargo build --locked --no-default-features --features="$PROXY_FEATURES" --release) && \
    mv target/release/linkerd2-proxy /out/linkerd2-proxy ; \
  fi

## Install the proxy binary into the base runtime image.
FROM $RUNTIME_IMAGE as runtime

WORKDIR /linkerd
COPY --from=build /out/linkerd2-proxy /usr/lib/linkerd/linkerd2-proxy
ENV LINKERD2_PROXY_LOG=warn,linkerd=info
# Inherits the ENTRYPOINT from the runtime image.
