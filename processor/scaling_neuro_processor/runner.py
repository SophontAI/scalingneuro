from __future__ import annotations

import logging
import shutil
from threading import Event, Thread
import time

from .api import ControlPlane
from .config import Config
from .errors import CapacityFailure, LeaseLost, ProcessorError
from .models import Job
from .pipeline import process_job


LOGGER = logging.getLogger("scaling-neuro-processor")


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


def _report_failure(api: ControlPlane, job: Job, error: ProcessorError) -> None:
    if isinstance(error, LeaseLost):
        return
    try:
        api.fail(job, retryable=error.retryable, error_code=error.code)
    except ProcessorError:
        LOGGER.warning(
            "job=%s phase=fail_report status=deferred code=%s", job.job_id, error.code
        )


def _clean_partial_workspace(config: Config, job: Job) -> None:
    job_root = config.job_root(job.job_id)
    for name in ("stage", "outputs"):
        shutil.rmtree(job_root / name, ignore_errors=True)


def run(
    config: Config, shutdown: Event, *, monotonic=time.monotonic, sleep=time.sleep
) -> int:
    config.work_root.mkdir(parents=True, exist_ok=True, mode=0o700)
    api = ControlPlane(config, sleep=sleep)
    processed = 0
    idle_since = monotonic()
    control_plane_connected = False
    while not shutdown.is_set():
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
            if isinstance(error, CapacityFailure):
                _clean_partial_workspace(config, job)
            elif not error.retryable:
                shutil.rmtree(config.job_root(job.job_id), ignore_errors=True)
        except OSError:
            storage_error = CapacityFailure("PROCESSOR_STORAGE_UNAVAILABLE")
            LOGGER.error(
                "job=%s phase=processing status=failed code=%s",
                job.job_id,
                storage_error.code,
            )
            _report_failure(api, job, storage_error)
            _clean_partial_workspace(config, job)
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
