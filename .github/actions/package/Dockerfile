ARG BASE_IMAGE=rust:1.55.0-buster
FROM $BASE_IMAGE
WORKDIR /linkerd
RUN apt-get update && \
    apt-get install -y jq && \
    apt-get install -y --no-install-recommends g++-aarch64-linux-gnu libc6-dev-arm64-cross && \
    rustup target add aarch64-unknown-linux-gnu && \
    apt-get install -y --no-install-recommends g++-arm-linux-gnueabihf libc6-dev-armhf-cross && \
    rustup target add armv7-unknown-linux-gnueabihf && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
ENV CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc
# v2.1.0
ARG CHECKSEC_SHA=04582bad41589ad479ca8b1f0170ed317475b5a5
RUN cd /usr/local/bin && curl -vsLO "https://raw.githubusercontent.com/slimm609/checksec.sh/$CHECKSEC_SHA/checksec" && chmod 755 checksec
COPY expected-checksec.json validate-checksec.sh /linkerd/
