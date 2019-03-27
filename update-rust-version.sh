#!/bin/bash

VERSION=$1

echo $VERSION > rust-toolchain
sed -i.bak -e "s/RUST_IMAGE=.*/RUST_IMAGE=rust:$VERSION/" Dockerfile
