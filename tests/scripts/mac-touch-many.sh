#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
capture="$scratch/arguments"
export capture

printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\n" "${MSYS_NO_PATHCONV:-}" "$@" > "$capture"' > "$scratch/fake"
chmod +x "$scratch/fake"
bash "$repo/scripts/mac-touch-many.sh" "$scratch/fake" volume.img /routed f_ 1
mapfile -t actual < "$capture"
[[ ${#actual[@]} -eq 5 ]] || { echo "mac-touch-many: wrong argument count" >&2; exit 1; }
[[ ${actual[0]} == 1 ]] || { echo "mac-touch-many: MSYS path conversion remains enabled" >&2; exit 1; }
[[ ${actual[1]} == touch && ${actual[2]} == volume.img && ${actual[3]} == /routed && ${actual[4]} == f_0000.txt ]] || {
    echo "mac-touch-many: wrong touch arguments" >&2
    exit 1
}
echo 'mac-touch-many: ok (native path conversion disabled, arguments preserved)'
