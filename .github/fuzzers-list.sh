#!/usr/bin/env bash

## Lists the fuzzers that should be run given a set of changed files.

# Find the nearest fuzzer crate, or nothing.
find_fuzz_dir() {
    d=${1%/*}
    if [ "$d" = . ]; then
        return
    elif [ -d "$d" ] && [[ "$d" = */fuzz ]]; then
        echo "$d"
    elif [ -f "$d/fuzz/Cargo.toml" ]; then
        echo "$d/fuzz"
    else
        find_fuzz_dir "$d"
    fi
}

for file in "$@" ; do
    if [ "$file" = .github/workflows/fuzzers.yml ] || [ "$file" = .github/fuzzers-list.sh ]; then
        find linkerd -type d -name fuzz
        break
    fi
    find_fuzz_dir "$file"
done | sort -u
