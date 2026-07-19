from __future__ import annotations

import argparse
import logging
import os
from pathlib import Path
import signal
from threading import Event
import time

from .config import Config, default_processor_id, read_token
from .errors import ProcessorError
from .runner import run


def parser() -> argparse.ArgumentParser:
    value = argparse.ArgumentParser(prog="scaling-neuro-processor")
    value.add_argument("--api-url", default="https://scalingneuro.com")
    value.add_argument(
        "--token-file",
        type=Path,
        default=Path("/run/secrets/scaling-neuro-processor-token"),
    )
    value.add_argument(
        "--work-root", type=Path, default=Path("/data/scaling-neuro/processor")
    )
    value.add_argument("--processor-id", default=default_processor_id())
    value.add_argument(
        "--claim-input-format",
        choices=("dicom-series-v1", "nifti-v1"),
        help="claim only the exact selected input format",
    )
    value.add_argument("--dcm2niix-bin", default="dcm2niix")
    value.add_argument("--native-tools-slurm-image", type=Path)
    value.add_argument("--slurm-srun-bin", default="/opt/slurm/bin/srun")
    value.add_argument(
        "--slurm-job-id",
        help="numeric parent Slurm allocation used for nested converter steps",
    )
    value.add_argument("--zstd-bin", default="zstd")
    value.add_argument("--lease-seconds", type=int, default=900)
    value.add_argument("--heartbeat-seconds", type=int, default=60)
    value.add_argument(
        "--object-transfer-timeout-seconds",
        type=int,
        default=3600,
        help="socket timeout for large object downloads and uploads",
    )
    value.add_argument("--idle-seconds", type=float, default=15)
    value.add_argument("--idle-exit-after", type=int, default=300)
    value.add_argument(
        "--max-jobs",
        type=int,
        default=0,
        help="0 processes jobs until the idle timeout",
    )
    value.add_argument(
        "--allow-insecure-http", action="store_true", help=argparse.SUPPRESS
    )
    value.add_argument("--allowed-object-host", action="append", default=[])
    value.add_argument(
        "--log-level", choices=("DEBUG", "INFO", "WARNING", "ERROR"), default="INFO"
    )
    return value


def main(argv: list[str] | None = None) -> int:
    os.umask(0o077)
    args = parser().parse_args(argv)
    logging.Formatter.converter = time.gmtime
    logging.basicConfig(
        level=getattr(logging, args.log_level),
        format="%(asctime)sZ level=%(levelname)s %(message)s",
        datefmt="%Y-%m-%dT%H:%M:%S",
    )
    shutdown = Event()

    def stop(_signum, _frame) -> None:  # noqa: ANN001
        shutdown.set()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGUSR1, stop)
    try:
        allowed_hosts = tuple(args.allowed_object_host) or (
            ".r2.cloudflarestorage.com",
        )
        config = Config(
            api_url=args.api_url,
            token=read_token(args.token_file),
            work_root=args.work_root,
            processor_id=args.processor_id,
            claim_input_format=args.claim_input_format,
            dcm2niix_bin=args.dcm2niix_bin,
            native_tools_slurm_image=args.native_tools_slurm_image,
            slurm_srun_bin=args.slurm_srun_bin,
            slurm_job_id=args.slurm_job_id,
            zstd_bin=args.zstd_bin,
            lease_seconds=args.lease_seconds,
            heartbeat_seconds=args.heartbeat_seconds,
            object_transfer_timeout_seconds=args.object_transfer_timeout_seconds,
            idle_seconds=args.idle_seconds,
            idle_exit_after_seconds=args.idle_exit_after,
            max_jobs=args.max_jobs,
            allow_insecure_http=args.allow_insecure_http,
            allowed_object_hosts=allowed_hosts,
        )
        return run(config, shutdown)
    except ProcessorError as error:
        logging.error("phase=startup status=failed code=%s", error.code)
        return 2
