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

case "$OSTYPE" in
  darwin*) NETSTAT="netstat -ptcp -an ";;
  *) NETSTAT="netstat -tan" ;;
esac

port_open() {
  $NETSTAT | grep "$1.*LISTEN" &> "$LOG"
}

dep_fortio() {
  which fortio &> /dev/null || go get fortio.org/fortio
  which fortio &> /dev/null || ( echo "fortio not found: Add the bin folder of your GOPATH to your PATH" ; exit 1 )
}

# install_iperf() {
#   case "$OSTYPE" of
#     darwin*) brew install iperf ;;

# }

# dep_iperf() {
#   if [which fortio &> /dev/null || go get fortio.org/fortio
# }
