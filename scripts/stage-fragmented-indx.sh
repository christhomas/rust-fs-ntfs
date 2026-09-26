#!/usr/bin/env bash
# Stage the small, committed foreign NTFS fixture for a matrix scenario.
set -euo pipefail

destination=$1
source_image="$(dirname "$0")/../test-disks/ntfs-fragmented-indx.img.gz"
mkdir -p "$(dirname "$destination")"
gzip -dc "$source_image" > "$destination"
[ "$(wc -c < "$destination")" -eq 33554432 ]
