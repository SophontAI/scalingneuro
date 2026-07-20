#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
WORKFLOW=$ROOT_DIR/.github/workflows/release-client.yml
SITE_BRIDGE=$ROOT_DIR/scripts/production-site.sh

[[ -x "$SITE_BRIDGE" ]]
bash -n "$SITE_BRIDGE"
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
# These are intentionally literal workflow expressions.
# shellcheck disable=SC2016
grep -qF '[[ "$($client --version)" == "neuro-sync ${EXPECTED_VERSION}" ]]' "$WORKFLOW"
grep -qF 'User-Agent: neuro-sync/${EXPECTED_VERSION}' "$WORKFLOW"
grep -qF '[[ "$candidate_policy" == "open-mri-1.0.0" ]]' "$WORKFLOW"
grep -qF 'candidate_registration_output=$("$client" register \' "$WORKFLOW"
grep -qF "contribution policy: open-mri-1.0.0" "$WORKFLOW"
grep -qF 'transition_policy=$(<"$RUNNER_TEMP/neuro-sync-transition-policy")' "$WORKFLOW"
grep -qF 'if [[ "$transition_policy" == "open-epi-1.0.0" ]]; then' "$WORKFLOW"
grep -qF 'transition_upgrade_output=$("$client" upload "$transition_empty" \' "$WORKFLOW"
if [[ $(grep -cF -- '--accept-policy-version open-mri-1.0.0)' "$WORKFLOW") -ne 2 ]]; then
  echo "Candidate registration and policy transition must name the exact all-MR policy" >&2
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

echo "release cutover ordering tests passed"
