#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

usage() {
  cat >&2 <<'EOF'
Usage: production-site.sh capture <candidate-dist> <bridge-dist> <production-commit> [origin]
       production-site.sh verify  <bridge-dist> [origin]

Capture rebuilds the static site from the commit currently published in
/version.json, proves every rebuilt public file still matches the canonical
origin, and combines those files with only the candidate _worker.js and
version.json. HTML comparison permits only Cloudflare's randomized email-address
obfuscation; all other static assets remain byte-exact. The release workflow
overlays the separately captured public downloads afterward.

Verify proves that every preserved static file is still served with the same
content from the canonical origin after the bridge deployment.
EOF
  exit 2
}

[[ $# -ge 2 && $# -le 5 ]] || usage

MODE=$1
shift

command -v curl >/dev/null
command -v git >/dev/null

WORK_DIR_TO_REMOVE=
trap 'rm -rf "${WORK_DIR_TO_REMOVE:-}"' EXIT

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

absolute_existing_dir() {
  local directory=$1
  [[ -d "$directory" ]] || return 1
  (cd "$directory" && pwd -P)
}

absolute_output_dir() {
  local directory=$1
  local parent
  parent=$(dirname "$directory")
  mkdir -p "$parent"
  parent=$(cd "$parent" && pwd -P)
  printf '%s/%s\n' "$parent" "$(basename "$directory")"
}

safe_relative_path() {
  local path=$1
  [[ "$path" =~ ^[A-Za-z0-9._+/-]+$ ]] &&
    [[ "$path" != /* ]] &&
    [[ "/$path/" != *"/../"* ]] &&
    [[ "/$path/" != *"/./"* ]] &&
    [[ "$path" != *"//"* ]]
}

fetch() {
  local url=$1
  local output=$2
  curl --fail --location --silent --show-error \
    --connect-timeout 15 --max-time 300 \
    --retry 5 --retry-all-errors \
    --header 'cache-control: no-cache' \
    --output "$output" "$url"
}

wait_for_fetches() {
  local status=0 pid
  for pid in "$@"; do
    wait "$pid" || status=1
  done
  return "$status"
}

parallel_fetch_paths() {
  local origin=$1
  local output_root=$2
  local inventory=$3
  local relative
  local -a pids=()
  while IFS= read -r relative; do
    mkdir -p "$output_root/$(dirname "$relative")"
    (fetch "$origin/$relative" "$output_root/$relative") &
    pids+=("$!")
    if (( ${#pids[@]} >= 8 )); then
      wait_for_fetches "${pids[@]}" || return 1
      pids=()
    fi
  done < "$inventory"
  if (( ${#pids[@]} > 0 )); then
    wait_for_fetches "${pids[@]}" || return 1
  fi
}

normalize_for_public_compare() {
  local input=$1
  local output=$2
  local mode=$3
  if [[ "$mode" == exact ]]; then
    install -m 0644 "$input" "$output"
    return
  fi
  command -v perl >/dev/null
  perl -0pe '
    s{<a href="/cdn-cgi/l/email-protection#[^"]*"><span class="__cf_email__" data-cfemail="[^"]*">\[email&#160;protected\]</span></a>}{<a data-scaling-neuro-email="protected"></a>}g;
    s{<a href="mailto:[^"]*">[^<]*</a>}{<a data-scaling-neuro-email="protected"></a>}g;
    s{<script data-cfasync="false" src="/cdn-cgi/scripts/[^"]+/cloudflare-static/email-decode\.min\.js"></script>}{}g;
  ' "$input" > "$output"
  if grep -qE '/cdn-cgi/(l/email-protection|scripts/.*/cloudflare-static/email-decode)' "$output"; then
    echo "Cloudflare email protection used an unrecognized HTML transform" >&2
    exit 1
  fi
}

capture() {
  [[ $# -ge 3 && $# -le 4 ]] || usage
  local candidate_dist bridge_dist production_commit origin
  candidate_dist=$(absolute_existing_dir "$1") || {
    echo "Candidate Pages directory does not exist: $1" >&2
    exit 1
  }
  bridge_dist=$(absolute_output_dir "$2")
  production_commit=$3
  origin=${4:-https://scalingneuro.com}
  origin=${origin%/}

  [[ "$production_commit" =~ ^[0-9a-f]{40}$ ]] || {
    echo "Published production commit is invalid: $production_commit" >&2
    exit 1
  }
  git -C "$ROOT_DIR" cat-file -e "${production_commit}^{commit}" 2>/dev/null || {
    echo "Published production commit is unavailable locally: $production_commit" >&2
    exit 1
  }
  [[ -f "$candidate_dist/_worker.js" && -f "$candidate_dist/version.json" ]] || {
    echo "Candidate Pages bundle is missing _worker.js or version.json" >&2
    exit 1
  }
  command -v jq >/dev/null
  local candidate_commit
  candidate_commit=$(jq -er '.commit | select(type == "string" and test("^[0-9a-f]{40}$"))' \
    "$candidate_dist/version.json")

  command -v npm >/dev/null
  command -v tar >/dev/null

  local work_dir source_dir rebuilt_dist staged_dist manifest_tmp manifest_path
  work_dir=$(mktemp -d "${TMPDIR:-/tmp}/scaling-neuro-production-site.XXXXXX")
  WORK_DIR_TO_REMOVE=$work_dir
  source_dir="$work_dir/source"
  rebuilt_dist="$source_dir/dist"
  staged_dist="$work_dir/bridge"
  manifest_tmp="$work_dir/static-SHA256SUMS"
  manifest_path="${bridge_dist}.static-SHA256SUMS"
  mkdir -p "$source_dir" "$staged_dist"

  git -C "$ROOT_DIR" archive "$production_commit" | tar -x -C "$source_dir"
  npm ci --prefix "$source_dir/worker" --no-audit --no-fund >/dev/null
  GITHUB_SHA="$production_commit" "$source_dir/scripts/build-site.sh"
  [[ -f "$rebuilt_dist/_worker.js" && -f "$rebuilt_dist/version.json" ]] || {
    echo "Published source commit did not build a complete Pages bundle" >&2
    exit 1
  }

  local inventory
  inventory="$work_dir/static-paths"
  local relative local_file fetched_file actual_sha comparison_mode
  while IFS= read -r local_file; do
    relative=${local_file#"$rebuilt_dist/"}
    case "$relative" in
      _worker.js|version.json) continue ;;
    esac
    safe_relative_path "$relative" || {
      echo "Published site build contains an unsafe path: $relative" >&2
      exit 1
    }
    printf '%s\n' "$relative" >> "$inventory"
  done < <(find "$rebuilt_dist" -type f | LC_ALL=C sort)
  [[ -s "$inventory" ]] || {
    echo "Published site build contains no preservable static files" >&2
    exit 1
  }
  parallel_fetch_paths "$origin" "$work_dir/live" "$inventory" || {
    echo "Could not capture every published static file" >&2
    exit 1
  }

  while IFS= read -r relative; do
    local_file="$rebuilt_dist/$relative"
    fetched_file="$work_dir/live/$relative"
    mkdir -p "$(dirname "$staged_dist/$relative")"
    comparison_mode=exact
    [[ "$relative" == *.html ]] && comparison_mode=normalized-html
    mkdir -p "$work_dir/normalized/source/$(dirname "$relative")" \
      "$work_dir/normalized/live/$(dirname "$relative")"
    normalize_for_public_compare \
      "$local_file" "$work_dir/normalized/source/$relative" "$comparison_mode"
    normalize_for_public_compare \
      "$fetched_file" "$work_dir/normalized/live/$relative" "$comparison_mode"
    if ! cmp -s \
      "$work_dir/normalized/source/$relative" \
      "$work_dir/normalized/live/$relative"; then
      echo "Canonical production file does not match commit $production_commit: /$relative" >&2
      exit 1
    fi
    install -m 0644 "$local_file" "$staged_dist/$relative"
    actual_sha=$(hash_file "$work_dir/normalized/source/$relative")
    printf '%s\t%s\t%s\n' "$comparison_mode" "$actual_sha" "$relative" >> "$manifest_tmp"
  done < "$inventory"

  [[ -s "$manifest_tmp" ]] || {
    echo "Published site build contains no preservable static files" >&2
    exit 1
  }
  if [[ -n $(cut -f3 "$manifest_tmp" | LC_ALL=C sort | uniq -d) ]]; then
    echo "Published site build repeats a static path" >&2
    exit 1
  fi

  install -m 0644 "$candidate_dist/_worker.js" "$staged_dist/_worker.js"
  jq --arg static_commit "$production_commit" \
    '. + {
      static_commit: $static_commit,
      deployment_phase: "backend-validation"
    }' "$candidate_dist/version.json" > "$work_dir/bridge-version.json"
  install -m 0644 "$work_dir/bridge-version.json" "$staged_dist/version.json"
  rm -rf "$bridge_dist"
  mv "$staged_dist" "$bridge_dist"
  install -m 0644 "$manifest_tmp" "$manifest_path"
  printf 'Preserved production site commit %s with candidate backend %s\n' \
    "$production_commit" "$candidate_commit"
}

verify() {
  [[ $# -ge 1 && $# -le 2 ]] || usage
  local bridge_dist origin manifest_path
  bridge_dist=$(absolute_existing_dir "$1") || {
    echo "Bridge Pages directory does not exist: $1" >&2
    exit 1
  }
  origin=${2:-https://scalingneuro.com}
  origin=${origin%/}
  manifest_path="${bridge_dist}.static-SHA256SUMS"
  [[ -s "$manifest_path" ]] || {
    echo "Preserved production-site manifest is missing: $manifest_path" >&2
    exit 1
  }

  local work_dir comparison_mode expected_sha relative local_sha remote_file remote_sha inventory
  work_dir=$(mktemp -d "${TMPDIR:-/tmp}/scaling-neuro-production-site-verify.XXXXXX")
  WORK_DIR_TO_REMOVE=$work_dir
  inventory="$work_dir/static-paths"
  while IFS=$'\t' read -r comparison_mode expected_sha relative; do
    if ! { [[ "$comparison_mode" == exact || "$comparison_mode" == normalized-html ]] &&
      [[ "$expected_sha" =~ ^[0-9a-f]{64}$ ]] &&
      safe_relative_path "$relative"; }; then
      echo "Preserved production-site manifest is invalid" >&2
      exit 1
    fi
    [[ -f "$bridge_dist/$relative" && ! -L "$bridge_dist/$relative" ]] || {
      echo "Bridge bundle is missing preserved static file: $relative" >&2
      exit 1
    }
    mkdir -p "$work_dir/normalized/local/$(dirname "$relative")"
    normalize_for_public_compare \
      "$bridge_dist/$relative" \
      "$work_dir/normalized/local/$relative" \
      "$comparison_mode"
    local_sha=$(hash_file "$work_dir/normalized/local/$relative")
    [[ "$local_sha" == "$expected_sha" ]] || {
      echo "Bridge static file changed after capture: $relative" >&2
      exit 1
    }
    printf '%s\n' "$relative" >> "$inventory"
  done < "$manifest_path"
  [[ -s "$inventory" ]] || {
    echo "Preserved production-site manifest is empty" >&2
    exit 1
  }
  parallel_fetch_paths "$origin" "$work_dir/live" "$inventory" || {
    echo "Could not fetch every phase-one static file" >&2
    exit 1
  }

  while IFS=$'\t' read -r comparison_mode expected_sha relative; do
    remote_file="$work_dir/live/$relative"
    mkdir -p "$work_dir/normalized/remote/$(dirname "$relative")"
    normalize_for_public_compare \
      "$remote_file" \
      "$work_dir/normalized/remote/$relative" \
      "$comparison_mode"
    remote_sha=$(hash_file "$work_dir/normalized/remote/$relative")
    [[ "$remote_sha" == "$expected_sha" ]] || {
      echo "Phase-one deployment changed the public static site: /$relative" >&2
      exit 1
    }
  done < "$manifest_path"
}

case "$MODE" in
  capture) capture "$@" ;;
  verify) verify "$@" ;;
  *) usage ;;
esac
