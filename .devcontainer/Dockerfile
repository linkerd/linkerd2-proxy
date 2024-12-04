ARG DEV_VERSION

FROM ghcr.io/linkerd/dev:${DEV_VERSION}
RUN scurl https://run.linkerd.io/install-edge | sh && \
    mkdir -p "$HOME/bin" && ln -s "$HOME/.linkerd2/bin/linkerd" "$HOME/bin/linkerd"

ENV RUSTFLAGS="--cfg tokio_unstable"

# XXX(ver) This doesn't currently work, because it puts
# /usr/local/cargo/registry into a weird state with regard to permissions.
#RUN rustup toolchain install --profile=minimal nightly
#RUN cargo +nightly install cargo-fuzz
