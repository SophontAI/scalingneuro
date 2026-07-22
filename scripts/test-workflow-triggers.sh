#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
workflows="$root/.github/workflows"

if grep --recursive --line-number --extended-regexp \
  '^[[:space:]]*(schedule:|-[[:space:]]*cron:)' "$workflows"; then
  echo "Recurring GitHub Actions are prohibited for this repository." >&2
  exit 1
fi

echo "GitHub workflow trigger policy passed: no recurring schedules."
