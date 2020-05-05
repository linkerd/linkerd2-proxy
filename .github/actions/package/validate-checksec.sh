#!/bin/sh

set -eu

if [ $# -ne 2 ]; then
    (
        echo "usage: $0 EXPECTED RECEIVED"
        echo
        echo "Found $# args"
    ) >&2
    exit 64
fi

expected_file="$1"
received_file="$2"
if [ ! -f "$expected_file" ]; then
    echo "JSON output not found: $expected_file" >&2
    exit 1
fi
if [ ! -f "$received_file" ]; then
    echo "JSON output not found: $received_file" >&2
    exit 1
fi

jq -S '.' "$received_file" | diff -u "$expected_file" - >&2
