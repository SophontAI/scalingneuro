#!/usr/bin/env bash
set -euo pipefail

if (( $# < 1 || $# > 2 )); then
  echo "usage: $0 BINARY [EXPECTED_VERSION]" >&2
  exit 2
fi

binary=$1
expected_version=${2:-}

[[ -x "$binary" ]] || { echo "static Linux client is missing or not executable: $binary" >&2; exit 1; }
command -v file >/dev/null 2>&1 || { echo "file is required" >&2; exit 1; }
command -v readelf >/dev/null 2>&1 || { echo "readelf is required" >&2; exit 1; }

export LC_ALL=C
header=$(readelf --file-header "$binary")
program_headers=$(readelf --program-headers "$binary")
dynamic=$(readelf --dynamic "$binary" 2>&1 || true)
description=$(file "$binary")

grep --quiet 'Class:.*ELF64' <<< "$header" || {
  echo "Linux client is not a 64-bit ELF binary" >&2
  exit 1
}
grep --quiet 'Machine:.*Advanced Micro Devices X86-64' <<< "$header" || {
  echo "Linux client is not x86-64" >&2
  exit 1
}
grep --ignore-case --quiet 'static' <<< "$description" || {
  echo "file does not identify the Linux client as statically linked: $description" >&2
  exit 1
}
if grep --quiet 'INTERP' <<< "$program_headers"; then
  echo "static Linux client unexpectedly contains a runtime interpreter" >&2
  exit 1
fi
if grep --quiet '(NEEDED)' <<< "$dynamic"; then
  echo "static Linux client unexpectedly contains a dynamic dependency" >&2
  echo "$dynamic" >&2
  exit 1
fi

version_output=$("$binary" --version)
if [[ -n "$expected_version" && "$version_output" != "neuro-sync $expected_version" ]]; then
  echo "unexpected client version: $version_output" >&2
  exit 1
fi

printf 'Verified fully static Linux x86_64 client: %s\n' "$version_output"
