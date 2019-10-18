# Intended to be run on a remote DOCKER_HOST
ARG RUST_IMAGE=rust:1.37.0-buster
FROM $RUST_IMAGE as build
WORKDIR /usr/src/linkerd2-proxy
COPY . ./
RUN ["make", "fetch"]
ENV RUSTFLAGS="-D warnings"
ENTRYPOINT ["make"]
