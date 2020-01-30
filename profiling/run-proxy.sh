#!/bin/bash
set -o errexit
set -o nounset
set -o pipefail

declare -a PRECMD
PRECMD=()

if [ "$PROXY_HEAPTRACK" -eq "1" ]; then
    PRECMD+=(env "LD_PRELOAD=/libmemory_profiler.so")
fi
if [ "$PROXY_PERF" -eq "1" ]; then
    PRECMD+=(perf record -F 2000 -g )
fi

"${PRECMD[@]}" /usr/lib/linkerd2_proxy

if [ "$PROXY_PERF" -eq "1" ]; then
  perf script | inferno-collapse-perf > "/out/out_$NAME.$ID.folded"  # separate step to be able to rerun flamegraph with another width if needed
  inferno-flamegraph --width 4000 "out/out_$NAME.$ID.folded" > "/out/flamegraph_$NAME.$ID.svg"
fi
