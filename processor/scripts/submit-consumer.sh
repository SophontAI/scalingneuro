#!/bin/sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 4 ]; then
    echo "usage: submit-consumer.sh IMAGE [API_URL [WORK_ROOT [TOKEN_FILE]]]" >&2
    exit 2
fi

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
exec /opt/slurm/bin/sbatch "$script_dir/../slurm/run-processor.sbatch" "$@"
