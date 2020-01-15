#!/bin/bash
PLOT_IMAGE="linkerd2_profiling_plot"
ARGS=()

usage() {
    cat <<USAGE

    Usage: $(basename $0) [--logy] [-h] <data1> <data2> <graph-name-prefix>

    --logy      use logarithmic scaling for y-axis
    -h          print this help message
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
    -h)
        usage
        exit 0
        ;;
    -*)
        ARGS+=("$1")
        ;;
    *)
        if [[ -z "$DATA1" ]]; then
            DATA1="wd/$1"
        elif [[ -z "$DATA2" ]]; then
            DATA2="wd/$1"
        elif [[ -z "$OUT" ]]; then
            OUT="wd/$1"
        else
            usage
            exit 1
        fi
        ;;
    esac
    shift
done

if [[ -z "$DATA1" || -z "$DATA2" || -z "$OUT" ]]; then
    usage
    exit 1
fi

# Build the plot script docker image. If it hasn't changed, this should build it
# from cache.
(cd plot && docker build -t "$PLOT_IMAGE" .)
# Run the python script in the docker image we just built, mounting the current
# working dir so the script can read the input data and write the plot images.
#
# Since mount type "bind" cannot mount to /, we mount pwd as /wd in the
# container, and modify the args to be paths in wd.
docker run \
    --rm -it \
    --mount src="$(pwd)",target=/wd,type=bind \
    "$PLOT_IMAGE" "${ARGS[@]}" "$DATA1" "$DATA2" "$OUT"
