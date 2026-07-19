#!/bin/bash
#SBATCH --job-name=scaling-neuro-processor-native
#SBATCH --account=sophont
#SBATCH --partition=c
#SBATCH --qos=bottom
#SBATCH --requeue
#SBATCH --export=NIL
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=4
#SBATCH --mem=16G
#SBATCH --time=0
#SBATCH --signal=B:USR1@120
#SBATCH --output=/data/paul/scaling-neuro/logs/%x-%j.out
#SBATCH --error=/data/paul/scaling-neuro/logs/%x-%j.err

set -euo pipefail
export PATH="/opt/slurm/bin:/usr/local/bin:/usr/bin:/bin"
export PYTHONNOUSERSITE=1
export PYTHONDONTWRITEBYTECODE=1
export PYTHONSAFEPATH=1
export TZ=UTC

if [[ $# -gt 4 ]]; then
    echo "usage: sbatch run-processor-native.sbatch [RELEASE [API_URL [WORK_ROOT [TOKEN_FILE]]]]" >&2
    exit 2
fi

release=${1:-/data/paul/scaling-neuro/native/releases/0.1.1}
api_url=${2:-https://scalingneuro.com}
work_root=${3:-/data/paul/scaling-neuro/processor}
token_file=${4:-/data/paul/scaling-neuro/secrets/processor-token}
python_bin="$release/venv/bin/python"
native_tools_image="$release/native-tools.sqsh"
release_manifest="$release/RELEASE"
srun_bin=/opt/slurm/bin/srun
env_bin=/usr/bin/env
app_root="$release/app"

if [[ ! -x "$python_bin" || ! -r "$native_tools_image" || ! -r "$release_manifest" || ! -r "$app_root/scaling_neuro_processor/__init__.py" ]]; then
    echo "native processor release is incomplete or unreadable" >&2
    exit 2
fi
if [[ ! -x "$srun_bin" || ! -x "$env_bin" || ! -r "$token_file" ]]; then
    echo "Slurm, env, or the processor token is unavailable" >&2
    exit 2
fi
expected_image_sha256=$(sed -n 's/^native_tools_squashfs_sha256=//p' "$release_manifest")
actual_image_sha256=$(sha256sum "$native_tools_image" | awk '{print $1}')
if [[ ! "$expected_image_sha256" =~ ^[0-9a-f]{64}$ || "$actual_image_sha256" != "$expected_image_sha256" ]]; then
    echo "converter image checksum does not match the installed release" >&2
    exit 2
fi
expected_controller_sha256=$(sed -n 's/^controller_source_sha256=//p' "$release_manifest")
actual_controller_sha256=$("$python_bin" - \
    "$release/requirements.lock" \
    "$app_root/scaling_neuro_processor" <<'CONTROLLER_DIGEST_PY'
from pathlib import Path
import hashlib
import sys

requirements = Path(sys.argv[1])
package = Path(sys.argv[2])
files = sorted(path for path in package.rglob("*.py") if path.is_file())
if not requirements.is_file() or requirements.is_symlink() or not files:
    raise SystemExit(2)
if any(path.is_symlink() for path in files):
    raise SystemExit(2)
entries = [("requirements.lock", requirements)]
entries.extend(
    (f"scaling_neuro_processor/{path.relative_to(package).as_posix()}", path)
    for path in files
)
digest = hashlib.sha256()
for logical_name, path in entries:
    name = logical_name.encode("utf-8")
    raw = path.read_bytes()
    digest.update(len(name).to_bytes(4, "big"))
    digest.update(name)
    digest.update(len(raw).to_bytes(8, "big"))
    digest.update(raw)
print(digest.hexdigest())
CONTROLLER_DIGEST_PY
)
if [[ ! "$expected_controller_sha256" =~ ^[0-9a-f]{64}$ || "$actual_controller_sha256" != "$expected_controller_sha256" ]]; then
    echo "controller source checksum does not match the installed release" >&2
    exit 2
fi

child_pid=""
stop_processor() {
    if [[ -n "$child_pid" ]]; then
        kill -TERM "$child_pid" 2>/dev/null || true
        wait "$child_pid" || true
    fi
}
trap stop_processor USR1 TERM INT

"$srun_bin" \
    "$env_bin" \
        "PYTHONPATH=$app_root" \
        PYTHONNOUSERSITE=1 \
        PYTHONDONTWRITEBYTECODE=1 \
        PYTHONSAFEPATH=1 \
        TZ=UTC \
        "$python_bin" -m scaling_neuro_processor \
        --api-url "$api_url" \
        --token-file "$token_file" \
        --work-root "$work_root" \
        --processor-id "slurm-native-${SLURM_JOB_ID}-${SLURM_PROCID:-0}" \
        --claim-input-format dicom-series-v1 \
        --native-tools-slurm-image "$native_tools_image" \
        --slurm-srun-bin "$srun_bin" \
        --slurm-job-id "$SLURM_JOB_ID" \
        --idle-exit-after 0 &
child_pid=$!
wait "$child_pid"
