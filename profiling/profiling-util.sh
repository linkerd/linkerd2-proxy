# Configuration env vars and their defaults:
HIDE="${HIDE-1}"
export ITERATIONS="${ITERATIONS-1}"
export DURATION="${DURATION-10s}"
export CONNECTIONS="${CONNECTIONS-4}"
export GRPC_STREAMS="${GRPC_STREAMS-4}"
export HTTP_RPS="${HTTP_RPS-4000 7000}"
export GRPC_RPS="${GRPC_RPS-4000 6000}"
export REQ_BODY_LEN="${REQ_BODY_LEN-10 200}"
export TCP="${TCP-1}"
export HTTP="${HTTP-1}"
export GRPC="${GRPC-1}"

export PROXY_PORT_OUTBOUND=4140
export PROXY_PORT_INBOUND=4143

export ID=$(date +"%Y%h%d_%Hh%Mm%Ss")

if [ "$HIDE" -eq "1" ]; then
  export LOG=/dev/null
else
  export LOG=/dev/stdout
fi

BRANCH_NAME=$(git symbolic-ref -q HEAD)
BRANCH_NAME=${BRANCH_NAME##refs/heads/}
BRANCH_NAME=${BRANCH_NAME:-HEAD}
BRANCH_NAME=$(echo $BRANCH_NAME | sed -e 's/\//-/g')
export RUN_NAME="$BRANCH_NAME $ID Iter: $ITERATIONS Dur: $DURATION Conns: $CONNECTIONS Streams: $GRPC_STREAMS"

single_benchmark_run () {
  # run benchmark utilities in background, only proxy runs in foreground
  # run client
  if [ "$MODE" = "TCP" ]; then
    export SERVER="iperf:$SERVER_PORT" && export PRECMD=$* && docker-compose up -d
    echo "TCP $DIRECTION"
    (docker-compose exec iperf \
      linkerd-await \
      --uri="http://proxy:4191/ready" \
      -- \
      iperf -t 6 -p "$PROXY_PORT" -c proxy) | tee "$NAME.$ID.txt" &> "$LOG"
    T=$(grep "/sec" "$NAME.$ID.txt" | cut -d' ' -f12)
    if [ -z "$T" ]; then
      T="0"
    fi
    echo "TCP $DIRECTION, 0, 0, $RUN_NAME, 0, $T" >> "summary.$RUN_NAME.txt"
  else
    export SERVER="iperf:$SERVER_PORT" && export PRECMD=$* && docker-compose up -d
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

          (docker-compose exec fortio \
            linkerd-await \
            --uri="http://proxy:4191/ready" \
            -- \
            fortio load $XARG \
            -resolve proxy \
            -c="$CONNECTIONS" \
            -t="$DURATION" \
            -keepalive=false \
            -payload-size="$l" \
            -qps="$r" \
            -labels="$RUN_NAME" \
            -json="out/$NAME-$r-rps.$ID.json" \
            -H "Host: transparency.test.svc.cluster.local" \
            "http://proxy:${PROXY_PORT}") &> "$LOG"

          T=$(grep Value "$NAME-$r-rps.$ID.json" | tail -1 | cut  -d':' -f2)
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
  $@
}
