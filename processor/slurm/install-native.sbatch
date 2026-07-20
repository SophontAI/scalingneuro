#!/bin/bash
#SBATCH --job-name=scaling-neuro-install
#SBATCH --account=sophont
#SBATCH --partition=c
#SBATCH --qos=bottom
#SBATCH --requeue
#SBATCH --export=NIL
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=2
#SBATCH --mem=4G
#SBATCH --time=00:30:00
#SBATCH --output=/data/paul/scaling-neuro/logs/%x-%j.out
#SBATCH --error=/data/paul/scaling-neuro/logs/%x-%j.err

set -euo pipefail
export PATH="/opt/slurm/bin:/usr/local/bin:/usr/bin:/bin"

if [[ $# -lt 1 || $# -gt 3 ]]; then
    echo "usage: sbatch install-native.sbatch PROCESSOR_SOURCE [INSTALL_ROOT [PYTHON]]" >&2
    exit 2
fi

processor_source=$1
install_root=${2:-/data/paul/scaling-neuro/native/releases/0.2.0}
python_bin=${3:-python3.12}

exec "$processor_source/scripts/install-native-on-compute.sh" \
    "$processor_source" \
    "$install_root" \
    "$python_bin"
