# Intended to be run on a remote DOCKER_HOST
ARG RUST_IMAGE=rust:1.44.1-buster
FROM $RUST_IMAGE as build
WORKDIR /usr/src/linkerd2-proxy
COPY . ./
RUN ["make", "fetch"]
ENTRYPOINT ["make"]
