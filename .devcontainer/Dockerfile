FROM docker.io/rust:1.56.1-bullseye

ENV DEBIAN_FRONTEND=noninteractive
RUN apt update && apt upgrade -y
RUN apt install -y --no-install-recommends \
    clang \
    cmake \
    golang \
    jq \
    lldb \
    lsb-release \
    sudo \
    time

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
RUN mkdir -p $HOME/bin
ENV PATH=$HOME/bin:$PATH

ARG KUBECTL_VERSION=v1.23.2
RUN curl --proto '=https' --tlsv1.3 -vsSfLo $HOME/bin/kubectl "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/amd64/kubectl" \
    && chmod 755 $HOME/bin/kubectl
RUN curl --proto '=https' --tlsv1.3 -vsSfL https://raw.githubusercontent.com/rancher/k3d/main/install.sh \
    | USE_SUDO=false K3D_INSTALL_DIR=$HOME/bin bash

RUN rustup component add clippy rustfmt

ARG CARGO_DENY_VERSION=0.11.0
RUN curl --proto '=https' --tlsv1.3 -vsSfL "https://github.com/EmbarkStudios/cargo-deny/releases/download/${CARGO_DENY_VERSION}/cargo-deny-${CARGO_DENY_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar zvxf - --strip-components=1 -C $HOME/bin "cargo-deny-${CARGO_DENY_VERSION}-x86_64-unknown-linux-musl/cargo-deny"

RUN curl --proto '=https' --tlsv1.3 -vsSfL https://run.linkerd.io/install-edge | sh \
    && ln -s $(readlink ~/.linkerd2/bin/linkerd) ~/bin/linkerd

ENTRYPOINT ["/usr/local/share/docker-init.sh"]
CMD ["sleep", "infinity"]
