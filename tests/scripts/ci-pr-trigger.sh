#!/usr/bin/env bash
# A PR based on another branch must still start CI (#352).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! awk '
    /^on:$/ { events = 1; next }
    events && /^[^[:space:]#]/ { events = 0 }
    events && /^  push:$/ { section = "push"; push = 1; next }
    events && /^  pull_request:$/ { section = "pr"; pr = 1; next }
    events && /^  [[:alnum:]_]+:/ { section = ""; next }
    section == "push" && /^    branches: \[main\]$/ { main = 1 }
    section == "push" && /^    tags: \[/ && /v\*/ { tags = 1 }
    section == "pr" && /^    branches:/ { filtered = 1 }
    END { exit !(push && pr && main && tags && !filtered) }
' "$ROOT/.github/workflows/ci.yml"; then
    echo "FAIL: CI must run on every PR base and keep main/v* push filters" >&2
    exit 1
fi

echo "CI trigger: every PR base; main/v* pushes"
