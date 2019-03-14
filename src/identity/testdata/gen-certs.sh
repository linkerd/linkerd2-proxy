#!/bin/bash
#
# Requires:
# go get -u github.com/cloudflare/cfssl/cmd/cfssl
# go get -u github.com/cloudflare/cfssl/cmd/cfssljson
#
set -euox pipefail

ca() {
  name=$1
  filename=$2

  echo "{\"names\":[{\"CN\": \"${name}\",\"OU\":\"None\"}]}" \
    | cfssl genkey -initca - \
    | cfssljson -bare "${filename}"

  rm "${filename}.csr"
}

ee() {
  ca_name=$1
  ee_name=$2
  ee_namespace=$3
  cp_namespace=$4

  ee_name="${ee_name}-${ee_namespace}"
  hostname="${ee_name}.${ee_namespace}.serviceaccount.identity.${cp_namespace}.cluster.local"

  echo '{}' \
    | cfssl gencert -ca "${ca_name}.pem" -ca-key "${ca_name}-key.pem" -hostname "${hostname}" - \
    | cfssljson -bare "${ee_name}"

  openssl pkcs8 -topk8 -nocrypt -inform pem -outform der \
    -in "${ee_name}-key.pem" \
    -out "${ee_name}-${ca_name}.p8"
  rm "${ee_name}-key.pem"

  openssl x509 -inform pem -outform der \
    -in "${ee_name}.pem" \
    -out "${ee_name}-${ca_name}.crt"
  rm "${ee_name}.pem"
}

ca "Cluster-local CA 1" ca1
ca "Cluster-local CA 1" ca2 # Same name, different key pair.

# The controller itself.
ee ca1 controller linkerd linkerd

ee ca1 foo ns1 linkerd
ee ca2 foo ns1 linkerd # Same, but different CA
ee ca1 bar ns1 linkerd # Different service.
