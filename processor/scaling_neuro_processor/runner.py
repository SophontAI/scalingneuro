from __future__ import annotations

import logging
from pathlib import Path
import shutil
import stat
from threading import Event, Thread
import time
from typing import Literal

from .api import ControlPlane
from .config import Config
from .errors import CapacityFailure, LeaseLost, ProcessorError
from .models import Job
from .pipeline import process_job


LOGGER = logging.getLogger("scaling-neuro-processor")
STALE_WORKSPACE_RETENTION_SECONDS = 72 * 60 * 60
WORKSPACE_GC_INTERVAL_SECONDS = 60 * 60


class Heartbeat(Thread):
    def __init__(self, api: ControlPlane, job: Job, active: Event, stop: Event) -> None:
        super().__init__(name="lease-heartbeat", daemon=True)
        self.api = api
        self.job = job
        self.active = active
        self.stop_event = stop

    def run(self) -> None:
        while not self.stop_event.wait(self.api.config.heartbeat_seconds):
            try:
                self.api.heartbeat(self.job)
            except ProcessorError:
                self.active.clear()
                LOGGER.warning(
                    "job=%s phase=heartbeat status=lease_uncertain", self.job.job_id
                )
                return


def _report_failure(
    api: ControlPlane, job: Job, error: ProcessorError
) -> Literal["queued", "failed"] | None:
    if isinstance(error, LeaseLost):
        return None
    try:
        return api.fail(job, retryable=error.retryable, error_code=error.code)
    except ProcessorError:
        LOGGER.warning(
            "job=%s phase=fail_report status=deferred code=%s", job.job_id, error.code
        )
        return None


def _garbage_collect_stale_workspaces(
    work_root: Path, *, now: float | None = None
) -> None:
    jobs_root = work_root / "jobs"
    try:
        entries = list(jobs_root.iterdir())
    except FileNotFoundError:
        return
    except OSError:
        LOGGER.warning("phase=workspace_gc status=deferred")
        return
    cutoff = (time.time() if now is None else now) - STALE_WORKSPACE_RETENTION_SECONDS
    for entry in entries:
        try:
            metadata = entry.lstat()
        except OSError:
            continue
        if (
            not stat.S_ISDIR(metadata.st_mode)
            or len(entry.name) != 32
            or any(character not in "0123456789abcdef" for character in entry.name)
            or metadata.st_mtime > cutoff
        ):
            continue
        shutil.rmtree(entry, ignore_errors=True)


def run(
    config: Config, shutdown: Event, *, monotonic=time.monotonic, sleep=time.sleep
) -> int:
    config.work_root.mkdir(parents=True, exist_ok=True, mode=0o700)
    _garbage_collect_stale_workspaces(config.work_root)
    next_workspace_gc = time.monotonic() + WORKSPACE_GC_INTERVAL_SECONDS
    api = ControlPlane(config, sleep=sleep)
    processed = 0
    idle_since = monotonic()
    control_plane_connected = False
    while not shutdown.is_set():
        if time.monotonic() >= next_workspace_gc:
            _garbage_collect_stale_workspaces(config.work_root)
            next_workspace_gc = time.monotonic() + WORKSPACE_GC_INTERVAL_SECONDS
        try:
            job = api.claim()
        except ProcessorError as error:
            LOGGER.warning("phase=claim status=retry code=%s", error.code)
            if shutdown.wait(config.idle_seconds):
                break
            continue
        if not control_plane_connected:
            LOGGER.info("phase=control_plane status=connected")
            control_plane_connected = True
        if job is None:
            if (
                config.idle_exit_after_seconds
                and monotonic() - idle_since >= config.idle_exit_after_seconds
            ):
                LOGGER.info("phase=poll status=idle_exit")
                break
            if shutdown.wait(config.idle_seconds):
                break
            continue
        idle_since = monotonic()
        LOGGER.info(
            "job=%s phase=claimed input_format=%s attempt=%d",
            job.job_id,
            job.input_format,
            job.attempt,
        )
        lease_active = Event()
        lease_active.set()
        heartbeat_stop = Event()
        heartbeat = Heartbeat(api, job, lease_active, heartbeat_stop)
        heartbeat.start()

        def propagate_shutdown() -> None:
            while not heartbeat_stop.wait(0.5):
                if shutdown.is_set():
                    lease_active.clear()
                    return

        shutdown_watcher = Thread(
            target=propagate_shutdown, name="shutdown-watcher", daemon=True
        )
        shutdown_watcher.start()
        try:
            process_job(config, api, job, lease_active)
            LOGGER.info("job=%s phase=complete status=processed", job.job_id)
        except ProcessorError as error:
            LOGGER.warning(
                "job=%s phase=processing status=failed code=%s", job.job_id, error.code
            )
            _report_failure(api, job, error)
            job_root = config.job_root(job.job_id, job.attempt, job.lease_token)
            # A retry always receives a new attempt and/or lease token, hence a
            # different workspace. Retaining this exact lease's archive or
            # extracted pixels can only duplicate patient-bearing scratch data.
            shutil.rmtree(job_root, ignore_errors=True)
        except OSError:
            storage_error = CapacityFailure("PROCESSOR_STORAGE_UNAVAILABLE")
            LOGGER.error(
                "job=%s phase=processing status=failed code=%s",
                job.job_id,
                storage_error.code,
            )
            _report_failure(api, job, storage_error)
            shutil.rmtree(
                config.job_root(job.job_id, job.attempt, job.lease_token),
                ignore_errors=True,
            )
        except Exception:
            internal_error = ProcessorError("INTERNAL_PROCESSOR_ERROR", retryable=True)
            # Never emit arbitrary exception text: it may contain signed URLs or
            # source paths. Code-only logs are sufficient for the job record.
            LOGGER.error(
                "job=%s phase=processing status=failed code=%s",
                job.job_id,
                internal_error.code,
            )
            _report_failure(api, job, internal_error)
            shutil.rmtree(
                config.job_root(job.job_id, job.attempt, job.lease_token),
                ignore_errors=True,
            )
        finally:
            heartbeat_stop.set()
            heartbeat.join(timeout=5)
            shutdown_watcher.join(timeout=1)
        processed += 1
        if config.max_jobs and processed >= config.max_jobs:
            break
        if shutil.disk_usage(config.work_root).free < config.disk_reserve_bytes:
            LOGGER.error("phase=capacity status=stopping code=LOW_DISK_SPACE")
            return 2
    return 0
