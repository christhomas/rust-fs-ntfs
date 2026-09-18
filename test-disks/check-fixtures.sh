#!/usr/bin/env bash
# check-fixtures.sh -- fail, naming what is missing, unless every fixture
# build-ntfs-feature-images.sh builds is in test-disks/.
#
# Without them the fixture-driven test files panic one test at a time
# (tests/common/mod.rs, "did you run test-disks/build-ntfs-feature-images.sh?")
# -- measured on a Mac on 2026-09-18: 414 FAILED lines and 4,755 lines of
# output, every one of them the same missing file. `chore test:suite` runs
# this first so the answer is two lines. It is a failure, not a skip.
#
# The list is build-ntfs-feature-images.sh's build_* functions. The two
# Windows-authored fixtures (ntfs-attrlist, ntfs-compressed) are not on it:
# nothing builds them, and tests/native_read_fixtures.rs skips without them.
set -euo pipefail

dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
missing=""
for img in basic manyfiles large-file sparse ads unicode deep; do
    [ -f "$dir/ntfs-$img.img" ] || missing="$missing ntfs-$img.img"
done
if [ -n "$missing" ]; then
    echo "missing fixtures in test-disks/:$missing" >&2
    echo "build them with test-disks/build-ntfs-feature-images.sh (Linux: needs mkntfs and a mount)" >&2
    exit 1
fi
