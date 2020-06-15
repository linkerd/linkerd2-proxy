#!/bin/bash -e
set -o nounset
set -o pipefail

PROFDIR=${0%/*}

cd "$PROFDIR"

source profiling-util.sh
if [[ "$OSTYPE" == "darwin"* ]]; then
  err 'you are trying to run perf on a mac' "This is not guaranteed to work in Docker for Mac.
If you're lucky, it might, so I will continue running the test."
else
  PARANOIA=$(cat /proc/sys/kernel/perf_event_paranoid)
  if [ "$PARANOIA" -eq -1 ]; then
    err 'you may not have permission to collect stats' "Consider tweaking /proc/sys/kernel/perf_event_paranoid:
 -1 - Not paranoid at all
  0 - Disallow raw tracepoint access for unpriv
  1 - Disallow cpu events for unpriv
  2 - Disallow kernel profiling for unpriv

You can adjust this value with:
  echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid

Current value is $PARANOIA"
    exit 1
  fi
fi

status 'Starting' "perf profile ${RUN_NAME}"

# Cleanup background processes when script is canceled
trap '{ teardown; }' EXIT

# Summary table header
echo 'Test, target req/s, req len, branch, p999 latency (ms), GBit/s' > "$OUT_DIR/summary.txt"

export PROXY_PERF=1;

if [ "$TCP" -eq 1 ]; then
  MODE=TCP DIRECTION=outbound NAME=tcpoutbound_bench PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=5001 single_benchmark_run
  MODE=TCP DIRECTION=inbound NAME=tcpinbound_bench PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=5001 single_benchmark_run
fi
if [ "$HTTP" -eq 1 ]; then
  MODE=HTTP DIRECTION=outbound NAME=http1outbound_bench PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=8080 single_benchmark_run
  MODE=HTTP DIRECTION=inbound NAME=http1inbound_bench PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=8080 single_benchmark_run
fi
if [ "$GRPC" -eq 1 ]; then
  MODE=gRPC DIRECTION=outbound NAME=grpcoutbound_bench PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=8079 single_benchmark_run
  MODE=gRPC DIRECTION=inbound NAME=grpcinbound_bench PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=8079 single_benchmark_run
fi
teardown

status 'Completed' "Log files (display with 'head -vn-0 $OUT_DIR/.txt $OUT_DIR/*.json | less'):"
ls "$OUT_DIR"/*.txt "$OUT_DIR"/*.json
echo SUMMARY:
cat "$OUT_DIR"/summary.txt
status 'Completed' 'inspect flamegraphs in browser:'
ls "$OUT_DIR"/*.svg

