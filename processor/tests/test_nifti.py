from __future__ import annotations

import gzip
import hashlib
import io
from pathlib import Path
import struct
import tempfile
import unittest

from scaling_neuro_processor.errors import InvalidNifti
from scaling_neuro_processor.nifti import (
    deterministic_gzip,
    inspect_gzip_nifti,
    inspect_nifti_stream,
)

from tests.helpers import nifti_bytes


class NiftiTests(unittest.TestCase):
    def test_validates_functional_4d_header_and_stream(self) -> None:
        raw = nifti_bytes()
        facts = inspect_nifti_stream(io.BytesIO(raw))
        self.assertEqual(facts.dimensions, [8, 8, 8, 10])
        self.assertEqual(facts.orientation, "RAS")
        self.assertEqual(facts.tr_seconds, 2.0)
        self.assertEqual(facts.uncompressed_sha256, hashlib.sha256(raw).hexdigest())

    def test_rejects_fewer_than_ten_volumes(self) -> None:
        with self.assertRaisesRegex(InvalidNifti, "NIFTI_DIMENSIONS_INVALID"):
            inspect_nifti_stream(io.BytesIO(nifti_bytes(volumes=9)))

    def test_rejects_uncompressed_hash_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "scan.nii.gz"
            path.write_bytes(gzip.compress(nifti_bytes(), mtime=0))
            with self.assertRaisesRegex(
                InvalidNifti, "NIFTI_UNCOMPRESSED_HASH_MISMATCH"
            ):
                inspect_gzip_nifti(path, "0" * 64)

    def test_rejects_constant_voxel_signal(self) -> None:
        raw = nifti_bytes()
        constant = raw[:352] + b"\0" * (len(raw) - 352)
        with self.assertRaisesRegex(InvalidNifti, "NIFTI_SIGNAL_CONSTANT"):
            inspect_nifti_stream(io.BytesIO(constant))

    def test_rejects_nonfinite_float_voxel(self) -> None:
        header = bytearray(nifti_bytes()[:352])
        struct.pack_into("<h", header, 70, 16)
        struct.pack_into("<h", header, 72, 32)
        voxel_count = 8 * 8 * 8 * 10
        payload = struct.pack("<f", float("nan")) + b"\0" * ((voxel_count - 1) * 4)
        with self.assertRaisesRegex(InvalidNifti, "NIFTI_SIGNAL_NONFINITE"):
            inspect_nifti_stream(io.BytesIO(bytes(header) + payload))

    def test_deterministic_gzip(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "scan.nii"
            first = Path(directory) / "first.nii.gz"
            second = Path(directory) / "second.nii.gz"
            source.write_bytes(nifti_bytes())
            deterministic_gzip(source, first)
            deterministic_gzip(source, second)
            self.assertEqual(first.read_bytes(), second.read_bytes())


if __name__ == "__main__":
    unittest.main()
