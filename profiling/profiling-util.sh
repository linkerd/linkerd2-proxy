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

RED=$(tput setaf 1)
GREEN=$(tput setaf 2)
BLUE=$(tput setaf 4)
BOLD=$(tput bold)
SGR0=$(tput sgr0)
WIDTH=12

err() {
    printf "${RED}${BOLD}error${SGR0}${BOLD}: %s${SGR0}\n" "$1" 1>&2
    if [ "$#" -gt 1 ]; then
      echo "$2" | sed "s/^/  ${BLUE}|${SGR0} /" 1>&2
      printf "\n" 1>&2
    fi
}

status() {
    printf "${GREEN}${BOLD}%${WIDTH}s${SGR0} %s\n" "$1" "$2"
}

BRANCH_NAME=$(git symbolic-ref -q HEAD)
BRANCH_NAME=${BRANCH_NAME##refs/heads/}
BRANCH_NAME=${BRANCH_NAME:-HEAD}
BRANCH_NAME=$(echo $BRANCH_NAME | sed -e 's/\//-/g')
export RUN_NAME="$BRANCH_NAME $ID Iter: $ITERATIONS Dur: $DURATION Conns: $CONNECTIONS Streams: $GRPC_STREAMS"

export OUT_DIR="../target/profile/$ID"
export TEST_KEY_DIR="/tmp/$ID/ssh"
status "Creating" "$OUT_DIR"

mkdir -p "$OUT_DIR"
mkdir -p "$TEST_KEY_DIR"
ssh-keygen -f "$TEST_KEY_DIR/id_ecdsa" -t ecdsa -b 521 -N '' -q
cat "$TEST_KEY_DIR/id_ecdsa.pub" > "$TEST_KEY_DIR/authorized_keys"

single_benchmark_run () {
  # run benchmark utilities in background, only proxy runs in foreground
  # run client
  if [ "$MODE" = "TCP" ]; then
    SERVER="iperf" docker-compose up -d &> "$LOG"
    status "Running" "TCP $DIRECTION"
    IPERF=$( ( (docker-compose exec iperf \
        linkerd-await \
        --uri="http://proxy:4191/ready" \
        -- \
        iperf -t 6 -p "$PROXY_PORT" -c proxy) | tee "$OUT_DIR/$NAME.txt") 2>&1 | tee "$LOG")

    if [[ $? -ne 0 ]]; then
      err "iperf failed:" "$IPERF"
      exit 1
    fi

    T=$(grep "/sec" "$OUT_DIR/$NAME.txt" | cut -d' ' -f12)
    if [ -z "$T" ]; then
      T="0"
    fi
    echo "TCP $DIRECTION, 0, 0, $RUN_NAME, 0, $T" >> "$OUT_DIR/summary.txt"
  else
    SERVER="fortio" docker-compose up -d &> "$LOG"
    docker-compose kill iperf &> "$LOG"
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
          status "Running" "$MODE $DIRECTION Iteration: $i RPS: $r REQ_BODY_LEN: $l"

          FORTIO=$( (docker-compose exec fortio \
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
            -json="out/${NAME}_$r-rps.json" \
            -H "Host: transparency.test.svc.cluster.local" \
            "http://proxy:${PROXY_PORT}") 2>&1 | tee "$LOG")

          if [[ $? -ne 0 ]]; then
              err "fortio failed:" "$FORTIO"
              exit 1
          fi

          T=$(grep Value "$OUT_DIR/${NAME}_$r-rps.json" | tail -1 | cut  -d':' -f2)
          if [ -z "$T" ]; then
            err "No last percentile value found"
            exit 1
          fi
          S=$(python -c "print(max($S, $T*1000.0))")
        done
        echo "$MODE $DIRECTION, $r, $l, $RUN_NAME, $S, 0" >> "$OUT_DIR/summary.txt"
      done
    done
  fi

  docker-compose exec proxy bash -c 'echo F | netcat 127.0.0.1 7777'
  if [ "$(docker wait profiling_proxy_1)" -gt 0 ]; then
    err "proxy failed! log output:" "$(docker logs profiling_proxy_1 2>&1)"
    exit 1
  fi
}

teardown() {
  (docker-compose down -t 5 > "$LOG") || err "teardown failed"
}
