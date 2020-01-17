#!/bin/bash
set -o errexit
set -o nounset
set -o pipefail

PROFDIR=$(dirname "$0")

source "$PROFDIR/profiling-util.sh"

echo "File marker $RUN_NAME"

cd "$PROFDIR"

dep_fortio || exit 1

# Cleanup background processes when script is canceled
trap '{ killall iperf fortio >& /dev/null; }' EXIT

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
  until ( port_open "$SERVER_PORT" )
  do
    sleep 1
  done
  # wait for proxy to start
  until ( port_open "$PROXY_PORT" )
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
          T=$(rev "$NAME-$r-rps.$ID.json" | grep -m 1 Value | cut  -d':' -f2)
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
  while ( port_open "$PROXY_PORT" )
  do
    sleep 1
  done
  # wait for service to terminate
  while ( port_open "$SERVER_PORT" )
  do
    sleep 1
  done
  ) &
  # run proxy in foreground
  PROFILING_SUPPORT_SERVER="127.0.0.1:$SERVER_PORT" cargo run --release --bin profile &> "$LOG" || echo "proxy failed"
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
