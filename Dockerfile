# Proxy build and runtime
#
# This is intended **DEVELOPMENT ONLY**, i.e. so that proxy developers can
# easily test the proxy in the context of the larger `linkerd2` project.
#
# When PROXY_UNOPTIMIZED is set and not empty, unoptimized rust artifacts are produced.
# This reduces build time and produces binaries with debug symbols, at the expense of
# runtime performance.

ARG RUST_IMAGE=rust:1.30.0
ARG RUNTIME_IMAGE=gcr.io/linkerd-io/base:2017-10-30.01

## Builds the proxy as incrementally as possible.
FROM $RUST_IMAGE as build

WORKDIR /usr/src/linkerd2-proxy

# Fetch external dependencies.
RUN mkdir -p src && touch src/lib.rs
COPY Cargo.toml Cargo.lock ./
COPY lib lib
RUN cargo fetch --locked

# Build libraries, leaving the proxy mocked out.
ARG PROXY_UNOPTIMIZED
RUN if [ -n "$PROXY_UNOPTIMIZED" ]; \
    then cargo build --frozen ; \
    else cargo build --frozen --release ; \
    fi

# Build the proxy binary using the already-built dependencies.
COPY src src
RUN if [ -n "$PROXY_UNOPTIMIZED" ]; \
    then \
    cargo build -p linkerd2-proxy --bin linkerd2-proxy --frozen && \
    mv target/debug/linkerd1-proxy target/linkerd2-proxy ; \
    else \
    cargo build -p linkerd2-proxy --bin linkerd2-proxy --frozen --release && \
    mv target/release/linkerd2-proxy target/linkerd2-proxy ; \
    fi


## Install the proxy binary into the base runtime image.
FROM $RUNTIME_IMAGE as runtime
WORKDIR /linkerd
COPY --from=build /usr/src/linkerd2-proxy/target/linkerd2-proxy ./linkerd2-proxy
ENV LINKERD2_PROXY_LOG=warn,linkerd2_proxy=info
ENTRYPOINT ["/linkerd/linkerd2-proxy"]
