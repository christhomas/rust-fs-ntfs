#!/usr/bin/env bash

set -euo pipefail

image=$1
size_mib=$2

mkdir -p "$(dirname "$image")"
dd if=/dev/zero of="$image" bs=1048576 count="$size_mib" status=none
