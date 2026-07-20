from __future__ import annotations

from pathlib import Path
import os
import tempfile
from threading import Event
import unittest
from unittest.mock import Mock, patch

from scaling_neuro_processor.config import Config
from scaling_neuro_processor.errors import ApiFailure, CapacityFailure, LeaseLost
from scaling_neuro_processor.runner import (
    STALE_WORKSPACE_RETENTION_SECONDS,
    _garbage_collect_stale_workspaces,
    run,
)


class RunnerTests(unittest.TestCase):
    def test_workspace_identity_is_bound_to_attempt_and_lease_token(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            config = Config(
                api_url="http://127.0.0.1",
                token="test",
                work_root=Path(directory) / "work",
                processor_id="test",
                allow_insecure_http=True,
                allowed_object_hosts=("127.0.0.1",),
            )
            first = config.job_root("same-job", 1, "lease-a")
            same = config.job_root("same-job", 1, "lease-a")
            next_attempt = config.job_root("same-job", 2, "lease-a")
            next_lease = config.job_root("same-job", 1, "lease-b")

            self.assertEqual(first, same)
            self.assertNotEqual(first, next_attempt)
            self.assertNotEqual(first, next_lease)
            self.assertEqual(first.parent, config.work_root / "jobs")
            self.assertRegex(first.name, r"^[0-9a-f]{32}$")
            self.assertNotIn("same-job", str(first))
            self.assertNotIn("lease-a", str(first))

    def test_lost_attempt_cleanup_cannot_delete_successor_workspace(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            config = Config(
                api_url="http://127.0.0.1",
                token="test",
                work_root=Path(directory) / "work",
                processor_id="test",
                max_jobs=1,
                allow_insecure_http=True,
                allowed_object_hosts=("127.0.0.1",),
            )
            lost_job = Mock(
                job_id="same-job",
                input_format="dicom-series-v1",
                attempt=1,
                lease_token="lost-lease",
            )
            lost_root = config.job_root(
                lost_job.job_id, lost_job.attempt, lost_job.lease_token
            )
            successor_root = config.job_root("same-job", 2, "successor-lease")
            for root, payload in (
                (lost_root, b"lost"),
                (successor_root, b"successor"),
            ):
                root.mkdir(parents=True)
                (root / "checkpoint").write_bytes(payload)
            api = Mock()
            api.config = config
            api.claim.return_value = lost_job

            with (
                patch(
                    "scaling_neuro_processor.runner.ControlPlane",
                    return_value=api,
                ),
                patch(
                    "scaling_neuro_processor.runner.process_job",
                    side_effect=LeaseLost(),
                ),
            ):
                self.assertEqual(run(config, Event()), 0)

            self.assertFalse(lost_root.exists())
            self.assertEqual(
                (successor_root / "checkpoint").read_bytes(), b"successor"
            )
            api.fail.assert_not_called()

    def test_stale_workspace_gc_is_bounded_and_ignores_unowned_paths(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            work_root = Path(directory) / "work"
            old = work_root / "jobs" / ("a" * 32)
            fresh = work_root / "jobs" / ("b" * 32)
            unrelated = work_root / "jobs" / "operator-notes"
            for path in (old, fresh, unrelated):
                path.mkdir(parents=True)
                (path / "payload").write_bytes(b"test")
            now = 1_000_000.0
            os.utime(old, (now - STALE_WORKSPACE_RETENTION_SECONDS - 1,) * 2)
            os.utime(fresh, (now, now))
            os.utime(unrelated, (now - STALE_WORKSPACE_RETENTION_SECONDS - 1,) * 2)

            _garbage_collect_stale_workspaces(work_root, now=now)

            self.assertFalse(old.exists())
            self.assertTrue(fresh.exists())
            self.assertTrue(unrelated.exists())

    def test_logs_first_successful_control_plane_response(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            config = Config(
                api_url="http://127.0.0.1",
                token="test",
                work_root=Path(directory) / "work",
                processor_id="test",
                idle_exit_after_seconds=1,
                allow_insecure_http=True,
                allowed_object_hosts=("127.0.0.1",),
            )
            monotonic = Mock(side_effect=(0.0, 2.0))
            with (
                patch("scaling_neuro_processor.runner.ControlPlane") as control_plane,
                self.assertLogs("scaling-neuro-processor", level="INFO") as logs,
            ):
                control_plane.return_value.claim.return_value = None
                result = run(config, Event(), monotonic=monotonic)

        self.assertEqual(result, 0)
        self.assertEqual(
            sum("phase=control_plane status=connected" in line for line in logs.output),
            1,
        )

    def test_capacity_and_storage_errors_remove_exact_workspace(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            config = Config(
                api_url="http://127.0.0.1",
                token="test",
                work_root=Path(directory) / "work",
                processor_id="test",
                max_jobs=1,
                allow_insecure_http=True,
                allowed_object_hosts=("127.0.0.1",),
            )
            job = Mock(
                job_id="job-capacity",
                input_format="dicom-series-v1",
                attempt=1,
                lease_token="lease-capacity",
            )
            job_root = config.job_root(job.job_id, job.attempt, job.lease_token)
            for error, expected_code in (
                (CapacityFailure(), "LOW_DISK_SPACE"),
                (
                    OSError("synthetic filesystem failure"),
                    "PROCESSOR_STORAGE_UNAVAILABLE",
                ),
            ):
                with self.subTest(expected_code=expected_code):
                    (job_root / "stage").mkdir(parents=True, exist_ok=True)
                    (job_root / "outputs").mkdir(parents=True, exist_ok=True)
                    (job_root / "stage" / "partial.dcm").write_bytes(b"partial")
                    (job_root / "outputs" / "partial.nii").write_bytes(b"partial")
                    (job_root / "input.tar.zst").write_bytes(
                        b"patient-bearing archive"
                    )
                    api = Mock()
                    api.config = config
                    api.claim.return_value = job
                    api.fail.return_value = "queued"
                    with (
                        patch(
                            "scaling_neuro_processor.runner.ControlPlane",
                            return_value=api,
                        ),
                        patch(
                            "scaling_neuro_processor.runner.process_job",
                            side_effect=error,
                        ),
                    ):
                        self.assertEqual(run(config, Event()), 0)

                    api.fail.assert_called_once_with(
                        job,
                        retryable=True,
                        error_code=expected_code,
                    )
                    self.assertFalse(job_root.exists())

    def test_unreported_retryable_failure_removes_exact_workspace(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            config = Config(
                api_url="http://127.0.0.1",
                token="test",
                work_root=Path(directory) / "work",
                processor_id="test",
                max_jobs=1,
                allow_insecure_http=True,
                allowed_object_hosts=("127.0.0.1",),
            )
            job = Mock(
                job_id="job-unreported-retry",
                input_format="dicom-series-v1",
                attempt=2,
                lease_token="lease-unreported-retry",
            )
            job_root = config.job_root(job.job_id, job.attempt, job.lease_token)
            successor_root = config.job_root(job.job_id, 3, "successor-lease")
            for root, payload in (
                (job_root, b"patient-bearing archive"),
                (successor_root, b"successor"),
            ):
                root.mkdir(parents=True)
                (root / "input.tar.zst").write_bytes(payload)
            api = Mock()
            api.config = config
            api.claim.return_value = job
            api.fail.side_effect = ApiFailure("FAIL_REPORT_UNAVAILABLE")
            with (
                patch(
                    "scaling_neuro_processor.runner.ControlPlane",
                    return_value=api,
                ),
                patch(
                    "scaling_neuro_processor.runner.process_job",
                    side_effect=ApiFailure("OBJECT_DOWNLOAD_UNAVAILABLE"),
                ),
            ):
                self.assertEqual(run(config, Event()), 0)

            self.assertFalse(job_root.exists())
            self.assertEqual(
                (successor_root / "input.tar.zst").read_bytes(), b"successor"
            )

    def test_terminal_retry_response_removes_the_entire_workspace(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            config = Config(
                api_url="http://127.0.0.1",
                token="test",
                work_root=Path(directory) / "work",
                processor_id="test",
                max_jobs=1,
                allow_insecure_http=True,
                allowed_object_hosts=("127.0.0.1",),
            )
            job = Mock(
                job_id="job-terminal-retry",
                input_format="dicom-series-v1",
                attempt=5,
                lease_token="lease-terminal-retry",
            )
            job_root = config.job_root(job.job_id, job.attempt, job.lease_token)
            job_root.mkdir(parents=True)
            (job_root / "input.tar.zst").write_bytes(b"patient-bearing archive")
            api = Mock()
            api.config = config
            api.claim.return_value = job
            api.fail.return_value = "failed"
            with (
                patch(
                    "scaling_neuro_processor.runner.ControlPlane",
                    return_value=api,
                ),
                patch(
                    "scaling_neuro_processor.runner.process_job",
                    side_effect=ApiFailure("OBJECT_DOWNLOAD_INTEGRITY_MISMATCH"),
                ),
            ):
                self.assertEqual(run(config, Event()), 0)

            api.fail.assert_called_once_with(
                job,
                retryable=True,
                error_code="OBJECT_DOWNLOAD_INTEGRITY_MISMATCH",
            )
            self.assertFalse(job_root.exists())


if __name__ == "__main__":
    unittest.main()
