from __future__ import annotations

import copy
import io
from pathlib import Path
from types import SimpleNamespace
import tempfile
from threading import Event
import unittest
from unittest.mock import Mock, patch

from pydicom import dcmread

from scaling_neuro_processor.archive import (
    ArchiveManifest,
    CLASSIFICATION_EVIDENCE,
    MAX_DICOM_BYTES,
    MAX_DICOM_INSTANCES,
    MAX_MANIFEST_BYTES,
    _archive_extraction_contract_bytes,
    _canonical_tar_header,
    extract_archive,
    validate_manifest,
)
from scaling_neuro_processor.config import Config
from scaling_neuro_processor.dicom_privacy import audit_dicom
from scaling_neuro_processor.errors import CapacityFailure, InvalidArchive, InvalidJob
from scaling_neuro_processor.models import DicomInput, Download, Job
from scaling_neuro_processor.pipeline import (
    CONVERSION_WORKING_SET_FACTOR,
    prepare_dicom_job,
)

from tests.helpers import (
    ARCHIVE_ID,
    SERIES_ID,
    SUBJECT_ID,
    archive_manifest,
    canonical_json,
    make_dicom,
)


GIB = 1024**3
MIB = 1024**2


def _usage(free: int) -> SimpleNamespace:
    return SimpleNamespace(total=free, used=0, free=free)


def _statvfs(free_inodes: int) -> SimpleNamespace:
    return SimpleNamespace(f_favail=free_inodes)


def _mock_process(stdout: bytes) -> Mock:
    process = Mock()
    process.stdout = io.BytesIO(stdout)
    process.poll.return_value = None
    process.wait.return_value = 0
    return process


class ProcessorContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.config = Config(
            api_url="http://127.0.0.1",
            token="test",
            work_root=self.root / "work",
            processor_id="test",
            allow_insecure_http=True,
            allowed_object_hosts=("127.0.0.1",),
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_accepts_every_bounded_classification_evidence_code(self) -> None:
        expected = {
            "echo_planar_pulse_sequence",
            "echo_planar_scanning_sequence",
            "functional_image_type",
            "echo_planar_sequence",
            "functional_protocol_label",
            "functional_tr_range",
            "multiple_temporal_positions",
        }
        self.assertEqual(CLASSIFICATION_EVIDENCE, expected)
        dicom = make_dicom(self.root / "evidence.dcm")
        base = archive_manifest(dicom)
        for code in sorted(expected):
            with self.subTest(code=code):
                manifest = copy.deepcopy(base)
                manifest["classification"]["evidence"] = [
                    {
                        "code": code,
                        "source": "dicom_header",
                        "effect": "supports",
                    }
                ]
                result = validate_manifest(
                    canonical_json(manifest),
                    expected_series_archive_id=ARCHIVE_ID,
                    expected_series_id=SERIES_ID,
                    expected_dicom_count=1,
                )
                self.assertEqual(
                    result.value["classification"]["evidence"][0]["code"], code
                )

    def test_processor_claim_accepts_500000_instances_and_rejects_500001(self) -> None:
        claim = {
            "schema_version": "1.0.0",
            "job_id": "job-count-boundary",
            "upload_id": "upload-count-boundary",
            "bundle_id": ARCHIVE_ID,
            "series_archive_id": ARCHIVE_ID,
            "series_id": SERIES_ID,
            "attempt": 1,
            "lease_token": "lease-token",
            "lease_expires_at": "2030-01-01T00:00:00Z",
            "input_format": "dicom-series-v1",
            "input": {
                "format": "dicom-tar-zstd",
                "dicom_count": MAX_DICOM_INSTANCES,
                "url": "http://127.0.0.1/archive",
                "size_bytes": 32,
                "sha256": "a" * 64,
            },
        }
        self.assertEqual(
            Job.from_json(claim).input.dicom_count,  # type: ignore[union-attr]
            500_000,
        )
        claim["input"]["dicom_count"] = 500_001
        with self.assertRaises(InvalidJob):
            Job.from_json(claim)

    def test_archive_resource_contract_has_exact_release_limits(self) -> None:
        self.assertEqual(MAX_DICOM_INSTANCES, 500_000)
        self.assertEqual(MAX_DICOM_BYTES, 256 * MIB)
        self.assertEqual(MAX_MANIFEST_BYTES, 128 * MIB)
        self.assertEqual(self.config.max_archive_uncompressed_bytes, 64 * GIB)
        self.assertEqual(self.config.archive_expansion_floor_bytes, 64 * MIB)
        self.assertEqual(self.config.archive_expansion_ratio, 20)
        self.assertEqual(self.config.disk_reserve_bytes, 20 * GIB)
        self.assertEqual(self.config.inode_reserve, 1024)

        self.assertEqual(_archive_extraction_contract_bytes(self.config, 1), 64 * MIB)
        self.assertEqual(
            _archive_extraction_contract_bytes(self.config, 3 * GIB), 60 * GIB
        )
        self.assertEqual(
            _archive_extraction_contract_bytes(self.config, 4 * GIB), 64 * GIB
        )
        self.assertEqual(
            _archive_extraction_contract_bytes(self.config, 100 * GIB), 64 * GIB
        )

    def _extract_mock_stream(
        self,
        archive_size: int,
        stdout: bytes,
        destination_name: str,
        *,
        free_bytes: int | None = None,
        free_inodes: int = 1_000_000,
    ) -> Path:
        archive = self.root / f"{destination_name}.tar.zst"
        with archive.open("wb") as stream:
            stream.truncate(archive_size)
        destination = self.root / destination_name
        process = _mock_process(stdout)
        with (
            patch(
                "scaling_neuro_processor.archive.shutil.disk_usage",
                return_value=_usage(
                    free_bytes
                    if free_bytes is not None
                    else self.config.disk_reserve_bytes + 128 * GIB
                ),
            ),
            patch(
                "scaling_neuro_processor.archive.os.statvfs",
                return_value=_statvfs(free_inodes),
            ),
            patch(
                "scaling_neuro_processor.archive.subprocess.Popen",
                return_value=process,
            ),
        ):
            extract_archive(
                self.config,
                archive,
                destination,
                expected_series_archive_id=ARCHIVE_ID,
                expected_series_id=SERIES_ID,
                expected_dicom_count=1,
            )
        return destination

    def test_rejects_oversized_members_before_reading_their_payload(self) -> None:
        cases = (
            (
                "dicom-member-too-large",
                _canonical_tar_header("dicom/000001.dcm", 256 * MIB + 1),
                "ARCHIVE_DICOM_SIZE_INVALID",
            ),
            (
                "manifest-member-too-large",
                _canonical_tar_header("manifest.json", 128 * MIB + 1),
                "ARCHIVE_MANIFEST_TOO_LARGE",
            ),
        )
        for name, header, code in cases:
            with self.subTest(name=name):
                with self.assertRaisesRegex(InvalidArchive, code):
                    self._extract_mock_stream(32, header, name)
                self.assertFalse((self.root / name).exists())

    def test_enforces_twenty_to_one_contract_without_expanding_sparse_fixture(
        self,
    ) -> None:
        archive_size = 4 * MIB
        first_byte_over_contract = archive_size * 20 + 1
        header = _canonical_tar_header("dicom/000001.dcm", first_byte_over_contract)
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_UNCOMPRESSED_LIMIT"):
            self._extract_mock_stream(
                archive_size,
                header,
                "ratio-limit",
            )
        self.assertFalse((self.root / "ratio-limit").exists())

    def test_low_free_bytes_and_inodes_are_retryable_and_leave_no_stage(self) -> None:
        archive = self.root / "capacity.tar.zst"
        archive.write_bytes(b"x" * 32)
        cases = (
            (
                "low-bytes",
                _usage(self.config.disk_reserve_bytes),
                _statvfs(1_000_000),
                "LOW_DISK_SPACE",
            ),
            (
                "low-inodes",
                _usage(self.config.disk_reserve_bytes + GIB),
                _statvfs(500_000 + self.config.inode_reserve - 1),
                "LOW_INODE_SPACE",
            ),
        )
        for name, usage, statvfs, code in cases:
            destination = self.root / name
            with (
                self.subTest(name=name),
                patch(
                    "scaling_neuro_processor.archive.shutil.disk_usage",
                    return_value=usage,
                ),
                patch(
                    "scaling_neuro_processor.archive.os.statvfs",
                    return_value=statvfs,
                ),
                self.assertRaisesRegex(CapacityFailure, code) as raised,
            ):
                extract_archive(
                    self.config,
                    archive,
                    destination,
                    expected_series_archive_id=ARCHIVE_ID,
                    expected_series_id=SERIES_ID,
                    expected_dicom_count=500_000,
                )
            self.assertTrue(raised.exception.retryable)
            self.assertFalse(destination.exists())

    def test_mid_extraction_capacity_failure_removes_partial_stage(self) -> None:
        header = _canonical_tar_header("dicom/000001.dcm", 2)
        with self.assertRaises(CapacityFailure) as raised:
            self._extract_mock_stream(
                32,
                header,
                "mid-extraction-capacity",
                free_bytes=self.config.disk_reserve_bytes + 1,
            )
        self.assertTrue(raised.exception.retryable)
        self.assertFalse((self.root / "mid-extraction-capacity").exists())

    def test_filesystem_write_error_is_retryable_and_removes_partial_stage(
        self,
    ) -> None:
        archive = self.root / "write-error.tar.zst"
        archive.write_bytes(b"x" * 32)
        destination = self.root / "write-error"
        process = _mock_process(
            _canonical_tar_header("dicom/000001.dcm", 1) + b"x" + b"\0" * 511
        )
        original_open = Path.open

        def fail_dicom_open(path: Path, *args, **kwargs):  # noqa: ANN002, ANN003
            if path.suffix == ".dcm":
                raise OSError("synthetic storage failure")
            return original_open(path, *args, **kwargs)

        with (
            patch(
                "scaling_neuro_processor.archive.shutil.disk_usage",
                return_value=_usage(self.config.disk_reserve_bytes + GIB),
            ),
            patch(
                "scaling_neuro_processor.archive.os.statvfs",
                return_value=_statvfs(1_000_000),
            ),
            patch(
                "scaling_neuro_processor.archive.subprocess.Popen",
                return_value=process,
            ),
            patch.object(Path, "open", fail_dicom_open),
            self.assertRaisesRegex(
                CapacityFailure, "PROCESSOR_STORAGE_UNAVAILABLE"
            ) as raised,
        ):
            extract_archive(
                self.config,
                archive,
                destination,
                expected_series_archive_id=ARCHIVE_ID,
                expected_series_id=SERIES_ID,
                expected_dicom_count=1,
            )
        self.assertTrue(raised.exception.retryable)
        self.assertFalse(destination.exists())

    def test_conversion_working_set_is_preflighted_before_converter(self) -> None:
        archive = Download(
            url="http://127.0.0.1/archive",
            size_bytes=32,
            sha256="a" * 64,
            headers={},
        )
        descriptor = DicomInput(
            archive=archive,
            format="dicom-tar-zstd",
            dicom_count=1,
        )
        job = Job(
            job_id="job-working-set",
            upload_id="upload-working-set",
            series_archive_id=ARCHIVE_ID,
            series_id=SERIES_ID,
            attempt=1,
            lease_token="lease-token",
            lease_expires_at="2030-01-01T00:00:00Z",
            input_format="dicom-series-v1",
            input=descriptor,
        )
        transport = Mock()
        transport.download.side_effect = lambda _descriptor, path: path.write_bytes(
            b"x" * 32
        )
        active = Event()
        active.set()
        manifest = ArchiveManifest(
            value={"dicom_instance_count": 1},
            sha256="b" * 64,
            extracted_bytes=100,
        )
        reserve = self.config.disk_reserve_bytes
        with (
            patch(
                "scaling_neuro_processor.pipeline.shutil.disk_usage",
                side_effect=(
                    _usage(reserve + archive.size_bytes),
                    _usage(
                        reserve
                        + manifest.extracted_bytes * CONVERSION_WORKING_SET_FACTOR
                        - 1
                    ),
                ),
            ),
            patch(
                "scaling_neuro_processor.pipeline.extract_archive",
                return_value=manifest,
            ),
            patch("scaling_neuro_processor.pipeline.convert") as converter,
            self.assertRaises(CapacityFailure) as raised,
        ):
            prepare_dicom_job(
                self.config,
                job,
                descriptor,
                transport,
                active,
            )
        self.assertTrue(raised.exception.retryable)
        converter.assert_not_called()

    def test_server_requires_exact_longitudinal_temporal_marker(self) -> None:
        path = self.root / "longitudinal-marker.dcm"
        make_dicom(path)
        self.assertEqual(
            audit_dicom(path, expected_subject_id=SUBJECT_ID).sop_instance_uid,
            "2.25.123456789012345678901234567890123456",
        )

        for value in (None, "MODIFIED"):
            with self.subTest(value=value):
                make_dicom(path)
                dataset = dcmread(path)
                if value is None:
                    del dataset.LongitudinalTemporalInformationModified
                else:
                    dataset.LongitudinalTemporalInformationModified = value
                dataset.save_as(path, enforce_file_format=True)
                with self.assertRaisesRegex(
                    InvalidArchive, "DICOM_PRIVACY_AUDIT_FAILED"
                ):
                    audit_dicom(path, expected_subject_id=SUBJECT_ID)


if __name__ == "__main__":
    unittest.main()
