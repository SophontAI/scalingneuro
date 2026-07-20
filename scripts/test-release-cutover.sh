#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
WORKFLOW=$ROOT_DIR/.github/workflows/release-client.yml
SITE_BRIDGE=$ROOT_DIR/scripts/production-site.sh
RETRY_HELPER=$ROOT_DIR/scripts/retry-release-candidate.sh

[[ -x "$SITE_BRIDGE" ]]
[[ -x "$RETRY_HELPER" ]]
bash -n "$SITE_BRIDGE"
bash -n "$RETRY_HELPER"
grep -qF 'deployment_phase: "backend-validation"' "$SITE_BRIDGE"
grep -qF 'static_commit: $static_commit' "$SITE_BRIDGE"

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
preserve=$(line_for "name: Preserve the exact currently published site and release")
phase_one=$(line_for "name: Deploy backend while retaining current site and downloads")
phase_one_verify=$(line_for "name: Verify phase one and unchanged public site and release bytes")
processor_readiness=$(line_for "name: Require exact Sophont processor readiness")
legacy_registration=$(line_for "name: Prove the preserved public client can register through phase one")
candidate_smoke=$(line_for "name: Prove candidate terminal client through production and Sophont")
phase_two=$(line_for "name: Cut over verified collaborator downloads")
phase_two_verify=$(line_for "name: Verify production API and released downloads")
publish=$(line_for "name: Publish only the production-verified draft")
publish_verify=$(line_for "name: Verify public release source and state")
rollback=$(line_for "name: Restore the forward-compatible bridge after any cutover failure")

assert_before "$base_upload" "$candidate_overlay"
assert_before "$preserve" "$phase_one"
assert_before "$phase_one" "$phase_one_verify"
assert_before "$phase_one_verify" "$processor_readiness"
assert_before "$processor_readiness" "$legacy_registration"
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
grep -qF 'path: candidate-phase-one' "$WORKFLOW"
grep -qF './scripts/production-site.sh capture \' "$WORKFLOW"
grep -qF 'candidate-phase-one dist-phase-one "$production_commit"' "$WORKFLOW"
grep -qF '(.static_commit // .commit) |' "$WORKFLOW"
grep -qF 'git merge-base --is-ancestor "$production_commit" "$GITHUB_SHA"' "$WORKFLOW"
if [[ $(grep -cF '.deployment_phase == "backend-validation"' "$WORKFLOW") -ne 2 ]]; then
  echo "Phase one and recovery must identify the backend-only bridge explicitly" >&2
  exit 1
fi
if [[ $(grep -cF '.static_commit == $static' "$WORKFLOW") -ne 2 ]]; then
  echo "Phase one and recovery must retain the original static-site commit" >&2
  exit 1
fi
if [[ $(grep -cF './scripts/production-site.sh verify dist-phase-one' "$WORKFLOW") -ne 2 ]]; then
  echo "Phase one and recovery must both prove the old static site stayed byte-identical" >&2
  exit 1
fi
grep -qF './scripts/verify-processor-readiness.sh https://scalingneuro.com 60 10' "$WORKFLOW"
grep -qF 'public_client --version' "$WORKFLOW"
grep -qF 'expected_public_policy=open-epi-1.0.0' "$WORKFLOW"
grep -qF 'expected_public_policy=open-mri-1.0.0' "$WORKFLOW"
grep -qF 'User-Agent: neuro-sync/${public_version}' "$WORKFLOW"
grep -qF '[[ "$advertised_public_policy" == "$expected_public_policy" ]]' "$WORKFLOW"
grep -qF 'public_registration_output=$("$public_client" register \' "$WORKFLOW"
grep -qF 'public_policy_acceptance=(--accept-policy)' "$WORKFLOW"
grep -qF -- '--accept-policy-version "$expected_public_policy"' "$WORKFLOW"
grep -qF '"${public_policy_acceptance[@]}")' "$WORKFLOW"
grep -qF 'contribution policy: ${expected_public_policy}' "$WORKFLOW"
grep -qF 'transition_state="$RUNNER_TEMP/neuro-sync-transition-state"' "$WORKFLOW"
grep -qF '> "$RUNNER_TEMP/neuro-sync-transition-policy"' "$WORKFLOW"
# shellcheck disable=SC2016
grep -qF 'release-transition+${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}@scalingneuro.com' "$WORKFLOW"
grep -qF 'command: pages deploy dist-phase-one --project-name=scalingneuro' "$WORKFLOW"
grep -qF 'command: pages deploy dist --project-name=scalingneuro' "$WORKFLOW"
grep -qF '.status == "ok" and .version == $version' "$WORKFLOW"
grep -qF 'consecutive_candidate_observations=0' "$WORKFLOW"
grep -qF 'consecutive_candidate_observations=$((consecutive_candidate_observations + 1))' "$WORKFLOW"
grep -qF 'if (( consecutive_candidate_observations >= 3 )); then' "$WORKFLOW"
if [[ $(grep -cF 'consecutive_candidate_observations=0' "$WORKFLOW") -ne 2 ]]; then
  echo "Phase-one convergence must initialize and reset its consecutive-success counter" >&2
  exit 1
