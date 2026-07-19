#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
WORKFLOW=$ROOT_DIR/.github/workflows/release-client.yml

line_for() {
  local needle=$1
  local matches
  matches=$(grep -nF -- "$needle" "$WORKFLOW" || true)
  if [[ $(wc -l <<<"$matches" | tr -d ' ') -ne 1 || -z "$matches" ]]; then
    echo "Expected exactly one release step containing: $needle" >&2
    echo "$matches" >&2
    exit 1
  fi
  printf '%s\n' "${matches%%:*}"
}

assert_before() {
  local earlier=$1
  local later=$2
  if (( earlier >= later )); then
    echo "Release cutover ordering assertion failed: $earlier must precede $later" >&2
    exit 1
  fi
}

base_upload=$(line_for "name: Upload backend and site bundle without release cutover")
candidate_overlay=$(line_for "name: Add versioned downloads and public index")
preserve=$(line_for "name: Preserve the exact currently published release")
remember=$(line_for "name: Remember the current production deployment")
phase_one=$(line_for "name: Deploy backend and site while retaining current downloads")
phase_one_verify=$(line_for "name: Verify phase one and unchanged public release bytes")
legacy_registration=$(line_for "name: Prove the preserved public client can register through phase one")
candidate_smoke=$(line_for "name: Prove candidate terminal client through production and Sophont")
phase_two=$(line_for "name: Cut over verified collaborator downloads")
phase_two_verify=$(line_for "name: Verify production API and released downloads")
publish=$(line_for "name: Publish only the production-verified draft")
publish_verify=$(line_for "name: Verify public release source and state")
rollback=$(line_for "name: Roll back the production deployment after any cutover failure")

assert_before "$base_upload" "$candidate_overlay"
assert_before "$preserve" "$remember"
assert_before "$remember" "$phase_one"
assert_before "$phase_one" "$phase_one_verify"
assert_before "$phase_one_verify" "$legacy_registration"
assert_before "$legacy_registration" "$candidate_smoke"
assert_before "$candidate_smoke" "$phase_two"

if [[ $(grep -cF '.status == "complete"' "$WORKFLOW") -ne 2 ]]; then
  echo "release workflow must validate both candidate run snapshots as complete" >&2
  exit 1
fi

if grep -qF '.status == "committed"' "$WORKFLOW"; then
  echo "release workflow must not confuse local run completion with remote chunk commit state" >&2
  exit 1
fi
assert_before "$phase_two" "$phase_two_verify"
assert_before "$phase_two_verify" "$publish"
assert_before "$publish" "$publish_verify"
assert_before "$publish_verify" "$rollback"

grep -qF './scripts/production-downloads.sh capture dist-phase-one' "$WORKFLOW"
grep -qF './scripts/production-downloads.sh verify dist-phase-one' "$WORKFLOW"
grep -qF 'public_client --version' "$WORKFLOW"
# shellcheck disable=SC2016
grep -qF 'release-transition+${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}@scalingneuro.com' "$WORKFLOW"
grep -qF 'command: pages deploy dist-phase-one --project-name=scalingneuro' "$WORKFLOW"
grep -qF 'command: pages deploy dist --project-name=scalingneuro' "$WORKFLOW"
# These are intentionally literal workflow expressions.
# shellcheck disable=SC2016
grep -qF '[[ "$($client --version)" == "neuro-sync ${EXPECTED_VERSION}" ]]' "$WORKFLOW"
# shellcheck disable=SC2016
grep -qF 'fixture=$(find release-smoke -type d -path '\''*/derived-fixtures/siemens'\'' -print -quit)' "$WORKFLOW"
# shellcheck disable=SC2016
grep -qF 'if: ${{ failure() && steps.previous_production.outputs.deployment_id !=' "$WORKFLOW"
grep -qF './scripts/rollback-pages-production.sh' "$WORKFLOW"

if grep -q '^  publish-release:' "$WORKFLOW"; then
  echo "GitHub publication must remain inside the rollback-protected cutover job" >&2
  exit 1
fi

if [[ $(grep -c 'command: pages deploy ' "$WORKFLOW") -ne 2 ]]; then
  echo "The release must have exactly two production Pages deployments" >&2
  exit 1
fi

echo "release cutover ordering tests passed"
