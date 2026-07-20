#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
API_URL=${1:-https://scalingneuro.com}
ATTEMPTS=${2:-30}
INTERVAL_SECONDS=${3:-10}

if [[ ! "$API_URL" =~ ^https://[A-Za-z0-9.-]+(:[0-9]+)?$ ]] ||
   [[ ! "$ATTEMPTS" =~ ^[1-9][0-9]*$ ]] ||
   [[ ! "$INTERVAL_SECONDS" =~ ^[1-9][0-9]*$ ]]; then
  echo "usage: verify-processor-readiness.sh [HTTPS_API_ORIGIN [ATTEMPTS [INTERVAL_SECONDS]]]" >&2
  exit 2
fi

processor_version=$(sed -n 's/^__version__ = "\([^"]*\)"/\1/p' \
  "$ROOT_DIR/processor/scaling_neuro_processor/__init__.py")
pipeline_version=$(sed -n 's/^PIPELINE_VERSION = "\([^"]*\)"/\1/p' \
  "$ROOT_DIR/processor/scaling_neuro_processor/__init__.py")
controller_digest=$(python3 \
  "$ROOT_DIR/processor/scripts/controller-source-sha256.py" \
  "$ROOT_DIR/processor/requirements.lock" \
  "$ROOT_DIR/processor/scaling_neuro_processor")

if [[ ! "$processor_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
   [[ ! "$pipeline_version" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$ ]] ||
   [[ ! "$controller_digest" =~ ^[0-9a-f]{64}$ ]]; then
  echo "Local processor release attestation is invalid" >&2
  exit 2
fi

for attempt in $(seq 1 "$ATTEMPTS"); do
  health=$(curl --fail --silent --show-error --max-time 15 \
    "$API_URL/health?processor-readiness=$attempt" || true)
  if jq --exit-status \
      --arg version "$processor_version" \
      --arg pipeline "$pipeline_version" \
      --arg digest "$controller_digest" '
        .status == "ok" and
        .processor.ready == true and
        .processor.required_version == $version and
        .processor.required_pipeline_version == $pipeline and
        .processor.required_controller_source_sha256 == $digest and
        .processor.active_compatible_consumers >= 1 and
        (.processor.active_controller_source_sha256 | index($digest) != null)
      ' <<<"$health" >/dev/null 2>&1; then
    echo "Sophont processor ready: version=$processor_version pipeline=$pipeline_version controller=$controller_digest"
    exit 0
  fi
  if (( attempt < ATTEMPTS )); then
    sleep "$INTERVAL_SECONDS"
  fi
done

echo "No fresh Sophont processor attested the exact release source" >&2
exit 1
