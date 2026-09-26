#!/usr/bin/env bash
# Decide CI job results from Actions' `needs` object. Missing, cancelled,
# failed and unexpected skipped jobs are all red. ASan is advisory and is
# deliberately not among the aggregate's dependencies.
set -euo pipefail

case "${1:-}" in
    windows)
        filter='(.changes.result == "success") and
            ((.changes.outputs.mkfs == "true" and
              .["validate-mkfs-windows-run"].result == "success") or
             (.changes.outputs.mkfs == "false" and
              .["validate-mkfs-windows-run"].result == "skipped"))'
        ;;
    aggregate)
        filter='[.test.result, .integration.result, .changes.result,
                 .["validate-mkfs-windows"].result] | all(. == "success")'
        ;;
    *)
        echo 'usage: ci-verdict.sh windows|aggregate' >&2
        exit 2
        ;;
esac

if ! printf '%s' "${GATE_NEEDS_JSON:-}" | jq -e "$filter" >/dev/null; then
    echo "CI $1 verdict failed: a required job failed, was cancelled, or was unexpectedly skipped" >&2
    exit 1
fi
