#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

for case_dir in tests/replay/*/; do
  case_name="$(basename "$case_dir")"
  [ -x "$case_dir/run.sh" ] || continue
  echo "==> $case_name"
  "$case_dir/run.sh"
done
