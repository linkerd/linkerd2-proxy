#!/bin/bash
set -o errexit
set -o pipefail
set +x

if [ "$PROXY_PERF" -eq "1" ]; then
    (perf record -F 2000 -g /usr/lib/linkerd/linkerd2-proxy) > perf.data
    (perf script | inferno-collapse-perf) > "/out/out_$NAME.$ID.folded"  # separate step to be able to rerun flamegraph with another width if needed
    inferno-flamegraph --width 4000 "/out/out_$NAME.$ID.folded" > "/out/flamegraph_$NAME.$ID.svg"
else
    /usr/lib/linkerd/linkerd2-proxy
fi
