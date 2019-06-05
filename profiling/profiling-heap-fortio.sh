#!/bin/bash
set -o errexit
set -o nounset
set -o pipefail

# Configuration env vars and their defaults:
HIDE="${HIDE-1}"
ITERATIONS="${ITERATIONS-1}"
DURATION="${DURATION-6s}"
CONNECTIONS="${CONNECTIONS-4}"
GRPC_STREAMS="${GRPC_STREAMS-4}"
HTTP_RPS="${HTTP_RPS-4000}"
GRPC_RPS="${GRPC_RPS-4000}"
REQ_BODY_LEN="${REQ_BODY_LEN-200}"
TCP="${TCP-1}"
HTTP="${HTTP-1}"
GRPC="${GRPC-1}"

PROXY_PORT_OUTBOUND=4140
PROXY_PORT_INBOUND=4143
PROFDIR=$(dirname "$0")
ID=$(date +"%Y%h%d_%Hh%Mm%Ss")
LINKERD_TEST_BIN="../target/release/profiling-opt-and-dbg-symbols"

BRANCH_NAME=$(git symbolic-ref -q HEAD)
BRANCH_NAME=${BRANCH_NAME##refs/heads/}
BRANCH_NAME=${BRANCH_NAME:-HEAD}
BRANCH_NAME=$(echo $BRANCH_NAME | sed -e 's/\//-/g')
RUN_NAME="$BRANCH_NAME $ID Iter: $ITERATIONS Dur: $DURATION Conns: $CONNECTIONS Streams: $GRPC_STREAMS"

echo "File marker $RUN_NAME"

cd "$PROFDIR"
which fortio &> /dev/null || go get fortio.org/fortio
which fortio &> /dev/null || ( echo "fortio not found: Add the bin folder of your GOPATH to your PATH" ; exit 1 )
ls libmemory_profiler.so memory-profiler-cli  &> /dev/null || ( curl -L -O https://github.com/nokia/memory-profiler/releases/download/0.3.0/memory-profiler-x86_64-unknown-linux-gnu.tgz ; tar xf memory-profiler-x86_64-unknown-linux-gnu.tgz ; rm memory-profiler-x86_64-unknown-linux-gnu.tgz )
ls $LINKERD_TEST_BIN  &> /dev/null || ( echo "$LINKERD_TEST_BIN not found: Please run ./profiling-build.sh" ; exit 1 )

# Cleanup background processes when script is canceled
trap '{ killall iperf fortio memory-profiler-cli >& /dev/null; }' EXIT

# Summary table header
echo "Test, target req/s, req len, branch, p999 latency (ms), GBit/s" > "summary.$RUN_NAME.txt"

single_benchmark_run () {
  # run benchmark utilities in background, only proxy runs in foreground
  (
  # spawn test server in background
  SERVER="fortio server -ui-path ''"
  if [ "$MODE" = "TCP" ]; then
    SERVER="iperf -s -p $SERVER_PORT"
  fi
  $SERVER &> "$LOG" &
  SPID=$!
  # wait for service to start
  until ( ss -tan | grep "LISTEN.*:$SERVER_PORT" &> "$LOG" )
  do
    sleep 1
  done
  # wait for proxy to start
  until ( ss -tan | grep "LISTEN.*:$PROXY_PORT" &> "$LOG" )
  do
    sleep 1
  done
  # run client
  if [ "$MODE" = "TCP" ]; then
    echo "TCP $DIRECTION"
    ( iperf -t 6 -p "$PROXY_PORT" -c 127.0.0.1 || ( echo "iperf client failed" > /dev/stderr; true ) ) | tee "$NAME.$ID.txt" &> "$LOG"
    T=$(grep "/sec" "$NAME.$ID.txt" | cut -d' ' -f12)
    if [ -z "$T" ]; then
      T="0"
    fi
    echo "TCP $DIRECTION, 0, 0, $RUN_NAME, 0, $T" >> "summary.$RUN_NAME.txt"
  else
    RPS="$HTTP_RPS"
    XARG=""
    if [ "$MODE" = "gRPC" ]; then
      RPS="$GRPC_RPS"
      XARG="-grpc -s $GRPC_STREAMS"
    fi
    for l in $REQ_BODY_LEN; do
      for r in $RPS; do
        # Store maximum p999 latency of multiple iterations here
        S=0
        for i in $(seq $ITERATIONS); do
          echo "$MODE $DIRECTION Iteration: $i RPS: $r REQ_BODY_LEN: $l"
          fortio load $XARG -resolve 127.0.0.1 -c="$CONNECTIONS" -qps="$r" -t="$DURATION" -payload-size="$l" -labels="$RUN_NAME" -json="$NAME-$r-rps.$ID.json" -keepalive=false -H 'Host: transparency.test.svc.cluster.local' "localhost:$PROXY_PORT" &> "$LOG"
          T=$(tac "$NAME-$r-rps.$ID.json" | grep -m 1 Value | cut  -d':' -f2)
          if [ -z "$T" ]; then
            echo "No last percentile value found"
            exit 1
          fi
          S=$(python -c "print(max($S, $T*1000.0))")
        done
        echo "$MODE $DIRECTION, $r, $l, $RUN_NAME, $S, 0" >> "summary.$RUN_NAME.txt"
      done
    done
  fi
  # kill server
  kill $SPID || ( echo "test server failed"; true )
  # signal that proxy can terminate now
  (echo F | nc 127.0.0.1 7777 &> /dev/null) || true
  # wait for proxy to terminate
  while ( ss -tan | grep "LISTEN.*:$PROXY_PORT" &> "$LOG" )
  do
    sleep 1
  done
  # wait for service to terminate
  while ( ss -tan | grep "LISTEN.*:$SERVER_PORT" &> "$LOG" )
  do
    sleep 1
  done
  ) &
  rm memory-profiling_*.dat &> "$LOG" || true
  # run proxy in foreground
  PROFILING_SUPPORT_SERVER="127.0.0.1:$SERVER_PORT" LD_PRELOAD=./libmemory_profiler.so $LINKERD_TEST_BIN --exact profiling_setup --nocapture &> "$LOG" || echo "proxy failed"
  mv memory-profiling_*.dat "$NAME.$ID.heap.dat"
  ./memory-profiler-cli server "$NAME.$ID.heap.dat" &> "$LOG" &
  MPID=$!
  # wait for memory-profiler-cli server
  until ( ss -tan | grep "LISTEN.*:8080" &> "$LOG" )
  do
    sleep 1
  done
  curl http://localhost:8080/data/last/export/flamegraph/flame.svg > "heap-flamegraph_$NAME.$ID.svg" 2> "$LOG"
  kill $MPID || ( echo "memory-profiler failed"; true )
  ./memory-profiler-cli export-heaptrack "$NAME.$ID.heap.dat" --output "$NAME.$ID.heaptrack.dat" &> "$LOG"
}

if [ "$HIDE" -eq "1" ]; then
  LOG=/dev/null
else
  LOG=/dev/stdout
fi

if [ "$TCP" -eq "1" ]; then
  MODE=TCP DIRECTION=outbound NAME=tcpoutbound_heap PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=8080 single_benchmark_run
  MODE=TCP DIRECTION=inbound NAME=tcpinbound_heap PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=8080 single_benchmark_run
fi
if [ "$HTTP" -eq "1" ]; then
  MODE=HTTP DIRECTION=outbound NAME=http1outbound_heap PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=8080 single_benchmark_run
  MODE=HTTP DIRECTION=inbound NAME=http1inbound_heap PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=8080 single_benchmark_run
fi
if [ "$GRPC" -eq "1" ]; then
  MODE=gRPC DIRECTION=outbound NAME=grpcoutbound_heap PROXY_PORT=$PROXY_PORT_OUTBOUND SERVER_PORT=8079 single_benchmark_run
  MODE=gRPC DIRECTION=inbound NAME=grpcinbound_heap PROXY_PORT=$PROXY_PORT_INBOUND SERVER_PORT=8079 single_benchmark_run
fi
echo "Log files (display with 'head -vn-0 *$ID.txt *$ID.json | less'):"
ls ./*$ID*.txt
echo SUMMARY:
cat "summary.$RUN_NAME.txt"
echo "a) Inspect the memory allocation flamegraphs in the browser:"
ls *$ID*.svg
echo "b) Run './memory-profiler-cli server CHANGEME_heap.$ID.heap.dat' and open http://localhost:8080/ to browse the memory graphs"
echo "c) Run 'heaptrack -a CHANGEME_heap.$ID.heaptrack.dat' to open the heaptrack files for a detailed view."
echo "(Replace CHANGEME with http1inbound, http1outbound, grpcinbound, grpcoutbound, tcpinbound, tcpoutbound.)"
