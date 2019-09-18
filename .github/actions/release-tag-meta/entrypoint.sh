#!/bin/sh

set -eu

ref="$1"
tag=$(echo "$ref" | sed s,^refs/tags/,,)
name=$(echo "$tag" | sed s,^release/,,)

echo ::set-output "name=tag::$tag"
echo ::set-output "name=name::$name"
