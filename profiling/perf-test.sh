#!/bin/bash
set -o errexit
set -o nounset
set -o pipefail

PROFDIR=$(dirname "$0")

source "$PROFDIR/profiling-util.sh"

echo "File marker $RUN_NAME"

cd "$PROFDIR"

# dep_fortio || exit 1

# Cleanup background processes when script is canceled
trap '{ docker-compose down -t 5; }' EXIT

# Summary table header
echo "Test, target req/s, req len, branch, p999 latency (ms), GBit/s" > "summary.$RUN_NAME.txt"

single_perf_run () {
  export PRECMD="perf record -F 2000 --call-graph dwarf"
  single_benchmark_run &&
  (docker-compose exec proxy perf script | inferno-collapse-perf) > "out_$NAME.$ID.folded"
}


if [ "$TCP" -eq "1" ]; then
  MODE=TCP DIRECTION=outbound NAME=tcpoutbound_bench PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=8080 single_benchmark_run
  MODE=TCP DIRECTION=inbound NAME=tcpinbound_bench PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=8080 single_benchmark_run
fi
if [ "$HTTP" -eq "1" ]; then
  MODE=HTTP DIRECTION=outbound NAME=http1outbound_bench PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=8080 single_benchmark_run
  MODE=HTTP DIRECTION=inbound NAME=http1inbound_bench PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=8080 single_benchmark_run
fi
if [ "$GRPC" -eq "1" ]; then
  MODE=gRPC DIRECTION=outbound NAME=grpcoutbound_bench PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=8079 single_benchmark_run
  MODE=gRPC DIRECTION=inbound NAME=grpcinbound_bench PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=8079 single_benchmark_run
fi
echo "Benchmark results (display with 'head -vn-0 *$ID.txt *$ID.json | less' or compare them with ./plot.py):"
ls ./*$ID*.txt
echo SUMMARY:
cat "summary.$RUN_NAME.txt"
echo "Run 'fortio report' and open http://localhost:8080/ to display the HTTP/gRPC graphs"
docker-compose down -t 5
