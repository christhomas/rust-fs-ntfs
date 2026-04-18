#!/bin/bash
# Build the fs_ntfs Rust crate as a universal static library for macOS.
#
# This script is called by Xcode's build phase before compiling Swift.
# It produces: target/universal/libfs_ntfs.a

set -euo pipefail

# Xcode runs scripts in a restricted PATH — discover cargo from login shell
CARGO_BIN=$(bash -l -c "which cargo" 2>/dev/null || true)
if [ -n "$CARGO_BIN" ]; then
    export PATH="$(dirname "$CARGO_BIN"):$PATH"
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# Determine build profile from Xcode configuration
if [ "${CONFIGURATION:-Debug}" = "Release" ]; then
    CARGO_PROFILE="release"
    CARGO_FLAGS="--release"
    PROFILE_DIR="release"
else
    CARGO_PROFILE="dev"
    CARGO_FLAGS=""
    PROFILE_DIR="debug"
fi

echo "Building fs_ntfs (${CARGO_PROFILE})..."

# Build for both architectures
cargo build ${CARGO_FLAGS} --target aarch64-apple-darwin
cargo build ${CARGO_FLAGS} --target x86_64-apple-darwin

# Create universal binary
mkdir -p target/universal
lipo -create \
    "target/aarch64-apple-darwin/${PROFILE_DIR}/libfs_ntfs.a" \
    "target/x86_64-apple-darwin/${PROFILE_DIR}/libfs_ntfs.a" \
    -output "target/universal/libfs_ntfs.a"

echo "Built: target/universal/libfs_ntfs.a"
