#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 || ! "$1" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ ]]; then
  echo "usage: launch-native-on-upload.sh UPLOAD_ID" >&2
  exit 2
fi

readonly job_name=scaling-neuro-processor-native
readonly squeue=/opt/slurm/bin/squeue
readonly sbatch=/opt/slurm/bin/sbatch
readonly flock=/usr/bin/flock
readonly launch_lock=/data/paul/scaling-neuro/launcher/submit.lock
readonly batch_script=/data/paul/scaling-neuro/source/processor/slurm/run-processor-native.sbatch

for executable in "$squeue" "$sbatch" "$flock"; do
  [[ -x "$executable" ]] || { echo "required executable unavailable: $executable" >&2; exit 2; }
done
[[ -r "$batch_script" ]] || { echo "processor batch script is unavailable" >&2; exit 2; }

exec 9>"$launch_lock"
"$flock" --exclusive --wait 30 9

active_job() {
  "$squeue" \
    --noheader \
    --user "$(id -un)" \
    --name "$job_name" \
    --states PENDING,RUNNING,CONFIGURING,COMPLETING,REQUEUED,RESIZING,SUSPENDED \
    --format '%A' | head -n 1
}

if [[ -n "$(active_job)" ]]; then
  # The receipt is already durable before this event is sent. Give a running
  # consumer more than one 15-second poll interval to claim it. If the job is
  # in its shutdown race and disappears, submit a replacement below.
  sleep 20
fi

if [[ -z "$(active_job)" ]]; then
  "$sbatch" \
    "$batch_script" \
    /data/paul/scaling-neuro/native/releases/0.2.0 \
    https://scalingneuro.com \
    /data/paul/scaling-neuro/processor \
    /data/paul/scaling-neuro/secrets/processor-token >/dev/null
fi
