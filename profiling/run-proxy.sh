#!/bin/bash
set -o errexit
set -o pipefail
set -x

export PROFILING_SUPPORT_SERVER="127.0.0.1:$SERVER_PORT"

while ! nc -z "$SERVER" "$SERVER_PORT"; do
  sleep 0.1
done

# this is a terrible hack to work around how the inbound proxy
# rewrites all addresses to localhost, since docker-compose assigns IP addresses
# to all containers running on its network: we open a SSH tunnel to the server's
# container to port-forward the target port for the test to localhost.
ssh -o "StrictHostKeyChecking=no" \
    -f -N -4 \
    -L "$SERVER_PORT:127.0.0.1:$SERVER_PORT" \
    "$SERVER" &

if [[ ! -z "$PROXY_PERF" ]]; then
    (perf record -F 2000 -g /usr/lib/linkerd/linkerd2-proxy) > perf.data
    (perf script | inferno-collapse-perf) > "/out/${NAME}.folded"
    inferno-flamegraph --width 4000 "/out/${NAME}.folded" > "/out/${NAME}_flamegraph.svg"
elif [[ ! -z "$PROXY_HEAP" ]]; then
    LD_PRELOAD=/usr/lib/libmemory_profiler.so /usr/lib/linkerd/linkerd2-proxy
    mv memory-profiling_*.dat "/out/${NAME}_heap.dat"

    memory-profiler-cli server "/out/${NAME}_heap.dat" &
    MPID=$!
    # wait for memory-profiler-cli server
    until ( ss -tan | grep "LISTEN.*:8080" &> /dev/null)
    do
        sleep 1
    done
    curl http://localhost:8080/data/last/export/flamegraph/flame.svg \
        > "/out/${NAME}_heap_flamegraph.svg"
    kill $MPID || ( echo "memory-profiler failed"; true )

    memory-profiler-cli export-heaptrack \
        "/out/${NAME}_heap.dat" \
        --output "/out/${NAME}_heaptrack.dat"
else
    /usr/lib/linkerd/linkerd2-proxy
fi
