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

# Please make changes via update-rust-version.sh
ARG RUST_IMAGE=rust:1.44.1-buster

# Use an arbitrary ~recent edge release image to get the proxy
# identity-initializing wrapper.
ARG RUNTIME_IMAGE=gcr.io/linkerd-io/proxy:edge-20.4.1

# Build the proxy, leveraging (new, experimental) cache mounting.
#
# See: https://github.com/moby/buildkit/blob/master/frontend/dockerfile/docs/experimental.md#run---mounttypecache
FROM $RUST_IMAGE as build

# When set, causes the proxy to be compiled in development mode.
ARG PROXY_UNOPTIMIZED

# Controls what features are enabled in the proxy. This is typically empty but
# may be set to `mock-orig-dst` for profiling builds, or to `multithreaded` to
# enable the multithreaded Tokio runtime.
ARG PROXY_FEATURES

RUN --mount=type=cache,target=/var/lib/apt/lists \
  --mount=type=cache,target=/var/tmp \
  apt update && apt install -y time cmake

WORKDIR /usr/src/linkerd2-proxy
COPY . .
RUN --mount=type=cache,target=target \
  --mount=type=cache,from=rust:1.44.1-buster,source=/usr/local/cargo,target=/usr/local/cargo \
  mkdir -p /out && \
  if [ -n "$PROXY_UNOPTIMIZED" ]; then \
  (cd linkerd2-proxy && /usr/bin/time -v cargo build --locked --features="$PROXY_FEATURES") && \
  mv target/debug/linkerd2-proxy /out/linkerd2-proxy ; \
  else \
  (cd linkerd2-proxy && /usr/bin/time -v cargo build --locked --release --features="$PROXY_FEATURES") && \
  mv target/release/linkerd2-proxy /out/linkerd2-proxy ; \
  fi

## Install the proxy binary into the base runtime image.
FROM $RUNTIME_IMAGE as runtime
WORKDIR /linkerd
COPY --from=build /out/linkerd2-proxy /usr/lib/linkerd/linkerd2-proxy
ENV LINKERD2_PROXY_LOG=warn,linkerd=info
# Inherits the ENTRYPOINT from the runtime image.
