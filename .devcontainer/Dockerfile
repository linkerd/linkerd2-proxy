ARG RUST_VERSION=1.62.1

FROM docker.io/rust:${RUST_VERSION}-bullseye as kubectl
ARG KUBECTL_VERSION=v1.24.2
RUN curl --proto '=https' --tlsv1.3 -vsSfLo /usr/local/bin/kubectl "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/amd64/kubectl" \
    && chmod 755 /usr/local/bin/kubectl

FROM docker.io/rust:${RUST_VERSION}-bullseye as k3d
ARG K3D_VERSION=v5.4.4
RUN cd /usr/local/bin \
    && curl --proto '=https' --tlsv1.3 -vsSfLO "https://github.com/k3d-io/k3d/releases/download/${K3D_VERSION}/k3d-linux-amd64" \
    && mv k3d-linux-amd64 k3d \
    && chmod 755 k3d

FROM docker.io/rust:${RUST_VERSION}-bullseye as deny
ARG DENY_VERSION=0.12.1
RUN curl --proto '=https' --tlsv1.3 -vsSfL "https://github.com/EmbarkStudios/cargo-deny/releases/download/${DENY_VERSION}/cargo-deny-${DENY_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar zvxf - --strip-components=1 -C /usr/local/bin "cargo-deny-${DENY_VERSION}-x86_64-unknown-linux-musl/cargo-deny"

FROM docker.io/rust:${RUST_VERSION}-bullseye as yq
ARG YQ_VERSION=v4.25.1
RUN curl --proto '=https' --tlsv1.3 -vsSfLo /usr/local/bin/yq "https://github.com/mikefarah/yq/releases/download/${YQ_VERSION}/yq_linux_amd64" \
    && chmod +x /usr/local/bin/yq

FROM docker.io/rust:${RUST_VERSION}-bullseye as checksec
ARG CHECKSEC_VERSION=2.5.0
RUN cd /usr/local/bin \
    && curl --proto '=https' --tlsv1.3 -vsSfLO "https://raw.githubusercontent.com/slimm609/checksec.sh/${CHECKSEC_VERSION}/checksec" \
    && chmod 755 checksec

FROM docker.io/rust:${RUST_VERSION}-bullseye as just
ARG JUST_VERSION=1.2.0
RUN curl --proto '=https' --tlsv1.3 -vsSfL "https://github.com/casey/just/releases/download/${JUST_VERSION}/just-${JUST_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar zvxf - -C /usr/local/bin just

FROM docker.io/rust:${RUST_VERSION}-bullseye as nextest
ARG NEXTEST_VERSION=0.9.24
RUN curl --proto '=https' --tlsv1.3 -vsSfL "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-${NEXTEST_VERSION}/cargo-nextest-${NEXTEST_VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
    | tar zvxf - -C /usr/local/bin cargo-nextest

FROM docker.io/rust:${RUST_VERSION}-bullseye as nightly
RUN rustup toolchain add nightly

FROM nightly as fuzz
RUN cargo +nightly install cargo-fuzz

FROM docker.io/rust:${RUST_VERSION}-bullseye as protoc
ARG PROTOC_VERSION=v3.20.1
WORKDIR /tmp
RUN arch="$(uname -m)" ; \
    version="$PROTOC_VERSION" ; \
    curl --proto '=https' --tlsv1.3 -vsSfLo protoc.zip  "https://github.com/google/protobuf/releases/download/$version/protoc-${version#v}-linux-$arch.zip" && \
    unzip protoc.zip bin/protoc && \
    chmod 755 bin/protoc

#
# Main image
#
FROM docker.io/rust:${RUST_VERSION}-bullseye
RUN rustup component add clippy rustfmt rust-analysis rust-std

ENV DEBIAN_FRONTEND=noninteractive
RUN apt update && apt upgrade -y
RUN apt install -y --no-install-recommends \
    clang \
    cmake \
    golang \
    jo \
    jq \
    locales \
    lsb-release \
    shellcheck \
    sudo \
    time

RUN sed -i 's/^# *\(en_US.UTF-8\)/\1/' /etc/locale.gen && locale-gen

ARG USER=code
ARG USER_UID=1000
ARG USER_GID=1000
RUN groupadd --gid=$USER_GID $USER \
    && useradd --uid=$USER_UID --gid=$USER_GID -m $USER \
    && echo "$USER ALL=(root) NOPASSWD:ALL" >/etc/sudoers.d/$USER \
    && chmod 0440 /etc/sudoers.d/$USER

# Install a Docker client that uses the host's Docker daemon
ARG USE_MOBY=false
ENV DOCKER_BUILDKIT=1
RUN curl --proto '=https' --tlsv1.3 -vsSfL https://raw.githubusercontent.com/microsoft/vscode-dev-containers/main/script-library/docker-debian.sh \
    | bash -s -- true /var/run/docker-host.sock /var/run/docker.sock "${USER}" "${USE_MOBY}" latest

USER $USER
ENV HOME=/home/$USER

RUN curl --proto '=https' --tlsv1.3 -vsSfL https://run.linkerd.io/install-edge | sh
ENV PATH=$HOME/.linkerd2/bin:$PATH

COPY --from=deny /usr/local/bin/cargo-deny /usr/local/bin/cargo-deny
COPY --from=k3d /usr/local/bin/k3d /usr/local/bin/k3d
COPY --from=kubectl /usr/local/bin/kubectl /usr/local/bin/kubectl
COPY --from=yq /usr/local/bin/yq /usr/local/bin/yq
COPY --from=checksec /usr/local/bin/checksec /usr/local/bin/checksec
COPY --from=nightly /usr/local/rustup/toolchains/nightly-x86_64-unknown-linux-gnu /usr/local/rustup/toolchains/nightly-x86_64-unknown-linux-gnu
COPY --from=fuzz /usr/local/cargo/bin/cargo-fuzz /usr/local/cargo/bin/cargo-fuzz
COPY --from=just /usr/local/bin/just /usr/local/bin/just
COPY --from=nextest /usr/local/bin/cargo-nextest /usr/local/bin/cargo-nextest

COPY --from=protoc /tmp/bin/protoc /usr/local/bin/protoc
ENV PROTOC_NO_VENDOR=1
ENV PROTOC=/usr/local/bin/protoc

ENTRYPOINT ["/usr/local/share/docker-init.sh"]
CMD ["sleep", "infinity"]
