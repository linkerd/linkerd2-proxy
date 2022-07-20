ARG DEV_VERSION=v19

FROM ghcr.io/linkerd/dev:${DEV_VERSION}-tools as nightly
RUN rustup toolchain install --profile=minimal nightly
RUN cargo +nightly install cargo-fuzz

FROM ghcr.io/linkerd/dev:${DEV_VERSION}-tools
COPY --from=nightly /usr/local/cargo/bin/cargo-fuzz /usr/local/cargo/bin/cargo-fuzz
COPY --from=nightly /usr/local/rustup/toolchains/nightly-x86_64-unknown-linux-gnu /usr/local/rustup/toolchains/nightly-x86_64-unknown-linux-gnu

ENV DOCKER_BUILDKIT=1
RUN groupadd --gid=1000 code \
    && useradd --create-home --uid=1000 --gid=1000 code \
    && echo "code ALL=(root) NOPASSWD:ALL" >/etc/sudoers.d/code \
    && chmod 0440 /etc/sudoers.d/code \
    && scurl https://raw.githubusercontent.com/microsoft/vscode-dev-containers/main/script-library/docker-debian.sh | bash -s \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

RUN chmod 777 $CARGO_HOME $RUSTUP_HOME
USER code
ENV USER=code
ENV HOME=/home/code

RUN curl --proto '=https' --tlsv1.3 -vsSfL https://run.linkerd.io/install-edge | sh
ENV PATH=$HOME/.linkerd2/bin:$PATH

ENTRYPOINT ["/usr/local/share/docker-init.sh"]
CMD ["sleep", "infinity"]
