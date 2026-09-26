#!/usr/bin/env bash
set -euo pipefail

binary=$1
image=$2
parent=$3
prefix=$4
count=$5

[[ $count =~ ^[0-9]+$ ]] || { echo "count must be a nonnegative integer" >&2; exit 2; }
for ((i = 0; i < count; i++)); do
    printf -v name '%s%04d.txt' "$prefix" "$i"
    "$binary" touch "$image" "$parent" "$name" >/dev/null
done
