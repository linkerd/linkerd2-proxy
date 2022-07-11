ARG RUST_VERSION=1.62.0
FROM docker.io/library/rust:${RUST_VERSION}-bullseye
ENV DEBIAN_FRONTEND=noninteractive
RUN apt update && \
    apt install -y jq && \
    rm -rf /var/lib/apt/lists/*
COPY entrypoint.sh /
ENTRYPOINT ["/entrypoint.sh"]