fi
grep -qF 'candidate_contribution=$(<"$contribution_file")' "$WORKFLOW"
grep -qF '.consent_policy_version == "open-mri-1.0.0" and' "$WORKFLOW"
grep -qF '.project_name == "Scaling Neuro public MRI contribution"' "$WORKFLOW"
if [[ $(grep -cF -- "--header 'cache-control: no-cache'" "$WORKFLOW") -lt 3 ]]; then
  echo "Phase-one convergence probes must bypass intermediary caches" >&2
  exit 1
fi
grep -qF 'timeout-minutes: 15' "$WORKFLOW"
grep -qF 'for attempt in $(seq 1 30); do' "$WORKFLOW"
grep -qF ': > "$health_file"' "$WORKFLOW"
grep -qF ': > "$version_file"' "$WORKFLOW"
grep -qF ': > "$contribution_file"' "$WORKFLOW"
grep -qF 'wait "$health_pid" || probe_status=1' "$WORKFLOW"
grep -qF 'wait "$version_pid" || probe_status=1' "$WORKFLOW"
grep -qF 'wait "$contribution_pid" || probe_status=1' "$WORKFLOW"
# These are intentionally literal workflow expressions.
# shellcheck disable=SC2016
grep -qF '[[ "$($client --version)" == "neuro-sync ${EXPECTED_VERSION}" ]]' "$WORKFLOW"
grep -qF 'User-Agent: neuro-sync/${EXPECTED_VERSION}' "$WORKFLOW"
if grep -qF 'candidate_policy=$(curl ' "$WORKFLOW"; then
  echo "Candidate smoke must not retain a one-shot policy probe after convergence" >&2
  exit 1
fi
grep -qF 'candidate_registration_output=$(./scripts/retry-release-candidate.sh \' "$WORKFLOW"
grep -qF 'registration "$client" register \' "$WORKFLOW"
grep -qF "contribution policy: open-mri-1.0.0" "$WORKFLOW"
grep -qF 'transition_policy=$(<"$RUNNER_TEMP/neuro-sync-transition-policy")' "$WORKFLOW"
grep -qF 'if [[ "$transition_policy" == "open-epi-1.0.0" ]]; then' "$WORKFLOW"
grep -qF 'transition_upgrade_output=$(./scripts/retry-release-candidate.sh \' "$WORKFLOW"
grep -qF 'first_upload_output=$(./scripts/retry-release-candidate.sh \' "$WORKFLOW"
grep -qF 'replay_upload_output=$(./scripts/retry-release-candidate.sh \' "$WORKFLOW"
grep -qF 'first_status_converged=false' "$WORKFLOW"
grep -qF 'first_status_converged=true' "$WORKFLOW"
grep -qF 'replay_status_converged=false' "$WORKFLOW"
grep -qF 'replay_status_converged=true' "$WORKFLOW"
if [[ $(grep -cF 'for _ in $(seq 1 24); do' "$WORKFLOW") -ne 2 ]]; then
  echo "Initial and replay status checks must both tolerate stale API generations" >&2
  exit 1
fi
if [[ $(grep -cF -- '--accept-policy-version open-mri-1.0.0' "$WORKFLOW") -ne 4 ]]; then
  echo "Candidate registration, policy transition, initial sync, and replay must name the exact all-MR policy" >&2
  exit 1
fi
if [[ $(grep -cF 'export NEURO_SYNC_STATE_DIR="$smoke_root/state"' "$WORKFLOW") -ne 1 ]]; then
  echo "Candidate retries must retain one state directory across registration and sync" >&2
  exit 1
