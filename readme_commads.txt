root@10.12.0.106
#snap install just --classic #might not be necessary
apt-get install jq
apt-get install cmake
apt-get install libclang-dev
 git clone https://github.com/linkerd/dev.git
 git clone https://github.com/praagarw/linkerd2-proxy

Setup just
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

export PATH=~/pranav/dev/bin:$PATH
cd linkerd2-proxy
just build
export RUSTFLAGS='--cfg tokio_unstable'
cargo auditable build  --target=x86_64-unknown-linux-gnu  --package=linkerd2-proxy
 cargo install cargo-nextest --locked

rustup target add x86_64-unknown-linux-musl
sudo apt update
sudo apt install musl-tools
sudo apt install g++-multilib
cargo auditable build --release --target x86_64-unknown-linux-musl

rustup target add x86_64-unknown-linux-gnu
cargo auditable build --release --target x86_64-unknown-linux-gnu


Setup K8s
curl -sfL https://get.k3s.io | sh -
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
sudo kubectl get nodes


Linkerd Docker file
.dockerignore -> !target/x86_64-unknown-linux-gnu/debug/linkerd2-proxy
Dockerfile
FROM scratch

# Copy your compiled binary from the build target directory
# Adjust this path based on where 'just build' placed the final binary
COPY target/x86_64-unknown-linux-gnu/debug/linkerd2-proxy /linkerd-proxy

# Define the entrypoint
ENTRYPOINT ["/linkerd-proxy"]

just build
just docker
docker build -f pranav_Dockerfile -t pranav/linkerd-proxy:myntra_1.0 .

 docker run --rm -it --entrypoint sh 10.12.0.106:5000/linkerd-proxy:myntra1.0
ldd /linkerd-proxy

Run docker registry

docker run -d -p 5000:5000 --restart=always --name local-registry registry:2

curl http://localhost:5000/v2/_catalog
docker tag pranav/linkerd-proxy:myntra_1.0 localhost:5000/linkerd-proxy:myntra_1.0
docker push localhost:5000/linkerd-proxy:myntra_1.0
curl http://localhost:5000/v2/_catalog
{"repositories":["linkerd-proxy"]}


docker tag pranav/linkerd-proxy:myntra_1.0 10.12.0.106:5000/linkerd-proxy:myntra1.0
docker tag 5cf21111e8f3 10.12.0.106:5000/linkerd-proxy:myntra1.0
docker push 10.12.0.106:5000/linkerd-proxy:myntra1.0
docker images

docker run --rm -it --entrypoint sh 10.12.0.106:5000/linkerd-proxy:myntra1.0



curl -sL https://run.linkerd.io/install | sh
export PATH=$PATH:/root/.linkerd2/bin
linkerd check --pre

linkerd uninstall | kubectl delete -f -

linkerd install --crds | kubectl apply -f -
linkerd install \
  --set global.proxy.image.name=10.12.0.106:5000/linkerd-proxy \
  --set global.proxy.image.version=myntra1.0 \
  --set global.proxy.image.pullPolicy=Always \
  | kubectl apply -f -

linkerd upgrade \
  --set global.proxy.image.name=10.12.0.106:5000/linkerd-proxy \
  --set global.proxy.image.version=myntra1.0 \
  --set global.proxy.image.pullPolicy=Always \
  | kubectl apply -f -

kubectl rollout status deployment linkerd-proxy-injector -n linkerd
kubectl rollout restart deployment linkerd-proxy-injector -n linkerd

 kubectl get pods -n linkerd

linkerd check

kubectl delete deployment listener -n custom-test
kubectl delete deployment emitter -n custom-test
kubectl get pods -n custom-test

kubectl delete deployment listener -n nfr-test
kubectl delete deployment emitter -n nfr-test

linkerd uninstall | kubectl delete -f -




kubectl create ns custom-test
kubectl annotate namespace custom-test linkerd.io/inject=enabled

kubectl create ns nfr-test
kubectl annotate namespace nfr-test linkerd.io/inject=enabled
kubectl annotate namespace nfr-test config.linkerd.io/proxy-version="myntra1.0" --overwrite
kubectl annotate namespace nfr-test config.linkerd.io/proxy-image="10.12.0.106:5000/linkerd-proxy" --overwrite


kubectl apply -f listener.yaml
kubectl apply -f emitter.yaml

kubectl rollout restart deployment listener -n custom-test
kubectl rollout restart deployment emitter -n custom-test

kubectl rollout restart deployment listener -n nfr-test
kubectl rollout restart deployment emitter -n nfr-test


kubectl -n custom-test patch deployment emitter --patch-file linkerd-env-patch.yaml --type strategic


EMITTER_POD=$(kubectl get pod -n custom-test -l app=emitter  -o jsonpath='{.items[0].metadata.name}')

LISTENER_POD=$(kubectl get pod -n custom-test -l app=listener -o jsonpath='{.items[0].metadata.name}')

kubectl logs $LISTENER_POD -n custom-test -c linkerd-proxy -f
 kubectl -n custom-test describe pod $EMITTER_POD

kubectl exec -it -n custom-test $EMITTER_POD -c emitter-app  -- /bin/sh
curl -s http://listener-svc.custom-test.svc.cluster.local
curl -s -H "myntra-nfr-test: TRUE" http://listener-svc.custom-test.svc.cluster.local 


vi /etc/rancher/k3s/registries.yaml 
mirrors: "10.12.0.106:5000": endpoint: - "http://10.12.0.106:5000" insecure: true
sudo systemctl restart k3s



CUSTOM_PROXY_IMAGE="10.12.0.106:5000/linkerd-proxy:custom-v1"
kubectl patch deployment linkerd-proxy-injector -n linkerd \
    --type='json' \
    -p='[{"op": "replace", "path": "/spec/template/spec/containers/0/image", "value": "'$CUSTOM_PROXY_IMAGE'"}]'



vi /etc/docker/daemon.json
{
  "insecure-registries": [
    "10.12.0.106:5000"
  ]
}

docker push 10.12.0.106:5000/linkerd-proxy:myntra1.0
curl -s 10.12.0.106:5000/v2/linkerd-proxy/manifests/myntra1.0

linkerd viz install | kubectl apply -f -


