#!/bin/bash
set -o nounset
set -o pipefail

PROFDIR=$(dirname "$0")

cd "$PROFDIR"

source "profiling-util.sh"

status "Starting" "heap profile ${RUN_NAME}"

# Cleanup background processes when script is canceled
trap '{ teardown; }' EXIT

# Summary table header
echo "Test, target req/s, req len, branch, p999 latency (ms), GBit/s" > "$OUT_DIR/summary.txt"

export PROXY_HEAP=1;

if [ "$TCP" -eq "1" ]; then
  MODE=TCP DIRECTION=outbound NAME=tcpoutbound_bench PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=5001 single_benchmark_run
  MODE=TCP DIRECTION=inbound NAME=tcpinbound_bench PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=5001 single_benchmark_run
fi
if [ "$HTTP" -eq "1" ]; then
  MODE=HTTP DIRECTION=outbound NAME=http1outbound_bench PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=8080 single_benchmark_run
  MODE=HTTP DIRECTION=inbound NAME=http1inbound_bench PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=8080 single_benchmark_run
fi
if [ "$GRPC" -eq "1" ]; then
  MODE=gRPC DIRECTION=outbound NAME=grpcoutbound_bench PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=8079 single_benchmark_run
  MODE=gRPC DIRECTION=inbound NAME=grpcinbound_bench PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=8079 single_benchmark_run
fi
teardown

status "Completed" "Log files (display with 'head -vn-0 $OUT_DIR/*.txt $OUT_DIR/*.json | less'):"
ls "$OUT_DIR"/*.txt "$OUT_DIR"/*.json
echo SUMMARY:
cat "$OUT_DIR"/summary.txt

echo "a) Inspect the memory allocation flamegraphs in the browser:"
ls "$OUT_DIR"/*heap_flamegraph.svg
echo "b) Run './memory-profiler-cli server $OUT_DIR/CHANGEME.heap.dat' and open http://localhost:8080/ to browse the memory graphs"
echo "c) Run 'heaptrack -a $OUT_DIR/.heaptrack.dat' to open the heaptrack files for a detailed view."
echo "(Replace CHANGEME with http1inbound, http1outbound, grpcinbound, grpcoutbound, tcpinbound, tcpoutbound.)"