fi
grep -qF 'Contribution policy updated to open-mri-1.0.0.' "$WORKFLOW"
grep -qF '.project_name == "Scaling Neuro public MRI contribution"' "$WORKFLOW"
# shellcheck disable=SC2016
grep -qF 'functional_fixture=$(find release-smoke -type d \' "$WORKFLOW"
grep -qF -- "-path '*/derived-fixtures/siemens' -print -quit)" "$WORKFLOW"
# shellcheck disable=SC2016
grep -qF 'structural_fixture=$(find release-smoke -type d \' "$WORKFLOW"
grep -qF -- "-path '*/derived-fixtures/siemens-structural-t1like' -print -quit)" "$WORKFLOW"
grep -qF 'mixed_fixture="$smoke_root/mixed-mr-fixture"' "$WORKFLOW"
grep -qF 'mkdir -p "$mixed_fixture/functional" "$mixed_fixture/structural"' "$WORKFLOW"
grep -qF "== 11 ]]" "$WORKFLOW"
# Recovery is forward-only after D1 can contain v2/all-MR state: it keeps the
# candidate backend and reuses the bundle containing the old public site and
# downloads.
# A capture failure happens before phase one and must never deploy the partially
# populated candidate bundle as a recovery action.
# shellcheck disable=SC2016
grep -qF 'id: preserve_current_release' "$WORKFLOW"
grep -qF 'echo "captured=true" >> "$GITHUB_OUTPUT"' "$WORKFLOW"
# shellcheck disable=SC2016
grep -qF "if: \${{ failure() && steps.preserve_current_release.outputs.captured == 'true' }}" "$WORKFLOW"
if grep -qF 'if: ${{ failure() }}' "$WORKFLOW"; then
  echo "Recovery must remain disabled when the public-download capture fails" >&2
  exit 1
fi
grep -qF 'wrangler pages deploy dist-phase-one ' "$WORKFLOW"
grep -qF -- '--commit-hash="$GITHUB_SHA"' "$WORKFLOW"
grep -qF 'production-site.sh verify dist-phase-one' "$WORKFLOW"
grep -qF 'production-downloads.sh verify dist-phase-one' "$WORKFLOW"
grep -qF 'Forward-compatible bridge recovery did not converge' "$WORKFLOW"
if grep -qF './scripts/rollback-pages-production.sh' "$WORKFLOW"; then
  echo "Release recovery must never restore a pre-v2 backend after phase one" >&2
  exit 1
fi
# The creation action exposes the only safe identity for a private draft. GitHub's
# public get-by-tag endpoint cannot resolve that draft until after publication.
# These are intentionally literal workflow expressions and shell variables.
# shellcheck disable=SC2016
grep -qF 'draft_release_id: ${{ steps.create_draft_release.outputs.id }}' "$WORKFLOW"
# shellcheck disable=SC2016
grep -qF 'RELEASE_ID: ${{ needs.assemble.outputs.draft_release_id }}' "$WORKFLOW"
# shellcheck disable=SC2016
if [[ $(grep -cF 'releases/tags/${tag}' "$WORKFLOW") -ne 1 ]]; then
  echo "Get-by-tag must be used only after the draft has been published" >&2
  exit 1
fi
# shellcheck disable=SC2016
if [[ $(grep -cF 'releases/${RELEASE_ID}' "$WORKFLOW") -ne 4 ]]; then
  echo "Draft publication and rollback must remain bound to the creation action release ID" >&2
  exit 1
fi
grep -qF '(.draft | type) == "boolean"' "$WORKFLOW"
# shellcheck disable=SC2016
grep -qF 'recovered_release=$(gh api --method PATCH' "$WORKFLOW"
grep -qF 'GitHub did not confirm the candidate release returned to draft state' "$WORKFLOW"

if grep -q '^  publish-release:' "$WORKFLOW"; then
  echo "GitHub publication must remain inside the rollback-protected cutover job" >&2
  exit 1
fi

if [[ $(grep -c 'command: pages deploy ' "$WORKFLOW") -ne 2 ]]; then
  echo "The release must have exactly two production Pages deployments" >&2
  exit 1
fi

retry_test_root=$(mktemp -d)
trap 'rm -rf "$retry_test_root"' EXIT
retry_fixture='set -euo pipefail
mkdir -p "$NEURO_SYNC_STATE_DIR"
counter_file="$NEURO_SYNC_STATE_DIR/$SCENARIO.count"
attempt=0
if [[ -f "$counter_file" ]]; then
  attempt=$(<"$counter_file")
