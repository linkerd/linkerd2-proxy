#!/bin/bash
set -o errexit
set -o pipefail
set +x

if [[ ! -z "$PROXY_PERF" ]]; then
    (perf record -F 2000 -g /usr/lib/linkerd/linkerd2-proxy) > perf.data
    (perf script | inferno-collapse-perf) > "/out/${NAME}.folded"
    inferno-flamegraph --width 4000 "/out/${NAME}.folded" > "/out/${NAME}_flamegraph.svg"
elif [[ ! -z "$PROXY_HEAP" ]]; then
    /usr/lib/linkerd/linkerd2-proxy
else
    LD_PRELOAD=/usr/lib/libmemory_profiler.so /usr/lib/linkerd/linkerd2-proxy
    mv memory-profiling_*.dat "/out/${NAME}_heap.dat"

    memory-profiler-cli server "${NAME}_heap.dat" &
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
fi
