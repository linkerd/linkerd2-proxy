ARG BASE_IMAGE=rust:1.41.0-buster
FROM $BASE_IMAGE
WORKDIR /linkerd
RUN apt-get update && \
    apt-get install -y jq && \
    apt-get clean && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/
# v2.1.0
ARG CHECKSEC_SHA=04582bad41589ad479ca8b1f0170ed317475b5a5
RUN cd /usr/local/bin && curl -vsLO "https://raw.githubusercontent.com/slimm609/checksec.sh/$CHECKSEC_SHA/checksec" && chmod 755 checksec
COPY expected-checksec.json validate-checksec.sh /linkerd/
