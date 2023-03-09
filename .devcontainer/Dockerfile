FROM ghcr.io/linkerd/dev:v39

RUN rustup toolchain install --profile=minimal nightly

RUN cargo +nightly install cargo-fuzz

RUN scurl https://run.linkerd.io/install-edge | sh

RUN mkdir -p "$HOME/bin" && ln -s "$HOME/.linkerd2/bin/linkerd" "$HOME/bin/linkerd"
