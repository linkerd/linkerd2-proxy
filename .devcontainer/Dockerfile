ARG DEV_VERSION

FROM ghcr.io/linkerd/dev:${DEV_VERSION}
RUN rustup toolchain install --profile=minimal nightly && \
    cargo +nightly install cargo-fuzz
RUN scurl https://run.linkerd.io/install-edge | sh && \
    mkdir -p "$HOME/bin" && ln -s "$HOME/.linkerd2/bin/linkerd" "$HOME/bin/linkerd"
