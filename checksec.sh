#!/bin/bash

set -eu

require_from() {
    if ! command -v "$1" >/dev/null 2>/dev/null ; then
        echo "Please acquire $1 from $2" >&2
        exit 1
    fi
}

require_from readelf     binutils
require_from checksec    https://github.com/slimm609/checksec.sh
require_from jq          https://stedolan.github.io/jq/

if [ $# -ne 1 ]; then
    echo "usage: $0 EXECUTABLE" >&2
    exit 64
fi

path=$1
if [ ! -x "$path" ]; then
    echo "Executable not found: $path" >&2
    exit 1
fi

out=$(checksec --output=json --file="$path")
echo "$out" | jq ".[\"$path\"] | del(.\"fortify-able\") | del(.fortified)" 
