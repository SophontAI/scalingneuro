from __future__ import annotations

import hashlib
import io
from pathlib import Path
import tempfile
import unittest

from scaling_neuro_processor.errors import InvalidNifti
from scaling_neuro_processor.metadata import validate_legacy_sidecar
from scaling_neuro_processor.nifti import inspect_nifti_stream

from tests.helpers import (
    ARCHIVE_ID,
    SERIES_ID,
    canonical_json,
    gzip_bytes,
    legacy_sidecar,
    nifti_bytes,
)


class MetadataTests(unittest.TestCase):
    def validate(self, root: Path, value: dict) -> None:
        raw_nifti = nifti_bytes()
        compressed = gzip_bytes(raw_nifti)
        path = root / "legacy.json"
        path.write_bytes(canonical_json(value))
        validate_legacy_sidecar(
            path,
            inspect_nifti_stream(io.BytesIO(raw_nifti)),
            expected_bundle_id=ARCHIVE_ID,
            expected_series_id=SERIES_ID,
            nifti_filename="legacy.nii.gz",
            nifti_size=len(compressed),
            nifti_sha256=hashlib.sha256(compressed).hexdigest(),
        )

    def test_accepts_exact_default_deny_sidecar(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            raw = nifti_bytes()
            self.validate(Path(directory), legacy_sidecar(raw, gzip_bytes(raw)))

    def test_rejects_unknown_metadata_field(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            raw = nifti_bytes()
            value = legacy_sidecar(raw, gzip_bytes(raw))
            value["source"]["station_name"] = "scanner-room-1"
            with self.assertRaisesRegex(InvalidNifti, "LEGACY_SIDECAR_INVALID"):
                self.validate(Path(directory), value)

    def test_rejects_non_normalized_converter_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            raw = nifti_bytes()
            value = legacy_sidecar(raw, gzip_bytes(raw))
            value["conversion"]["arguments"] = ["/private/source/path"]
            with self.assertRaisesRegex(InvalidNifti, "LEGACY_SIDECAR_INVALID"):
                self.validate(Path(directory), value)


if __name__ == "__main__":
    unittest.main()
