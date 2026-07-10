#!/usr/bin/env bash

# Scaling Neuro concept preview
#
# This is intentionally a non-functional safety preview. It does not inspect,
# modify, copy, de-identify, or upload DICOM files. Its only purpose is to make
# the proposed one-folder production workflow concrete for pilot discussions.

set -eu

deface_mode=auto
source_dir=

usage() {
  echo "usage: bash neuro-sync-preview.sh [--deface auto|always|off] /path/to/dicom-folder"
  echo ""
  echo "  auto    process structural scans only when facial anatomy may be present"
  echo "  always  process every structural scan"
  echo "  off     keep every structural scan local"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --deface)
      [ "$#" -ge 2 ] || { echo "error: --deface needs auto, always, or off"; exit 1; }
      deface_mode=$2
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      echo "error: unknown option: $1"
      usage
      exit 1
      ;;
    *)
      [ -z "$source_dir" ] || { echo "error: provide exactly one DICOM folder"; exit 1; }
      source_dir=$1
      shift
      ;;
  esac
done

case "$deface_mode" in
  auto|always|off) ;;
  *) echo "error: --deface must be auto, always, or off"; exit 1 ;;
esac

if [ -z "$source_dir" ]; then
  usage
  exit 1
fi

if [ ! -d "$source_dir" ]; then
  echo "error: folder does not exist: $source_dir"
  exit 1
fi

echo "Scaling Neuro — safe concept preview"
echo ""
echo "[ok] source folder accepted: $source_dir"
echo "[ok] structural privacy mode: $deface_mode"
echo "[plan] run CPU-only, one series at a time, with resumable checkpoints"
echo "[plan] watch for completed DICOM series"
echo "[plan] classify functional, diffusion, and structural series"
echo "[plan] remove identifying metadata locally"
case "$deface_mode" in
  auto)
    echo "[plan] pass no-face structural FOVs; process faces; hold uncertainty locally"
    ;;
  always)
    echo "[plan] privacy-process every structural scan, then verify the output"
    ;;
  off)
    echo "[plan] keep every structural scan local"
    ;;
esac
echo "[plan] upload only consented scans that pass the privacy gate"
echo ""
echo "Preview complete. No files were opened, modified, or transmitted."
