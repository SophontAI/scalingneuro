from __future__ import annotations

from pathlib import Path
import tempfile
from threading import Event
import unittest
from unittest.mock import Mock, patch

from scaling_neuro_processor.config import Config
from scaling_neuro_processor.errors import CapacityFailure
from scaling_neuro_processor.runner import run


class RunnerTests(unittest.TestCase):
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

    def test_capacity_and_storage_errors_clean_only_partial_workspace(self) -> None:
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
            )
            job_root = config.job_root(job.job_id)
            retained_archive = job_root / "input.tar.zst"

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
                    retained_archive.write_bytes(b"checkpointed archive")
                    api = Mock()
                    api.config = config
                    api.claim.return_value = job
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
                    self.assertFalse((job_root / "stage").exists())
                    self.assertFalse((job_root / "outputs").exists())
                    self.assertTrue(retained_archive.exists())


if __name__ == "__main__":
    unittest.main()
