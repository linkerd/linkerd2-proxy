ARG DEV_VERSION=v21

FROM ghcr.io/linkerd/dev:${DEV_VERSION}

RUN rustup toolchain install --profile=minimal nightly
RUN cargo +nightly install cargo-fuzz

RUN curl --proto '=https' --tlsv1.3 -vsSfL https://run.linkerd.io/install-edge | sh
ENV PATH=$HOME/.linkerd2/bin:$PATH

ENTRYPOINT ["/usr/local/share/docker-init.sh"]
CMD ["sleep", "infinity"]
