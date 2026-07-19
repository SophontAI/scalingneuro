#!/usr/bin/env python3
"""Fast, network-free tests for vendor_dicom_qa.py helpers."""

from __future__ import annotations

import hashlib
import struct
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import vendor_dicom_qa as qa


class VendorDicomQaTests(unittest.TestCase):
    def test_uid_is_stable_and_valid(self) -> None:
        uid = qa.deterministic_uid("fixture")
        self.assertEqual(uid, qa.deterministic_uid("fixture"))
        self.assertTrue(uid.startswith("2.25."))
        self.assertLessEqual(len(uid), 64)

    def test_tree_hash_is_order_independent(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            (root / "z.dcm").write_bytes(b"z")
            (root / "a.dcm").write_bytes(b"a")
            expected = qa.dicom_tree_hash(root)
            self.assertEqual(expected, qa.dicom_tree_hash(root))

    def test_canonical_json_hash_ignores_unselected_metadata(self) -> None:
        left = {"TR": 2.0, "PatientName": "raw"}
        right = {"TR": 2.0, "PatientName": "safe"}
        self.assertEqual(
            qa.canonical_json_hash(left, ("TR",)),
            qa.canonical_json_hash(right, ("TR",)),
        )

    def test_nifti_signature_ignores_only_text_ranges(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            first = bytearray(356)
            second = bytearray(356)
            for data in (first, second):
                struct.pack_into("<I", data, 0, 348)
                struct.pack_into("<8h", data, 40, 4, 1, 1, 1, 1, 1, 1, 1)
                struct.pack_into("<f", data, 108, 352.0)
                data[352:] = b"DATA"
            first[4:8] = b"raw!"
            second[4:8] = b"safe"
            a = root / "a.nii"
            b = root / "b.nii"
            a.write_bytes(first)
            b.write_bytes(second)
            self.assertEqual(
                qa.normalized_nifti_signature(a),
                qa.normalized_nifti_signature(b),
            )
            second[352:] = b"FAIL"
            b.write_bytes(second)
            self.assertNotEqual(
                qa.normalized_nifti_signature(a),
                qa.normalized_nifti_signature(b),
            )

    def test_all_pins_are_full_sha256_or_git_sha(self) -> None:
        self.assertEqual(len(qa.DCM2NIIX_SOURCE_SHA256), 64)
        for fixture in qa.PUBLIC_FIXTURES.values():
            self.assertEqual(len(fixture.commit), 40)
            self.assertEqual(len(fixture.dicom_tree_sha256), 64)
        for digest in qa.DERIVED_FIXTURE_TREE_SHA256.values():
            self.assertEqual(len(digest), 64)

    def test_processor_archive_expectations_bind_exact_client_identity(self) -> None:
        bundle = {
            "bundle_id": "a" * 24,
            "series_id": "b" * 24,
            "source_dicom_count": 10,
            "archive": {"dicom_instance_count": 10},
        }
        self.assertEqual(
            qa.processor_archive_expectations(bundle, 10),
            ("a" * 24, "b" * 24, 10),
        )
        bundle["archive"]["dicom_instance_count"] = 9
        with self.assertRaisesRegex(
            qa.QaFailure, "cannot bind the processor archive audit"
        ):
            qa.processor_archive_expectations(bundle, 10)

    def test_pixel_inventory_audit_does_not_assume_instance_order(self) -> None:
        from types import SimpleNamespace

        def dataset(pixel: bytes, syntax: str = "1.2.840.10008.1.2.1"):
            return SimpleNamespace(
                PixelData=pixel,
                file_meta=SimpleNamespace(TransferSyntaxUID=syntax),
            )

        left = [dataset(b"a"), dataset(b"b"), dataset(b"a")]
        right = [dataset(b"b"), dataset(b"a"), dataset(b"a")]
        expected = {
            (hashlib.sha256(b"a").hexdigest(), "1.2.840.10008.1.2.1"): 2,
            (hashlib.sha256(b"b").hexdigest(), "1.2.840.10008.1.2.1"): 1,
        }
        self.assertEqual(expected, qa.pixel_inventory(left))
        self.assertEqual(expected, qa.pixel_inventory(right))


if __name__ == "__main__":
    unittest.main()