fi
attempt=$((attempt + 1))
printf "%s\n" "$attempt" > "$counter_file"
case "$SCENARIO:$attempt" in
  stale-get:1)
    echo "Error: --accept-policy-version names open-mri-1.0.0, but the server requires open-epi-1.0.0; review the current policy and pass that exact version" >&2
    exit 31
    ;;
  stale-post:1)
    echo "Error: ingest API request failed (CONSENT_POLICY_UPDATE_REQUIRED): Review and accept the current public contribution policy" >&2
    exit 32
    ;;
  stale-route:1|route-wrong-operation:1)
    echo "Error: ingest API request failed (NOT_FOUND): Route was not found" >&2
    exit 33
    ;;
  stale-upload-contract:1)
    echo "Error: ingest API request failed (INVALID_REQUEST): series[0] contains unknown field: series_kind" >&2
    exit 34
    ;;
  stale-upload-route:1)
    echo "Error: ingest API request failed (NOT_FOUND): Route was not found" >&2
    exit 35
    ;;
  unrelated:*)
    echo "Error: unrelated release failure" >&2
    exit 23
    ;;
  exhausted:*)
    echo "Error: --accept-policy-version names open-mri-1.0.0, but the server requires open-epi-1.0.0; review the current policy and pass that exact version" >&2
    exit 17
    ;;
esac
printf "success:%s:%s\n" "$SCENARIO" "$attempt"'

run_retry_success() {
  local operation=$1
  local scenario=$2
  local state_dir="$retry_test_root/$scenario"
  local output
  output=$(NEURO_SYNC_STATE_DIR="$state_dir" \
    SCENARIO="$scenario" \
    SCALING_NEURO_RELEASE_RETRY_ATTEMPTS=3 \
    SCALING_NEURO_RELEASE_RETRY_DELAY_SECONDS=0 \
    "$RETRY_HELPER" "$operation" bash -c "$retry_fixture")
  [[ "$output" == "success:$scenario:2" ]]
  [[ "$(<"$state_dir/$scenario.count")" == 2 ]]
}

run_retry_success registration stale-get
run_retry_success registration stale-post
run_retry_success policy-upgrade stale-route
run_retry_success upload stale-upload-contract
run_retry_success upload stale-upload-route

unrelated_state="$retry_test_root/unrelated"
set +e
unrelated_output=$(NEURO_SYNC_STATE_DIR="$unrelated_state" \
  SCENARIO=unrelated \
  SCALING_NEURO_RELEASE_RETRY_ATTEMPTS=3 \
  SCALING_NEURO_RELEASE_RETRY_DELAY_SECONDS=0 \
  "$RETRY_HELPER" upload bash -c "$retry_fixture" 2>&1)
unrelated_status=$?
set -e
[[ "$unrelated_status" == 23 ]]
[[ "$unrelated_output" == *"Error: unrelated release failure"* ]]
[[ "$(<"$unrelated_state/unrelated.count")" == 1 ]]

wrong_operation_state="$retry_test_root/route-wrong-operation"
set +e
NEURO_SYNC_STATE_DIR="$wrong_operation_state" \
  SCENARIO=route-wrong-operation \
  SCALING_NEURO_RELEASE_RETRY_ATTEMPTS=3 \
  SCALING_NEURO_RELEASE_RETRY_DELAY_SECONDS=0 \
  "$RETRY_HELPER" registration bash -c "$retry_fixture" >/dev/null 2>&1
wrong_operation_status=$?
set -e
[[ "$wrong_operation_status" == 33 ]]
[[ "$(<"$wrong_operation_state/route-wrong-operation.count")" == 1 ]]

exhausted_state="$retry_test_root/exhausted"
set +e
exhausted_output=$(NEURO_SYNC_STATE_DIR="$exhausted_state" \
  SCENARIO=exhausted \
  SCALING_NEURO_RELEASE_RETRY_ATTEMPTS=3 \
  SCALING_NEURO_RELEASE_RETRY_DELAY_SECONDS=0 \
  "$RETRY_HELPER" upload bash -c "$retry_fixture" 2>&1)
exhausted_status=$?
set -e
[[ "$exhausted_status" == 17 ]]
[[ "$exhausted_output" == *"Error: --accept-policy-version names open-mri-1.0.0"* ]]
[[ "$(<"$exhausted_state/exhausted.count")" == 3 ]]

echo "release cutover ordering tests passed"
