#!/usr/bin/env bash
set +x
CARGO=$(which cargo)
timeout 10m $CARGO fuzz build