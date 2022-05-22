ARG RUST_VERSION=1.60.0
ARG BASE_IMAGE=rust:${RUST_VERSION}-bullseye
FROM $BASE_IMAGE
WORKDIR /linkerd
RUN apt-get update && \
    apt-get install -y jq
ARG CHECKSEC_VERSION=2.5.0
RUN cd /usr/local/bin && \
    curl -vsLO "https://raw.githubusercontent.com/slimm609/checksec.sh/${CHECKSEC_VERSION}/checksec" && \
    chmod 755 checksec
COPY entrypoint.sh expected-checksec.json /linkerd/
ENTRYPOINT ["/linkerd/entrypoint.sh"]
