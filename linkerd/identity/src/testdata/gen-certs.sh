#!/bin/sh
#
# Requires:
# go get -u github.com/cloudflare/cfssl/cmd/cfssl
# go get -u github.com/cloudflare/cfssl/cmd/cfssljson
#
set -euox pipefail

ca() {
  name=$1
  filename=$2

  echo "{\"names\":[{\"CN\": \"${name}\",\"OU\":\"None\"}], \"ca\": {\"expiry\": \"87600h\"}}" \
    | cfssl genkey -initca - \
    | cfssljson -bare "${filename}"

  rm "${filename}.csr"
}

ee() {
  ca_name=$1
  ee_name=$2
  ee_ns=$3
  cp_ns=$4

  hostname="${ee_name}.${ee_ns}.serviceaccount.identity.${cp_ns}.cluster.local"

  ee="${ee_name}-${ee_ns}-${ca_name}"
  echo '{}' \
    | cfssl gencert -ca "${ca_name}.pem" -ca-key "${ca_name}-key.pem" -hostname "${hostname}" -config=ca-config.json - \
    | cfssljson -bare "${ee}"
  mkdir -p "${ee}"

  openssl pkcs8 -topk8 -nocrypt -inform pem -outform der \
    -in "${ee}-key.pem" \
    -out "${ee}/key.p8"
  rm "${ee}-key.pem"

  openssl x509 -inform pem -outform der \
    -in "${ee}.pem" \
    -out "${ee}/crt.der"
  rm "${ee}.pem"

  ## TODO DER-encode?
  #openssl x509 -inform pem -outform der \
  #  -in "${ee}.csr" \
  #  -out "${ee}/csr.der"
  mv "${ee}.csr" "${ee}/csr.pem"
}

ca "Cluster-local CA 1" ca1
ca "Cluster-local CA 1" ca2 # Same name, different key pair.

# The controller itself.
ee ca1 controller linkerd linkerd

ee ca1 default default linkerd
ee ca1 foo ns1 linkerd
ee ca2 foo ns1 linkerd # Same, but different CA
ee ca1 bar ns1 linkerd # Different service.
