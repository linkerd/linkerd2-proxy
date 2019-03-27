#!/bin/bash
#
# Requires:
# go get -u github.com/cloudflare/cfssl/cmd/cfssl
# go get -u github.com/cloudflare/cfssl/cmd/cfssljson
#
set -euox pipefail

ca() {
  cn=$1
  name=$2
  echo "{\"names\":[{\"CN\": \"${cn}\",\"OU\":\"None\"}]}" \
    | cfssl genkey -initca - \
    | cfssljson -bare "${name}"

  rm "${name}.csr"
}

ee() {
  ca_name=$1
  ee_serviceaccount=$2
  ee_namespace=$3
  controller_namespace=$4

  ee_name=${ee_serviceaccount}-${ee_namespace}
  hostname=${ee_serviceaccount}.${ee_namespace}.serviceaccount.identity.${controller_namespace}.cluster.local

  mkdir -p "${ee_name}"
  echo '{}' \
    | cfssl gencert -ca "${ca_name}.pem" -ca-key "${ca_name}-key.pem" -hostname="${hostname}" - \
    | cfssljson -bare "${ee_name}"

  openssl pkcs8 -topk8 -nocrypt -inform pem -outform der \
    -in "${ee_name}-key.pem" \
    -out "${ee_name}/key.p8"

  rm "${ee_name}.csr"
  rm "${ee_name}-key.pem"
  rm "${ee_name}.pem"
}

ca "Cluster-local CA 1" ca1
# ca "Cluster-local CA 1" ca2 # Same name, different key pair.

# The controller itself.
# ee ca1 controller linkerd linkerd

ee ca1 foo ns1 linkerd
# ee ca2 foo ns1 linkerd # Same, but different CA
ee ca1 bar ns1 linkerd # Different service.
