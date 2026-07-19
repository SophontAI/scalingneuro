#!/bin/bash
#SBATCH --job-name=scaling-neuro-processor
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
export PATH="/opt/slurm/bin:/usr/bin:/bin"

if [[ $# -lt 1 || $# -gt 4 ]]; then
    echo "usage: sbatch run-processor.sbatch IMAGE [API_URL [WORK_ROOT [TOKEN_FILE]]]" >&2
    exit 2
fi

image=$1
api_url=${2:-https://scalingneuro.com}
work_root=${3:-/data/paul/scaling-neuro/processor}
token_file=${4:-/data/paul/scaling-neuro/secrets/processor-token}

if [[ ! -r "$image" || ! -r "$token_file" ]]; then
    echo "processor image or token file is not readable" >&2
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

/opt/slurm/bin/srun \
    --export=ENROOT_RESTRICT_DEV=yes \
    --container-image="$image" \
    --container-readonly \
    --no-container-mount-home \
    --no-container-remap-root \
    --no-container-entrypoint \
    --container-mounts="$work_root:$work_root:rw+rprivate,$token_file:/run/secrets/scaling-neuro-processor-token:ro+rprivate" \
    --container-workdir=/app \
    /app/scripts/entrypoint.sh \
        --api-url "$api_url" \
        --token-file /run/secrets/scaling-neuro-processor-token \
        --work-root "$work_root" \
        --processor-id "slurm-${SLURM_JOB_ID}-${SLURM_PROCID:-0}" \
        --idle-exit-after 0 &
child_pid=$!
wait "$child_pid"
