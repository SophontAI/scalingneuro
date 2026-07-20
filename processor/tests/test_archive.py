from __future__ import annotations

import copy
import hashlib
import io
import json
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch

from scaling_neuro_processor.archive import (
    PHILIPS_REQUIRED_PRIVATE_FIELDS,
    SANDBOX_ZSTD_INVALID_EXIT,
    _check_zstd_returncode,
    extract_archive,
    validate_manifest,
    zstd_decompression_command,
)
from scaling_neuro_processor.config import Config
from scaling_neuro_processor.errors import ConverterFailure, InvalidArchive
from scaling_neuro_processor.sandbox import NATIVE_ZSTD
from scaling_neuro_processor.dicom_privacy import PRIVACY_ERROR, safe_scanner_text

from tests.helpers import (
    ARCHIVE_ID,
    TEST_DISK_RESERVE_BYTES,
    archive_manifest,
    archive_manifest_v2,
    conform_enhanced_mr,
    make_archive,
    make_dicom,
    make_functional_dicom_v2,
    make_structural_dicom,
)


class ArchiveTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.config = Config(
            api_url="http://127.0.0.1",
            token="test",
            work_root=self.root / "work",
            processor_id="test",
            disk_reserve_bytes=TEST_DISK_RESERVE_BYTES,
            allow_insecure_http=True,
            allowed_object_hosts=("127.0.0.1",),
        )
        self.dicom = make_dicom(self.root / "source.dcm")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def extract(
        self,
        manifest,
        *,
        extra=None,
        header_mutator=None,
        padding_byte=0,
        expected_series_kind=None,
        expected_processing_route=None,
        dicoms=None,
    ):
        archive = (
            self.root / f"archive-{len(list(self.root.glob('archive-*')))}.tar.zst"
        )
        make_archive(
            archive,
            self.dicom,
            manifest,
            dicoms=dicoms,
            extra_member=extra,
            dicom_header_mutator=header_mutator,
            dicom_padding_byte=padding_byte,
        )
        return extract_archive(
            self.config,
            archive,
            self.root / f"extract-{len(list(self.root.glob('extract-*')))}",
            expected_series_archive_id=ARCHIVE_ID,
            expected_series_id="b" * 24,
            expected_dicom_count=len(dicoms) if dicoms is not None else 1,
            expected_series_kind=(
                expected_series_kind or manifest.get("series_kind", "functional_epi")
            ),
            expected_processing_route=(
                expected_processing_route
                or manifest.get("processing_route", "functional-epi-v1")
            ),
            expected_pixel_data_policy=manifest.get(
                "pixel_data_policy", "scanner-native-not-defaced"
            ),
        )

    def rewrite_dicom(self, mutate) -> None:
        from pydicom import dcmread

        path = self.root / "source.dcm"
        dataset = dcmread(path)
        mutate(dataset)
        conform_enhanced_mr(dataset)
        dataset.save_as(path, enforce_file_format=True)
        self.dicom = path.read_bytes()

    def rewrite_philips_dicom(
        self, mutate=lambda _dataset: None, *, include_private_contract=True
    ) -> None:
        from pydicom.tag import Tag

        def with_manufacturer(dataset) -> None:
            dataset.Manufacturer = "Philips Medical Systems"
            dataset.ManufacturerModelName = "Achieva dStream"
            dataset.SoftwareVersions = "Philips 5.1.1"
            dataset.ImageType = [
                value for value in dataset.ImageType if str(value) != "MOSAIC"
            ]
            for tag in (Tag(0x0029, 0x0010), Tag(0x0029, 0x1010)):
                if tag in dataset:
                    del dataset[tag]
            if include_private_contract:
                dataset.add_new(Tag(0x2005, 0x0010), "LO", "Philips MR Imaging DD 001")
                dataset.add_new(Tag(0x2005, 0x100D), "FL", 0.0)
                dataset.add_new(Tag(0x2005, 0x100E), "FL", 0.00363177)
                dataset.add_new(Tag(0x2001, 0x0010), "LO", "Philips Imaging DD 001")
                dataset.add_new(Tag(0x2001, 0x1018), "SL", 32)
                dataset.add_new(Tag(0x2001, 0x1022), "FL", 0.75)
            mutate(dataset)

        self.rewrite_dicom(with_manufacturer)

    def philips_manifest(self):
        manifest = archive_manifest(self.dicom)
        manifest["source"]["manufacturer"] = "Philips Medical Systems"
        manifest["source"]["model"] = "Achieva dStream"
        manifest["source"]["software_versions"] = ["Philips 5.1.1"]
        manifest["source"]["image_type"] = [
            value for value in manifest["source"]["image_type"] if value != "MOSAIC"
        ]
        manifest["deidentification"]["safe_private_exceptions"] = [
            "dicom_ps3.15_philips_scale_intercept_slope",
            "dicom_ps3.15_philips_number_of_slices",
            "dicom_ps3.15_philips_water_fat_shift",
        ]
        return manifest

    def assert_privacy_rejected(self) -> None:
        with self.assertRaisesRegex(InvalidArchive, "DICOM_PRIVACY_AUDIT_FAILED"):
            self.extract(archive_manifest(self.dicom))

    def assert_functional_purpose_downgraded(self, mutate) -> None:
        from pydicom import dcmread

        self.rewrite_dicom(mutate)
        dataset = dcmread(io.BytesIO(self.dicom), stop_before_pixels=True)
        manifest = archive_manifest(self.dicom)
        scanning_sequence = dataset.ScanningSequence
        manifest["source"]["scanning_sequence"] = (
            [str(scanning_sequence)]
            if isinstance(scanning_sequence, str)
            else [str(value) for value in scanning_sequence]
        )
        manifest["source"]["image_type"] = [str(value) for value in dataset.ImageType]
        result = self.extract(manifest)
        self.assertFalse(result.functional_epi_headers_confirmed)

    def scientific_manifest(self, kind: str):
        evidence = {
            "diffusion": (
                "diffusion_detected",
                "diffusion_scientific_metadata_contract_verified",
            ),
            "asl_perfusion": (
                "asl_or_perfusion_detected",
                "asl_scientific_metadata_contract_verified",
            ),
        }[kind]
        manifest = archive_manifest_v2(
            self.dicom,
            series_kind=kind,
            processing_route="archive-verify-v1",
            evidence_code=evidence[0],
        )
        manifest["classification"]["evidence"].append(
            {
                "code": evidence[1],
                "source": "dicom_header",
                "effect": "supports",
            }
        )
        return manifest

    def manifest_for_dicoms(self, dicoms: list[bytes], sop_uids: list[str]):
        manifest = archive_manifest_v2(dicoms[0])
        manifest["dicom_instance_count"] = len(dicoms)
        manifest["source"]["dicom_count"] = len(dicoms)
        manifest["instances"] = [
            {
                "path": f"dicom/{index:06d}.dcm",
                "size_bytes": len(dicom),
                "sha256": hashlib.sha256(dicom).hexdigest(),
                "sop_instance_uid": sop_uid,
            }
            for index, (dicom, sop_uid) in enumerate(
                zip(dicoms, sop_uids, strict=True), start=1
            )
        ]
        return manifest

    def test_extracts_and_verifies_every_instance(self) -> None:
        result = self.extract(archive_manifest(self.dicom))
        self.assertEqual(result.dicom_count, 1)
        self.assertEqual(result.value["source"]["manufacturer"], "SIEMENS")
        self.assertTrue(result.functional_epi_headers_confirmed)

    def test_v2_structural_mr_is_privacy_audited_without_functional_gate(self) -> None:
        self.dicom = make_structural_dicom(self.root / "source.dcm")
        manifest = archive_manifest_v2(self.dicom)

        result = self.extract(manifest)

        self.assertEqual(result.series_kind, "structural_t1w")
        self.assertEqual(result.processing_route, "archive-verify-v1")
        self.assertEqual(result.pixel_data_policy, "scanner-native-not-defaced")

    def test_series_provenance_allows_omission_but_rejects_conflicting_values(
        self,
    ) -> None:
        from pydicom import dcmread

        first_uid = "2.25.123456789012345678901234567890123456"
        second_uid = "2.25.123456789012345678901234567890123457"
        first = make_structural_dicom(self.root / "first.dcm", first_uid)

        def second_with(mutate) -> bytes:
            path = self.root / "second.dcm"
            make_structural_dicom(path, second_uid)
            dataset = dcmread(path)
            mutate(dataset)
            dataset.save_as(path, enforce_file_format=True)
            return path.read_bytes()

        def extract_pair(second: bytes, *, omit_scanner: bool = False):
            dicoms = [first, second]
            manifest = self.manifest_for_dicoms(dicoms, [first_uid, second_uid])
            if omit_scanner:
                for key in ("manufacturer", "model", "software_versions"):
                    manifest["source"].pop(key, None)
            return self.extract(manifest, dicoms=dicoms)

        def remove_optional_series_metadata(dataset) -> None:
            dataset.Manufacturer = ""
            for keyword in (
                "ManufacturerModelName",
                "SoftwareVersions",
                "PatientPosition",
                "MagneticFieldStrength",
                "SequenceName",
            ):
                if (element := dataset.data_element(keyword)) is not None:
                    del dataset[element.tag]
            for keyword in ("ScanOptions", "MRAcquisitionType", "SeriesNumber"):
                setattr(dataset, keyword, "")

        omitted = second_with(remove_optional_series_metadata)
        self.assertEqual(extract_pair(omitted, omit_scanner=True).dicom_count, 2)

        scanner_conflict = second_with(
            lambda dataset: setattr(dataset, "SoftwareVersions", "GE DV27")
        )
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_SCANNER_METADATA_MISMATCH"
        ):
            extract_pair(scanner_conflict)

        acquisition_conflict = second_with(
            lambda dataset: setattr(dataset, "MRAcquisitionType", "2D")
        )
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_ACQUISITION_METADATA_MISMATCH"
        ):
            extract_pair(acquisition_conflict)

        series_conflict = second_with(
            lambda dataset: setattr(
                dataset,
                "SeriesInstanceUID",
                "2.25.100000000000000000000000000000000099",
            )
        )
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_SERIES_METADATA_MISMATCH"):
            extract_pair(series_conflict)

    def test_accepts_complete_classic_root_public_diffusion_contract(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def diffusion(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "DIFFUSION"]
            dataset.add_new(Tag(0x0018, 0x9087), "FD", 1000.0)
            dataset.add_new(Tag(0x0018, 0x9075), "CS", "DIRECTIONAL")
            gradient = Dataset()
            gradient.add_new(Tag(0x0018, 0x9089), "FD", [1.0, 0.0, 0.0])
            dataset.add_new(Tag(0x0018, 0x9076), "SQ", Sequence([gradient]))

        self.rewrite_dicom(diffusion)

        self.assertEqual(
            self.extract(self.scientific_manifest("diffusion")).dicom_count, 1
        )

    def test_rejects_spoofed_classic_diffusion_evidence_without_vector(self) -> None:
        from pydicom.tag import Tag

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def incomplete(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "DIFFUSION"]
            dataset.add_new(Tag(0x0018, 0x9087), "FD", 1000.0)
            dataset.add_new(Tag(0x0018, 0x9075), "CS", "DIRECTIONAL")

        self.rewrite_dicom(incomplete)

        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED"
        ):
            self.extract(self.scientific_manifest("diffusion"))

    def test_incomplete_direct_diffusion_b_matrix_leaf_is_detected_and_rejected(
        self,
    ) -> None:
        from pydicom.tag import Tag

        self.dicom = make_structural_dicom(self.root / "source.dcm")
        self.rewrite_dicom(
            lambda dataset: dataset.add_new(Tag(0x0018, 0x9602), "FD", 1.0)
        )

        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED"
        ):
            self.extract(self.scientific_manifest("diffusion"))

    def test_enhanced_diffusion_requires_atomic_every_frame_contract(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag
        from pydicom.uid import EnhancedMRImageStorage

        def frame(*, derived: bool = False) -> Dataset:
            functional_group = Dataset()
            frame_type = Dataset()
            frame_type.add_new(
                Tag(0x0008, 0x9007),
                "CS",
                [
                    "DERIVED" if derived else "ORIGINAL",
                    "PRIMARY",
                    "DIFFUSION",
                    "ADC" if derived else "NONE",
                ],
            )
            functional_group.add_new(Tag(0x0018, 0x9226), "SQ", Sequence([frame_type]))
            if not derived:
                diffusion_item = Dataset()
                diffusion_item.add_new(Tag(0x0018, 0x9087), "FD", 1000.0)
                diffusion_item.add_new(Tag(0x0018, 0x9075), "CS", "DIRECTIONAL")
                gradient = Dataset()
                gradient.add_new(Tag(0x0018, 0x9089), "FD", [1.0, 0.0, 0.0])
                diffusion_item.add_new(Tag(0x0018, 0x9076), "SQ", Sequence([gradient]))
                functional_group.add_new(
                    Tag(0x0018, 0x9117), "SQ", Sequence([diffusion_item])
                )
            return functional_group

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def complete(dataset) -> None:
            dataset.SOPClassUID = EnhancedMRImageStorage
            dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "DIFFUSION", "NONE"]
            dataset.NumberOfFrames = "2"
            dataset.PerFrameFunctionalGroupsSequence = Sequence([frame(), frame()])

        self.rewrite_dicom(complete)
        manifest = self.scientific_manifest("diffusion")
        self.assertEqual(self.extract(manifest).dicom_count, 1)

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def partial(dataset) -> None:
            complete(dataset)
            dataset.PerFrameFunctionalGroupsSequence = Sequence([frame(), Dataset()])

        self.rewrite_dicom(partial)
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED"
        ):
            self.extract(self.scientific_manifest("diffusion"))

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def mixed(dataset) -> None:
            complete(dataset)
            dataset.ImageType = ["MIXED", "PRIMARY", "DIFFUSION", "MIXED"]
            dataset.PerFrameFunctionalGroupsSequence = Sequence(
                [frame(), frame(derived=True)]
            )

        self.rewrite_dicom(mixed)
        self.assertEqual(
            self.extract(self.scientific_manifest("diffusion")).dicom_count, 1
        )

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def mismatched_summary(dataset) -> None:
            mixed(dataset)
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "DIFFUSION", "NONE"]

        self.rewrite_dicom(mismatched_summary)
        with self.assertRaisesRegex(InvalidArchive, PRIVACY_ERROR):
            self.extract(self.scientific_manifest("diffusion"))

    def test_enhanced_asl_accepts_multiple_items_and_rejects_partial_frames(
        self,
    ) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag
        from pydicom.uid import EnhancedMRImageStorage

        def asl_item(context: str) -> Dataset:
            item = Dataset()
            item.add_new(Tag(0x0018, 0x9252), "LO", "")
            item.add_new(Tag(0x0018, 0x9257), "CS", context)
            item.add_new(Tag(0x0018, 0x9259), "CS", "NO")
            item.add_new(Tag(0x0018, 0x925C), "CS", "NO")
            slab = Dataset()
            slab.add_new(Tag(0x0018, 0x9253), "US", 1)
            slab.add_new(Tag(0x0018, 0x9254), "FD", 100.0)
            slab.add_new(Tag(0x0018, 0x9255), "FD", [1.0, 0.0, 0.0])
            slab.add_new(Tag(0x0018, 0x9256), "FD", [0.0, 0.0, 0.0])
            slab.add_new(Tag(0x0018, 0x9258), "UL", 1500)
            item.add_new(Tag(0x0018, 0x9260), "SQ", Sequence([slab]))
            return item

        def functional_group() -> Dataset:
            group = Dataset()
            group.add_new(
                Tag(0x0018, 0x9251),
                "SQ",
                Sequence([asl_item("LABEL"), asl_item("CONTROL")]),
            )
            return group

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def complete(dataset) -> None:
            dataset.SOPClassUID = EnhancedMRImageStorage
            dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "PERFUSION", "NONE"]
            dataset.add_new(Tag(0x0018, 0x9250), "CS", "PSEUDOCONTINUOUS")
            dataset.NumberOfFrames = "2"
            dataset.PerFrameFunctionalGroupsSequence = Sequence(
                [functional_group(), functional_group()]
            )

        self.rewrite_dicom(complete)
        manifest = self.scientific_manifest("asl_perfusion")
        manifest["deidentification"]["metadata_transformations"] = [
            "emptied_asl_technique_description"
        ]
        self.assertEqual(self.extract(manifest).dicom_count, 1)

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def partial(dataset) -> None:
            complete(dataset)
            dataset.PerFrameFunctionalGroupsSequence = Sequence(
                [functional_group(), Dataset()]
            )

        self.rewrite_dicom(partial)
        manifest = self.scientific_manifest("asl_perfusion")
        manifest["deidentification"]["metadata_transformations"] = [
            "emptied_asl_technique_description"
        ]
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED"
        ):
            self.extract(manifest)

    def test_classic_public_asl_macro_is_independently_verified(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def complete(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "ASL"]
            dataset.add_new(Tag(0x0018, 0x9250), "CS", "PSEUDOCONTINUOUS")
            item = Dataset()
            item.add_new(Tag(0x0018, 0x9252), "LO", "")
            item.add_new(Tag(0x0018, 0x9257), "CS", "M_ZERO_SCAN")
            item.add_new(Tag(0x0018, 0x9259), "CS", "NO")
            item.add_new(Tag(0x0018, 0x925C), "CS", "NO")
            dataset.add_new(Tag(0x0018, 0x9251), "SQ", Sequence([item]))

        self.rewrite_dicom(complete)
        manifest = self.scientific_manifest("asl_perfusion")
        manifest["deidentification"]["metadata_transformations"] = [
            "emptied_asl_technique_description"
        ]

        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_asl_metadata_transformations_attest_independent_observed_counts(
        self,
    ) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def positive_conditionals(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "ASL"]
            dataset.add_new(Tag(0x0018, 0x9250), "CS", "PSEUDOCONTINUOUS")
            item = Dataset()
            item.add_new(Tag(0x0018, 0x9252), "LO", "")
            item.add_new(Tag(0x0018, 0x9257), "CS", "M_ZERO_SCAN")
            item.add_new(Tag(0x0018, 0x9259), "CS", "YES")
            item.add_new(Tag(0x0018, 0x925A), "FD", 20.0)
            item.add_new(Tag(0x0018, 0x925B), "LO", "REDACTED")
            item.add_new(Tag(0x0018, 0x925C), "CS", "YES")
            timing = Dataset()
            timing.add_new(Tag(0x0018, 0x925E), "LO", "")
            timing.add_new(Tag(0x0018, 0x925F), "UL", 1800)
            item.add_new(Tag(0x0018, 0x925D), "SQ", Sequence([timing]))
            dataset.add_new(Tag(0x0018, 0x9251), "SQ", Sequence([item]))

        self.rewrite_dicom(positive_conditionals)
        manifest = self.scientific_manifest("asl_perfusion")
        transformations = [
            "emptied_asl_technique_description",
            "redacted_asl_crusher_description",
            "emptied_asl_bolus_cutoff_technique",
        ]
        manifest["deidentification"]["metadata_transformations"] = transformations
        self.assertEqual(self.extract(manifest).dicom_count, 1)

        for omitted in transformations:
            with self.subTest(omitted=omitted):
                incomplete = copy.deepcopy(manifest)
                incomplete["deidentification"]["metadata_transformations"] = [
                    item for item in transformations if item != omitted
                ]
                with self.assertRaisesRegex(
                    InvalidArchive, "ARCHIVE_DEIDENTIFICATION_UNVERIFIED"
                ):
                    self.extract(incomplete)

    def test_ge_asl_fields_are_validated_as_supplemental_metadata(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag

        def add_ge_fields(dataset, technique: str = "PSEUDOCONTINUOUS") -> None:
            dataset.add_new(Tag(0x0043, 0x0010), "LO", "GEMS_PARM_01")
            dataset.add_new(Tag(0x0043, 0x10A3), "CS", technique)
            dataset.add_new(Tag(0x0043, 0x10A5), "IS", "1800")

        def add_public_asl(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "ASL"]
            dataset.add_new(Tag(0x0018, 0x9250), "CS", "PSEUDOCONTINUOUS")
            item = Dataset()
            item.add_new(Tag(0x0018, 0x9252), "LO", "")
            item.add_new(Tag(0x0018, 0x9257), "CS", "M_ZERO_SCAN")
            item.add_new(Tag(0x0018, 0x9259), "CS", "NO")
            item.add_new(Tag(0x0018, 0x925C), "CS", "NO")
            dataset.add_new(Tag(0x0018, 0x9251), "SQ", Sequence([item]))
            add_ge_fields(dataset)

        self.dicom = make_structural_dicom(self.root / "source.dcm")
        self.rewrite_dicom(add_public_asl)
        manifest = self.scientific_manifest("asl_perfusion")
        manifest["source"]["image_type"] = ["ORIGINAL", "PRIMARY", "M", "ASL"]
        manifest["deidentification"]["safe_private_exceptions"] = [
            "ge_gems_parm_01_asl_technique_duration_v1"
        ]
        manifest["deidentification"]["metadata_transformations"] = [
            "emptied_asl_technique_description"
        ]
        self.assertEqual(self.extract(manifest).dicom_count, 1)

        self.dicom = make_structural_dicom(self.root / "source.dcm")
        self.rewrite_dicom(add_ge_fields)
        manifest = self.scientific_manifest("asl_perfusion")
        manifest["deidentification"]["safe_private_exceptions"] = [
            "ge_gems_parm_01_asl_technique_duration_v1"
        ]
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED"
        ):
            self.extract(manifest)

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def disagreeing_technique(dataset) -> None:
            add_public_asl(dataset)
            dataset[Tag(0x0043, 0x10A3)].value = "PULSED"

        self.rewrite_dicom(disagreeing_technique)
        manifest = self.scientific_manifest("asl_perfusion")
        manifest["source"]["image_type"] = ["ORIGINAL", "PRIMARY", "M", "ASL"]
        manifest["deidentification"]["safe_private_exceptions"] = [
            "ge_gems_parm_01_asl_technique_duration_v1"
        ]
        manifest["deidentification"]["metadata_transformations"] = [
            "emptied_asl_technique_description"
        ]
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED"
        ):
            self.extract(manifest)

    def test_ge_asl_context_cannot_be_routed_as_non_asl(self) -> None:
        from pydicom import dcmread
        from pydicom.tag import Tag

        def add_ge_asl_context(dataset) -> None:
            dataset.add_new(Tag(0x0043, 0x0010), "LO", "GEMS_PARM_01")
            dataset.add_new(Tag(0x0043, 0x10A3), "CS", "PSEUDOCONTINUOUS")
            dataset.add_new(Tag(0x0043, 0x10A5), "IS", "1800")

        def make_ge_functional(path: Path) -> bytes:
            make_functional_dicom_v2(path)
            dataset = dcmread(path)
            dataset.Manufacturer = "GE MEDICAL SYSTEMS"
            dataset.ManufacturerModelName = "Discovery MR750"
            dataset.SoftwareVersions = "GE DV26"
            dataset.ImageType = [
                value for value in dataset.ImageType if str(value) != "MOSAIC"
            ]
            for tag in (Tag(0x0029, 0x0010), Tag(0x0029, 0x1010)):
                if tag in dataset:
                    del dataset[tag]
            dataset.save_as(path, enforce_file_format=True)
            return path.read_bytes()

        cases = (
            (
                "functional_epi",
                "functional-epi-v1",
                "functional_epi_confirmed",
                make_ge_functional,
            ),
            (
                "other_mr",
                "archive-verify-v1",
                "supported_mr_image",
                make_structural_dicom,
            ),
        )
        for kind, route, evidence, factory in cases:
            with self.subTest(series_kind=kind):
                self.dicom = factory(self.root / "source.dcm")
                self.rewrite_dicom(add_ge_asl_context)
                dataset = dcmread(io.BytesIO(self.dicom), stop_before_pixels=True)
                manifest = archive_manifest_v2(
                    self.dicom,
                    series_kind=kind,
                    processing_route=route,
                    evidence_code=evidence,
                    source={
                        "dicom_count": 1,
                        "manufacturer": str(dataset.Manufacturer),
                        "model": str(dataset.ManufacturerModelName),
                        "software_versions": [str(dataset.SoftwareVersions)],
                        "scanning_sequence": [str(dataset.ScanningSequence)],
                        "sequence_variant": [str(dataset.SequenceVariant)],
                        "scan_options": [str(dataset.ScanOptions)],
                        "sequence_name": str(dataset.SequenceName),
                        "mr_acquisition_type": str(dataset.MRAcquisitionType),
                        "image_type": [str(value) for value in dataset.ImageType],
                        "series_number": int(dataset.SeriesNumber),
                    },
                    safe_private_exceptions=[
                        "ge_gems_parm_01_asl_technique_duration_v1",
                    ],
                )
                with self.assertRaisesRegex(
                    InvalidArchive, "ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED"
                ):
                    self.extract(manifest)

    def test_derived_diffusion_product_archives_without_acquired_gradient_claim(
        self,
    ) -> None:
        self.dicom = make_structural_dicom(self.root / "source.dcm")
        self.rewrite_dicom(
            lambda dataset: setattr(
                dataset, "ImageType", ["DERIVED", "SECONDARY", "ADC", "DIFFUSION"]
            )
        )
        manifest = archive_manifest_v2(
            self.dicom,
            series_kind="derived_mr",
            evidence_code="derived_or_secondary",
        )

        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_accepts_positional_enhanced_mr_and_frame_geometry_contract(self) -> None:
        from pydicom import dcmread
        from pydicom.tag import Tag
        from pydicom.uid import EnhancedMRImageStorage

        path = self.root / "source.dcm"
        make_structural_dicom(path)
        dataset = dcmread(path)
        dataset.SOPClassUID = EnhancedMRImageStorage
        dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage
        dataset.ImageType = ["ORIGINAL", "PRIMARY", "VOLUME", "NONE"]
        dataset.add_new(Tag(0x0008, 0x9007), "CS", dataset.ImageType)
        dataset.add_new(Tag(0x0020, 0x9056), "SH", "0123456789abcdef")
        dataset.add_new(Tag(0x0020, 0x9057), "UL", 1)
        dataset.add_new(Tag(0x0020, 0x9153), "FD", 0.0)
        dataset.add_new(Tag(0x0020, 0x9157), "UL", [1, 2])
        dataset.add_new(Tag(0x0020, 0x9165), "AT", Tag(0x0020, 0x9057))
        dataset.add_new(Tag(0x0020, 0x9167), "AT", Tag(0x0020, 0x9111))
        conform_enhanced_mr(dataset)
        dataset.save_as(path, enforce_file_format=True)
        self.dicom = path.read_bytes()

        result = self.extract(archive_manifest_v2(self.dicom))

        self.assertEqual(result.dicom_count, 1)

    def test_enhanced_and_legacy_image_type_terms_match_client_contract(self) -> None:
        from pydicom.uid import EnhancedMRImageStorage

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def enhanced_adc(dataset) -> None:
            dataset.SOPClassUID = EnhancedMRImageStorage
            dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage
            dataset.ImageType = ["DERIVED", "PRIMARY", "DIFFUSION", "ADC"]

        self.rewrite_dicom(enhanced_adc)
        manifest = archive_manifest_v2(
            self.dicom,
            series_kind="derived_mr",
            evidence_code="derived_or_secondary",
        )
        self.assertEqual(self.extract(manifest).dicom_count, 1)

        legacy_uid = "1.2.840.10008.5.1.4.1.1.4.4"
        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def legacy_empty_value_four(dataset) -> None:
            dataset.SOPClassUID = legacy_uid
            dataset.file_meta.MediaStorageSOPClassUID = legacy_uid
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "VOLUME", ""]

        self.rewrite_dicom(legacy_empty_value_four)
        self.assertEqual(self.extract(archive_manifest_v2(self.dicom)).dicom_count, 1)

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def modern_empty_value_four(dataset) -> None:
            legacy_empty_value_four(dataset)
            dataset.SOPClassUID = EnhancedMRImageStorage
            dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage

        self.rewrite_dicom(modern_empty_value_four)
        self.assert_privacy_rejected()

    def test_classic_image_type_preserves_empty_optional_positions(self) -> None:
        self.dicom = make_structural_dicom(self.root / "source.dcm")
        self.rewrite_dicom(
            lambda dataset: setattr(
                dataset, "ImageType", ["ORIGINAL", "PRIMARY", "", "T1"]
            )
        )

        self.assertEqual(
            self.extract(archive_manifest_v2(self.dicom)).dicom_count,
            1,
        )

    def test_rejects_reordered_enhanced_frame_type_semantics(self) -> None:
        from pydicom import dcmread
        from pydicom.tag import Tag
        from pydicom.uid import EnhancedMRImageStorage

        path = self.root / "source.dcm"
        make_structural_dicom(path)
        dataset = dcmread(path)
        dataset.SOPClassUID = EnhancedMRImageStorage
        dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage
        dataset.ImageType = ["PRIMARY", "ORIGINAL", "VOLUME", "NONE"]
        dataset.add_new(
            Tag(0x0008, 0x9007),
            "CS",
            ["ORIGINAL", "PRIMARY", "NONE", "VOLUME"],
        )
        conform_enhanced_mr(dataset)
        dataset.save_as(path, enforce_file_format=True)
        self.dicom = path.read_bytes()

        self.assert_privacy_rejected()

    def test_enhanced_root_mixed_summary_does_not_allow_frame_type_mixed(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag
        from pydicom.uid import EnhancedMRImageStorage

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def frame_type(dataset, value_four: str) -> None:
            dataset.SOPClassUID = EnhancedMRImageStorage
            dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage
            dataset.ImageType = ["MIXED", "PRIMARY", "VOLUME", "MIXED"]
            dataset.NumberOfFrames = "2"
            original = Dataset()
            original.add_new(
                Tag(0x0008, 0x9007),
                "CS",
                ["ORIGINAL", "PRIMARY", "VOLUME", "NONE"],
            )
            derived = Dataset()
            derived.add_new(
                Tag(0x0008, 0x9007),
                "CS",
                ["DERIVED", "PRIMARY", "VOLUME", value_four],
            )
            dataset.PerFrameFunctionalGroupsSequence = Sequence([original, derived])

        self.rewrite_dicom(lambda dataset: frame_type(dataset, "NONE"))
        self.assertEqual(self.extract(archive_manifest_v2(self.dicom)).dicom_count, 1)

        self.dicom = make_structural_dicom(self.root / "source.dcm")
        self.rewrite_dicom(lambda dataset: frame_type(dataset, "MIXED"))
        self.assert_privacy_rejected()

    def test_rejects_non_pseudonymous_enhanced_stack_id(self) -> None:
        from pydicom import dcmread
        from pydicom.tag import Tag

        path = self.root / "source.dcm"
        make_structural_dicom(path)
        dataset = dcmread(path)
        dataset.add_new(Tag(0x0020, 0x9056), "SH", "ORIGINAL_STACK")
        dataset.save_as(path, enforce_file_format=True)
        self.dicom = path.read_bytes()

        self.assert_privacy_rejected()

    def test_v2_manifest_route_mismatch_fails_closed(self) -> None:
        self.dicom = make_structural_dicom(self.root / "source.dcm")
        manifest = archive_manifest_v2(self.dicom)

        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_PROCESSING_ROUTE_MISMATCH"
        ):
            self.extract(
                manifest,
                expected_series_kind="functional_epi",
                expected_processing_route="functional-epi-v1",
            )

    def test_v2_manifest_requires_explicit_not_defaced_disclosure(self) -> None:
        self.dicom = make_structural_dicom(self.root / "source.dcm")
        manifest = archive_manifest_v2(self.dicom)
        del manifest["deidentification"]["recognizable_visual_features"]

        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_MANIFEST_SCHEMA"):
            self.extract(manifest)

    def test_scanner_identity_policy_is_vendor_neutral(self) -> None:
        self.assertTrue(safe_scanner_text("uMR Ultra 2027"))
        self.assertTrue(safe_scanner_text("UNITEDIMAGING"))
        self.assertTrue(safe_scanner_text("Filename Recon 2"))
        self.assertFalse(safe_scanner_text("Participant Scanner"))
        self.assertFalse(safe_scanner_text("Scanner 1234567"))
        self.assertFalse(safe_scanner_text("https://scanner.invalid"))

    def test_manifest_accepts_only_fixed_generic_coil_names(self) -> None:
        manifest = archive_manifest_v2(self.dicom)
        manifest["source"]["receive_coil_name"] = "MULTI_COIL"
        manifest["source"]["transmit_coil_name"] = "SURFACE"

        def validate(value):
            return validate_manifest(
                json.dumps(value, separators=(",", ":")).encode(),
                expected_series_archive_id=value["series_archive_id"],
                expected_series_id=value["series_id"],
                expected_dicom_count=value["dicom_instance_count"],
                expected_series_kind=value["series_kind"],
                expected_processing_route=value["processing_route"],
                expected_pixel_data_policy=value["pixel_data_policy"],
            )

        self.assertEqual(validate(manifest).dicom_count, 1)

        for key, value in (
            ("receive_coil_name", "MULTI COIL"),
            ("receive_coil_name", "SITE_MULTI_COIL"),
            ("transmit_coil_name", "S"),
            ("transmit_coil_name", "SITE_SURFACE"),
        ):
            with self.subTest(key=key, value=value):
                invalid = archive_manifest_v2(self.dicom)
                invalid["source"][key] = value
                with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_MANIFEST_SCHEMA"):
                    validate(invalid)

    def test_accepts_scanner_neutral_release_routes(self) -> None:
        self.assertEqual(self.extract(archive_manifest(self.dicom)).dicom_count, 1)

        self.dicom = make_dicom(self.root / "source.dcm")
        self.rewrite_philips_dicom()
        self.assertEqual(self.extract(self.philips_manifest()).dicom_count, 1)

    def test_requires_sanitized_csa_for_any_classic_mosaic(self) -> None:
        def remove_mosaic(dataset) -> None:
            dataset.ImageType = [
                value for value in dataset.ImageType if str(value) != "MOSAIC"
            ]

        self.rewrite_dicom(remove_mosaic)
        manifest = archive_manifest(self.dicom)
        manifest["source"]["image_type"].remove("MOSAIC")
        self.assertEqual(self.extract(manifest).dicom_count, 1)

        self.dicom = make_dicom(self.root / "source.dcm")

        def remove_manufacturer(dataset) -> None:
            dataset.Manufacturer = ""

        self.rewrite_dicom(remove_manufacturer)
        manifest = archive_manifest(self.dicom)
        manifest["source"].pop("manufacturer")
        self.assertEqual(self.extract(manifest).dicom_count, 1)

        self.dicom = make_dicom(self.root / "source.dcm")

        def remove_csa(dataset) -> None:
            dataset.Manufacturer = ""
            del dataset[0x00290010]
            del dataset[0x00291010]

        self.rewrite_dicom(remove_csa)
        manifest = archive_manifest(self.dicom)
        manifest["source"].pop("manufacturer")
        manifest["deidentification"].pop("safe_private_exceptions")
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_MOSAIC_CSA_REQUIRED"):
            self.extract(manifest)

    def test_requires_uih_grid_geometry_without_manufacturer_identity(self) -> None:
        from pydicom.tag import Tag

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def with_grid_geometry(dataset) -> None:
            dataset.Manufacturer = ""
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "GRID"]
            dataset.add_new(Tag(0x0065, 0x0010), "LO", "Image Private Header")
            dataset.add_new(Tag(0x0065, 0x1050), "DS", "32")

        self.rewrite_dicom(with_grid_geometry)
        manifest = archive_manifest_v2(
            self.dicom,
            series_kind="other_mr",
            evidence_code="supported_mr_image",
            safe_private_exceptions=[
                "uih_image_private_header_grid_slice_count_numeric_v1"
            ],
        )
        manifest["source"].pop("manufacturer")
        manifest["source"]["image_type"] = ["ORIGINAL", "PRIMARY", "M", "GRID"]
        self.assertEqual(self.extract(manifest).dicom_count, 1)

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def without_grid_geometry(dataset) -> None:
            dataset.Manufacturer = ""
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "GRID"]

        self.rewrite_dicom(without_grid_geometry)
        manifest = archive_manifest_v2(
            self.dicom,
            series_kind="other_mr",
            evidence_code="supported_mr_image",
        )
        manifest["source"].pop("manufacturer")
        manifest["source"]["image_type"] = ["ORIGINAL", "PRIMARY", "M", "GRID"]
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_GRID_SLICE_COUNT_REQUIRED"
        ):
            self.extract(manifest)

    def test_philips_scaling_pair_is_atomic_while_other_fields_are_optional(
        self,
    ) -> None:
        private_tags = {
            "scale_intercept": 0x2005100D,
            "scale_slope": 0x2005100E,
            "number_of_slices": 0x20011018,
            "water_fat_shift": 0x20011022,
        }
        self.assertEqual(set(private_tags), set(PHILIPS_REQUIRED_PRIVATE_FIELDS))
        for field_name, tag in private_tags.items():
            with self.subTest(field_name=field_name):
                self.dicom = make_dicom(self.root / "source.dcm")
                self.rewrite_philips_dicom(lambda dataset, tag=tag: dataset.pop(tag))
                manifest = self.philips_manifest()
                if field_name == "number_of_slices":
                    manifest["deidentification"]["safe_private_exceptions"].remove(
                        "dicom_ps3.15_philips_number_of_slices"
                    )
                elif field_name == "water_fat_shift":
                    manifest["deidentification"]["safe_private_exceptions"].remove(
                        "dicom_ps3.15_philips_water_fat_shift"
                    )
                if field_name in {"scale_intercept", "scale_slope"}:
                    self.assert_privacy_rejected()
                else:
                    self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_rejects_invalid_philips_private_scientific_ranges(self) -> None:
        invalid_values = {
            0x2005100D: 1.0e10,
            0x2005100E: 0.0,
            0x20011018: 0,
            0x20011022: -1.0,
        }
        for tag, invalid_value in invalid_values.items():
            with self.subTest(tag=hex(tag)):
                self.dicom = make_dicom(self.root / "source.dcm")

                def invalidate(dataset, tag=tag, value=invalid_value) -> None:
                    dataset[tag].value = value

                self.rewrite_philips_dicom(invalidate)
                with self.assertRaisesRegex(
                    InvalidArchive, "DICOM_PRIVACY_AUDIT_FAILED"
                ):
                    self.extract(self.philips_manifest())

    def test_rejects_duplicate_philips_semantic_private_field(self) -> None:
        from pydicom.tag import Tag

        def duplicate_intercept(dataset) -> None:
            dataset.add_new(Tag(0x2005, 0x0011), "LO", "Philips MR Imaging DD 001")
            dataset.add_new(Tag(0x2005, 0x110D), "FL", 0.0)

        self.rewrite_philips_dicom(duplicate_intercept)
        with self.assertRaisesRegex(InvalidArchive, "DICOM_PRIVACY_AUDIT_FAILED"):
            self.extract(self.philips_manifest())

    def test_accepts_philips_private_creator_block_variation(self) -> None:
        from pydicom.tag import Tag

        def vary_creator_blocks(dataset) -> None:
            del dataset[0x20050010]
            del dataset[0x2005100D]
            del dataset[0x2005100E]
            del dataset[0x20010010]
            del dataset[0x20011018]
            del dataset[0x20011022]
            dataset.add_new(Tag(0x2005, 0x0011), "LO", "Philips MR Imaging DD 001")
            dataset.add_new(Tag(0x2005, 0x110D), "FL", 0.0)
            dataset.add_new(Tag(0x2005, 0x110E), "FL", 0.00363177)
            dataset.add_new(Tag(0x2001, 0x0012), "LO", "Philips Imaging DD 001")
            dataset.add_new(Tag(0x2001, 0x1218), "SL", 32)
            dataset.add_new(Tag(0x2001, 0x1222), "FL", 0.75)

        self.rewrite_philips_dicom(vary_creator_blocks)
        self.assertEqual(self.extract(self.philips_manifest()).dicom_count, 1)

    def test_spoofed_functional_manifest_cannot_admit_structural_mr(self) -> None:
        def mutate(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "MOSAIC"]
            dataset.ScanningSequence = "GR"
            del dataset.SequenceName

        self.assert_functional_purpose_downgraded(mutate)

    def test_accepts_exact_measured_siemens_epfid_without_bold_label(self) -> None:
        def mutate(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "ND", "MOSAIC"]
            dataset.ScanningSequence = "EP"
            dataset.SequenceName = "epfid"

        self.rewrite_dicom(mutate)
        self.assertEqual(self.extract(archive_manifest(self.dicom)).dicom_count, 1)

    def test_zero_diffusion_b_value_does_not_alone_make_epi_diffusion(self) -> None:
        def mutate(dataset) -> None:
            dataset.DiffusionBValue = 0.0

        self.rewrite_dicom(mutate)
        self.assertEqual(self.extract(archive_manifest(self.dicom)).dicom_count, 1)

    def test_rejects_diffusion_headers_despite_functional_manifest(self) -> None:
        def mutate(dataset) -> None:
            dataset.ImageType = [
                "ORIGINAL",
                "PRIMARY",
                "M",
                "EPI",
                "MOSAIC",
            ]
            dataset.SequenceName = "ep2d"
            dataset.DiffusionBValue = 1000.0

        self.assert_functional_purpose_downgraded(mutate)

    def test_rejects_perfusion_headers_despite_functional_manifest(self) -> None:
        def mutate(dataset) -> None:
            dataset.AcquisitionContrast = "PERFUSION"

        self.assert_functional_purpose_downgraded(mutate)

    def test_rejects_asl_like_perfusion_headers(self) -> None:
        def mutate(dataset) -> None:
            dataset.AcquisitionContrast = "PERFUSION"
            dataset.SequenceName = "ep2d"
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "EPI", "MOSAIC"]

        self.assert_functional_purpose_downgraded(mutate)

    def test_rejects_derived_headers_despite_functional_manifest(self) -> None:
        def mutate(dataset) -> None:
            dataset.ImageType = [
                "DERIVED",
                "PRIMARY",
                "M",
                "EPI",
                "BOLD",
                "MOSAIC",
            ]

        self.assert_functional_purpose_downgraded(mutate)

    def test_rejects_non_epi_and_missing_temporal_structure(self) -> None:
        def non_epi(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "BOLD", "MOSAIC"]
            dataset.ScanningSequence = "GR"
            del dataset.SequenceName

        self.assert_functional_purpose_downgraded(non_epi)

        self.dicom = make_dicom(self.root / "source.dcm")

        def no_temporal_structure(dataset) -> None:
            del dataset.NumberOfTemporalPositions

        self.assert_functional_purpose_downgraded(no_temporal_structure)

    def test_rejects_localizer_like_non_epi_headers(self) -> None:
        def localizer(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "MOSAIC"]
            dataset.ScanningSequence = "GR"
            del dataset.SequenceName

        self.assert_functional_purpose_downgraded(localizer)

    def test_functional_label_alone_does_not_substitute_for_epi_evidence(self) -> None:
        def bold_label_without_epi(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "BOLD", "MOSAIC"]
            dataset.ScanningSequence = "GR"
            dataset.SequenceName = "bold"

        self.assert_functional_purpose_downgraded(bold_label_without_epi)

    def test_accepts_enhanced_mr(self) -> None:
        from pydicom.uid import EnhancedMRImageStorage

        def enhanced(dataset) -> None:
            dataset.SOPClassUID = EnhancedMRImageStorage
            dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "FMRI", "NONE"]

        self.rewrite_dicom(enhanced)
        self.assertEqual(self.extract(archive_manifest(self.dicom)).dicom_count, 1)

    def test_rejects_enhanced_color_mr_without_safe_icc_contract(self) -> None:
        from pydicom.uid import EnhancedMRColorImageStorage

        def enhanced_color(dataset) -> None:
            dataset.SOPClassUID = EnhancedMRColorImageStorage
            dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRColorImageStorage
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "FMRI", "NONE"]

        self.rewrite_dicom(enhanced_color)
        self.assert_privacy_rejected()

    def test_accepts_enhanced_mr_timing_from_functional_groups(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.uid import EnhancedMRImageStorage

        def enhanced(dataset) -> None:
            dataset.SOPClassUID = EnhancedMRImageStorage
            dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "FMRI", "NONE"]
            del dataset.RepetitionTime
            del dataset.EchoTime
            timing = Dataset()
            timing.RepetitionTime = "800"
            echo = Dataset()
            echo.EffectiveEchoTime = 30.0
            second_echo = Dataset()
            second_echo.EffectiveEchoTime = 12.0
            shared = Dataset()
            shared.MRTimingAndRelatedParametersSequence = Sequence([timing])
            shared.MREchoSequence = Sequence([echo, second_echo])
            dataset.SharedFunctionalGroupsSequence = Sequence([shared])

        self.rewrite_dicom(enhanced)
        self.assertEqual(self.extract(archive_manifest(self.dicom)).dicom_count, 1)

    def test_accepts_two_position_short_functional_epi(self) -> None:
        def short_run(dataset) -> None:
            dataset.NumberOfTemporalPositions = 2

        self.rewrite_dicom(short_run)
        self.assertEqual(self.extract(archive_manifest(self.dicom)).dicom_count, 1)

    def test_accepts_missing_scanner_provenance(self) -> None:
        def remove_scanner_provenance(dataset) -> None:
            dataset.Manufacturer = ""
            del dataset.ManufacturerModelName
            del dataset.SoftwareVersions

        self.rewrite_dicom(remove_scanner_provenance)
        manifest = archive_manifest(self.dicom)
        manifest["source"].pop("manufacturer")
        manifest["source"].pop("model")
        manifest["source"].pop("software_versions")
        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_accepts_ge_standard_dicom_without_private_metadata(self) -> None:
        from pydicom.tag import Tag

        def ge(dataset) -> None:
            dataset.Manufacturer = "GE MEDICAL SYSTEMS"
            dataset.ManufacturerModelName = "Discovery MR750"
            dataset.SoftwareVersions = "GE DV26.0"
            dataset.ImageType = [
                value for value in dataset.ImageType if str(value) != "MOSAIC"
            ]
            for tag in (Tag(0x0029, 0x0010), Tag(0x0029, 0x1010)):
                if tag in dataset:
                    del dataset[tag]

        self.rewrite_dicom(ge)
        manifest = archive_manifest(self.dicom)
        manifest["source"]["manufacturer"] = "GE MEDICAL SYSTEMS"
        manifest["source"]["model"] = "Discovery MR750"
        manifest["source"]["software_versions"] = ["GE DV26.0"]
        manifest["source"]["image_type"].remove("MOSAIC")
        manifest["deidentification"].pop("safe_private_exceptions")
        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_rejects_manifest_and_dicom_manufacturer_mismatch(self) -> None:
        manifest = archive_manifest(self.dicom)
        manifest["source"]["manufacturer"] = "Philips Medical Systems"
        manifest["source"]["model"] = "Achieva dStream"
        manifest["source"]["software_versions"] = ["Philips 5.1.1"]
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_MANUFACTURER_MISMATCH"):
            self.extract(manifest)

    def test_accepts_unmeasured_scanner_model_or_software(self) -> None:
        def mutate(dataset) -> None:
            dataset.ManufacturerModelName = "MAGNETOM Prisma"

        self.rewrite_dicom(mutate)
        manifest = archive_manifest(self.dicom)
        manifest["source"]["model"] = "MAGNETOM Prisma"
        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_production_zstd_runs_in_tokenless_readonly_container(self) -> None:
        archive = self.root / "input.tar.zst"
        archive.write_bytes(b"test")
        config = Config(
            api_url="https://scalingneuro.com",
            token="controller-secret-not-for-zstd",
            work_root=self.root / "controller-work",
            processor_id="zstd-sandbox-test",
            native_tools_slurm_image=Path("/release/native-tools.sqsh"),
            slurm_job_id="12345",
            enroot_runtime_root=self.root / "enroot",
            disk_reserve_bytes=TEST_DISK_RESERVE_BYTES,
        )
        command = zstd_decompression_command(config, archive)
        self.assertEqual(command[0], "/opt/slurm/bin/srun")
        self.assertIn("--container-image=/release/native-tools.sqsh", command)
        self.assertIn("--container-readonly", command)
        self.assertIn("--no-container-mount-home", command)
        self.assertIn("--no-container-remap-root", command)
        self.assertIn("--no-container-entrypoint", command)
        self.assertIn(
            f"--container-mounts={archive.resolve()}:/input/archive.zst:ro+rprivate",
            command,
        )
        self.assertIn(NATIVE_ZSTD, command[-1])
        self.assertIn('if [ "$status" -eq 1 ]', command[-1])
        self.assertNotIn(config.token, command)
        self.assertNotIn(str(config.work_root), command)

        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_ZSTD_INVALID"):
            _check_zstd_returncode(config, SANDBOX_ZSTD_INVALID_EXIT)
        for returncode in (1, 126, 127, 139):
            with self.subTest(returncode=returncode):
                with self.assertRaisesRegex(
                    ConverterFailure, "ZSTD_SANDBOX_FAILED"
                ) as error:
                    _check_zstd_returncode(config, returncode)
                self.assertTrue(error.exception.retryable)

    def sandbox_config(self) -> Config:
        return Config(
            api_url="https://scalingneuro.com",
            token="controller-secret-not-for-zstd",
            work_root=self.root / "controller-work",
            processor_id="zstd-sandbox-test",
            native_tools_slurm_image=Path("/release/native-tools.sqsh"),
            slurm_job_id="12345",
            enroot_runtime_root=self.root / "enroot",
            disk_reserve_bytes=TEST_DISK_RESERVE_BYTES,
        )

    def sandbox_zstd_failure(self, returncode: int, destination: str) -> None:
        archive = self.root / "input.tar.zst"
        archive.write_bytes(b"test" * 8)
        process = Mock()
        process.stdout = io.BytesIO()
        process.wait.return_value = returncode
        with patch(
            "scaling_neuro_processor.archive.subprocess.Popen",
            return_value=process,
        ):
            extract_archive(
                self.sandbox_config(),
                archive,
                self.root / destination,
                expected_series_archive_id=ARCHIVE_ID,
                expected_series_id="b" * 24,
                expected_dicom_count=1,
            )

    def test_sandbox_zstd_malformed_input_is_terminal(self) -> None:
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_ZSTD_INVALID"):
            self.sandbox_zstd_failure(SANDBOX_ZSTD_INVALID_EXIT, "malformed-zstd")

    def test_sandbox_zstd_runtime_failure_is_retryable(self) -> None:
        with self.assertRaisesRegex(ConverterFailure, "ZSTD_SANDBOX_FAILED") as error:
            self.sandbox_zstd_failure(127, "failed-zstd-runtime")
        self.assertTrue(error.exception.retryable)

    def test_rejects_noncanonical_tar_identity_header(self) -> None:
        def add_uname(header: bytearray) -> None:
            header[265 : 265 + len(b"PATIENT-NAME")] = b"PATIENT-NAME"

        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_TAR_HEADER_INVALID"):
            self.extract(
                archive_manifest(self.dicom),
                header_mutator=add_uname,
            )

    def test_rejects_nonzero_member_padding(self) -> None:
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_TAR_PADDING_INVALID"):
            self.extract(archive_manifest(self.dicom), padding_byte=0x41)

    def test_rejects_instance_hash_mismatch(self) -> None:
        manifest = archive_manifest(self.dicom)
        manifest["instances"][0]["sha256"] = "0" * 64
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_INSTANCE_MISMATCH"):
            self.extract(manifest)

    def test_rejects_sop_uid_mismatch(self) -> None:
        manifest = archive_manifest(self.dicom)
        manifest["instances"][0]["sop_instance_uid"] = "2.25.999"
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_SOP_UID_MISMATCH"):
            self.extract(manifest)

    def test_rejects_path_traversal(self) -> None:
        member = tarfile.TarInfo("../escape.dcm")
        member.size = 1
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_INSTANCE_ORDER"):
            self.extract(archive_manifest(self.dicom), extra=member)

    def test_rejects_symlink(self) -> None:
        member = tarfile.TarInfo("dicom/000002.dcm")
        member.type = tarfile.SYMTYPE
        member.linkname = "/etc/passwd"
        member.size = 0
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_TAR_HEADER_INVALID"):
            self.extract(archive_manifest(self.dicom), extra=member)

    def test_rejects_free_text_in_source_manifest(self) -> None:
        manifest = archive_manifest(self.dicom)
        manifest["source"]["manufacturer"] = "Research Participant Lab"
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_MANIFEST_SCHEMA"):
            self.extract(manifest)

    def test_rejects_free_text_classification_evidence(self) -> None:
        manifest = archive_manifest(self.dicom)
        manifest["classification"]["evidence"][0]["code"] = "participant_20260718"
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_MANIFEST_SCHEMA"):
            self.extract(manifest)

    def test_rejects_unverified_deidentification(self) -> None:
        manifest = copy.deepcopy(archive_manifest(self.dicom))
        manifest["deidentification"]["unknown_private_removed"] = False
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_DEIDENTIFICATION_UNVERIFIED"
        ):
            self.extract(manifest)

    def test_rejects_identity_leak_in_nested_sequence(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            nested = Dataset()
            nested.PatientName = "RESEARCH PARTICIPANT"
            dataset.add_new(Tag(0x0008, 0x1140), "SQ", Sequence([nested]))

        self.rewrite_dicom(mutate)
        self.assert_privacy_rejected()

    def test_accepts_recursively_audited_reference_sequence(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.uid import MRImageStorage

        def mutate(dataset) -> None:
            nested = Dataset()
            nested.ReferencedSOPClassUID = MRImageStorage
            nested.ReferencedSOPInstanceUID = (
                "2.25.223456789012345678901234567890123456"
            )
            dataset.ReferencedImageSequence = Sequence([nested])

        self.rewrite_dicom(mutate)
        self.assertEqual(self.extract(archive_manifest(self.dicom)).dicom_count, 1)

    def test_rejects_private_text_even_under_known_creator(self) -> None:
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            dataset.add_new(Tag(0x0019, 0x0010), "LO", "SIEMENS CSA HEADER")
            dataset.add_new(Tag(0x0019, 0x100A), "LO", "RESEARCH PARTICIPANT")

        self.rewrite_dicom(mutate)
        self.assert_privacy_rejected()

    def test_rejects_private_numeric_identifier_under_known_creator(self) -> None:
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            dataset.add_new(Tag(0x0029, 0x0010), "LO", "SIEMENS CSA HEADER")
            dataset.add_new(Tag(0x0029, 0x1011), "DS", "20260718")

        self.rewrite_dicom(mutate)
        self.assert_privacy_rejected()

    def test_accepts_exact_ps315_vendor_diffusion_private_attributes(self) -> None:
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            dataset.DeidentificationMethod = (
                "Scaling Neuro scaling-neuro.dicom-deidentification 2.0.0"
            )
            dataset.add_new(Tag(0x0019, 0x0010), "LO", "SIEMENS MR HEADER")
            dataset.add_new(Tag(0x0019, 0x100C), "IS", "1200")
            dataset.add_new(Tag(0x0019, 0x100D), "CS", "DIRECTIONAL")
            dataset.add_new(Tag(0x0019, 0x100E), "FD", [0.0, 1.0, -0.25])
            dataset.add_new(
                Tag(0x0019, 0x1027),
                "FD",
                [0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            )

        self.rewrite_dicom(mutate)
        legacy = archive_manifest(self.dicom)
        manifest = archive_manifest_v2(
            self.dicom,
            series_kind="diffusion",
            processing_route="archive-verify-v1",
            evidence_code="diffusion_detected",
            source=legacy["source"],
            safe_private_exceptions=[
                "siemens_csa_image_header_numeric_v1",
                "dicom_ps3.15_siemens_mr_header_diffusion",
            ],
        )
        manifest["classification"]["evidence"].append(
            {
                "code": "diffusion_scientific_metadata_contract_verified",
                "source": "dicom_header",
                "effect": "supports",
            }
        )
        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_accepts_exact_ps315_philips_diffusion_private_attributes(self) -> None:
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            dataset.DeidentificationMethod = (
                "Scaling Neuro scaling-neuro.dicom-deidentification 2.0.0"
            )
            dataset.add_new(Tag(0x2001, 0x1003), "FL", 1200.0)
            dataset.add_new(Tag(0x2001, 0x1004), "CS", "AP")
            dataset.add_new(Tag(0x2001, 0x1008), "IS", "2")
            dataset.add_new(Tag(0x2005, 0x10B0), "FL", 1.0)
            dataset.add_new(Tag(0x2005, 0x10B1), "FL", 0.0)
            dataset.add_new(Tag(0x2005, 0x10B2), "FL", 0.0)

        self.rewrite_philips_dicom(mutate)
        legacy = self.philips_manifest()
        manifest = archive_manifest_v2(
            self.dicom,
            series_kind="diffusion",
            processing_route="archive-verify-v1",
            evidence_code="diffusion_detected",
            source=legacy["source"],
            safe_private_exceptions=[
                "dicom_ps3.15_philips_diffusion",
                "dicom_ps3.15_philips_phase_number",
                "philips_mr_imaging_dd_001_diffusion_gradient_vector_numeric_v1",
                "dicom_ps3.15_philips_scale_intercept_slope",
                "dicom_ps3.15_philips_number_of_slices",
                "dicom_ps3.15_philips_water_fat_shift",
            ],
        )
        manifest["classification"]["evidence"].append(
            {
                "code": "diffusion_scientific_metadata_contract_verified",
                "source": "dicom_header",
                "effect": "supports",
            }
        )
        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_philips_dd005_indices_are_supplemental_not_a_diffusion_source(
        self,
    ) -> None:
        from pydicom.tag import Tag

        def add_indices(dataset) -> None:
            dataset.DeidentificationMethod = (
                "Scaling Neuro scaling-neuro.dicom-deidentification 2.0.0"
            )
            dataset.add_new(Tag(0x2005, 0x0014), "LO", "Philips MR Imaging DD 005")
            dataset.add_new(Tag(0x2005, 0x1412), "IS", "3")
            dataset.add_new(Tag(0x2005, 0x1413), "IS", "7")

        def add_public_diffusion(dataset) -> None:
            add_indices(dataset)
            dataset.add_new(Tag(0x0018, 0x9087), "FD", 1000.0)
            dataset.add_new(Tag(0x0018, 0x9075), "CS", "DIRECTIONAL")
            dataset.add_new(Tag(0x0018, 0x9089), "FD", [1.0, 0.0, 0.0])

        self.rewrite_philips_dicom(add_public_diffusion)
        philips_source = self.philips_manifest()["source"]
        manifest = self.scientific_manifest("diffusion") | {"source": philips_source}
        manifest["deidentification"]["safe_private_exceptions"] = [
            "philips_mr_imaging_dd_005_diffusion_indices_numeric_v1",
            "dicom_ps3.15_philips_scale_intercept_slope",
            "dicom_ps3.15_philips_number_of_slices",
            "dicom_ps3.15_philips_water_fat_shift",
        ]
        self.assertEqual(self.extract(manifest).dicom_count, 1)

        self.dicom = make_dicom(self.root / "source.dcm")
        self.rewrite_philips_dicom(add_indices)
        manifest = self.scientific_manifest("diffusion")
        manifest["source"] = self.philips_manifest()["source"]
        manifest["deidentification"]["safe_private_exceptions"] = [
            "philips_mr_imaging_dd_005_diffusion_indices_numeric_v1",
            "dicom_ps3.15_philips_scale_intercept_slope",
            "dicom_ps3.15_philips_number_of_slices",
            "dicom_ps3.15_philips_water_fat_shift",
        ]
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED"
        ):
            self.extract(manifest)

    def test_accepts_exact_ps315_ge_diffusion_private_attribute(self) -> None:
        from pydicom.tag import Tag

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def mutate(dataset) -> None:
            dataset.add_new(Tag(0x0043, 0x0010), "LO", "GEMS_PARM_01")
            dataset.add_new(Tag(0x0043, 0x1039), "IS", [1200, 0, 0, 0])
            dataset.add_new(Tag(0x0019, 0x0010), "LO", "GEMS_ACQU_01")
            dataset.add_new(Tag(0x0019, 0x10BB), "DS", "1")
            dataset.add_new(Tag(0x0019, 0x10BC), "DS", "0")
            dataset.add_new(Tag(0x0019, 0x10BD), "DS", "0")

        self.rewrite_dicom(mutate)
        manifest = self.scientific_manifest("diffusion")
        manifest["deidentification"]["safe_private_exceptions"] = [
            "dicom_ps3.15_ge_diffusion_b_value",
            "ge_gems_acqu_01_diffusion_gradient_vector_numeric_v1",
        ]
        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_rejects_disagreeing_public_and_private_diffusion_sources(self) -> None:
        from pydicom.tag import Tag

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def mutate(dataset) -> None:
            dataset.add_new(Tag(0x0018, 0x9087), "FD", 0.0)
            dataset.add_new(Tag(0x0018, 0x9075), "CS", "NONE")
            dataset.add_new(Tag(0x0043, 0x0010), "LO", "GEMS_PARM_01")
            dataset.add_new(Tag(0x0043, 0x1039), "IS", [1200, 0, 0, 0])
            dataset.add_new(Tag(0x0019, 0x0010), "LO", "GEMS_ACQU_01")
            dataset.add_new(Tag(0x0019, 0x10BB), "DS", "1")
            dataset.add_new(Tag(0x0019, 0x10BC), "DS", "0")
            dataset.add_new(Tag(0x0019, 0x10BD), "DS", "0")

        self.rewrite_dicom(mutate)
        manifest = self.scientific_manifest("diffusion")
        manifest["deidentification"]["safe_private_exceptions"] = [
            "dicom_ps3.15_ge_diffusion_b_value",
            "ge_gems_acqu_01_diffusion_gradient_vector_numeric_v1",
        ]

        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_SCIENTIFIC_METADATA_UNVERIFIED"
        ):
            self.extract(manifest)

    def test_accepts_exact_uih_grid_and_diffusion_private_contract(self) -> None:
        from pydicom.tag import Tag

        for image_term in ("GRID", "VFRAME"):
            with self.subTest(image_term=image_term):
                self.dicom = make_structural_dicom(self.root / "source.dcm")

                def mutate(dataset, image_term=image_term) -> None:
                    dataset.Manufacturer = "United Imaging"
                    dataset.ManufacturerModelName = "uMR 790"
                    dataset.SoftwareVersions = "United Imaging 5.0"
                    dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", image_term]
                    dataset.add_new(Tag(0x0065, 0x0010), "LO", "Image Private Header")
                    dataset.add_new(Tag(0x0065, 0x1050), "DS", "32")
                    dataset.add_new(Tag(0x0065, 0x1009), "FD", 1000.0)
                    dataset.add_new(Tag(0x0065, 0x1037), "FD", [1.0, 0.0, 0.0])

                self.rewrite_dicom(mutate)
                manifest = self.scientific_manifest("diffusion")
                manifest["source"]["manufacturer"] = "United Imaging"
                manifest["source"]["model"] = "uMR 790"
                manifest["source"]["software_versions"] = ["United Imaging 5.0"]
                manifest["source"]["image_type"] = [
                    "ORIGINAL",
                    "PRIMARY",
                    "M",
                    image_term,
                ]
                manifest["deidentification"]["safe_private_exceptions"] = [
                    "uih_image_private_header_grid_slice_count_numeric_v1",
                    "uih_image_private_header_diffusion_numeric_v1",
                ]

                self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_accepts_exact_philips_private_asl_contract(self) -> None:
        from pydicom.tag import Tag

        self.dicom = make_structural_dicom(self.root / "source.dcm")

        def mutate(dataset) -> None:
            dataset.Manufacturer = "Philips Medical Systems"
            dataset.ManufacturerModelName = "Achieva dStream"
            dataset.SoftwareVersions = "Philips 5.1.1"
            dataset.add_new(Tag(0x0018, 0x9250), "CS", "PSEUDOCONTINUOUS")
            dataset.InversionTime = "1800"
            dataset.add_new(Tag(0x2005, 0x0010), "LO", "Philips MR Imaging DD 005")
            dataset.add_new(Tag(0x2005, 0x1029), "CS", "LABEL")

        self.rewrite_dicom(mutate)
        manifest = self.scientific_manifest("asl_perfusion")
        manifest["source"]["manufacturer"] = "Philips Medical Systems"
        manifest["source"]["model"] = "Achieva dStream"
        manifest["source"]["software_versions"] = ["Philips 5.1.1"]
        manifest["deidentification"]["safe_private_exceptions"] = [
            "philips_mr_imaging_dd_005_asl_label_code_v1"
        ]

        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_rejects_safe_private_exception_from_wrong_vendor_family(self) -> None:
        from pydicom.tag import Tag

        self.dicom = make_structural_dicom(self.root / "source.dcm")
        self.rewrite_dicom(
            lambda dataset: (
                dataset.add_new(Tag(0x0065, 0x0010), "LO", "Image Private Header"),
                dataset.add_new(Tag(0x0065, 0x1050), "DS", "32"),
            )
        )
        manifest = archive_manifest_v2(
            self.dicom,
            safe_private_exceptions=[
                "uih_image_private_header_grid_slice_count_numeric_v1"
            ],
        )

        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_VENDOR_METADATA_MISMATCH"):
            self.extract(manifest)

    def test_rejects_arbitrary_philips_private_direction_code(self) -> None:
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            dataset.add_new(Tag(0x2001, 0x1003), "FL", 1000.0)
            dataset.add_new(Tag(0x2001, 0x1004), "CS", "SITE42")

        self.rewrite_philips_dicom(mutate)
        manifest = self.philips_manifest()
        manifest["deidentification"]["safe_private_exceptions"].insert(
            0, "dicom_ps3.15_philips_diffusion"
        )
        with self.assertRaisesRegex(InvalidArchive, "DICOM_PRIVACY_AUDIT_FAILED"):
            self.extract(manifest)

    def test_rejects_out_of_contract_vendor_diffusion_private_value(self) -> None:
        from pydicom.tag import Tag

        for values in ([1_000_001, 0, 0, 0], [1000, 1_000_000_001, 0, 0]):
            with self.subTest(values=values):
                self.dicom = make_structural_dicom(self.root / "source.dcm")

                def mutate(dataset, values=values) -> None:
                    dataset.add_new(Tag(0x0043, 0x0010), "LO", "GEMS_PARM_01")
                    dataset.add_new(Tag(0x0043, 0x1039), "IS", values)

                self.rewrite_dicom(mutate)
                manifest = archive_manifest_v2(
                    self.dicom,
                    safe_private_exceptions=["dicom_ps3.15_ge_diffusion_b_value"],
                )
                with self.assertRaisesRegex(
                    InvalidArchive, "DICOM_PRIVACY_AUDIT_FAILED"
                ):
                    self.extract(manifest)

    def test_accepts_only_canonical_rebuilt_siemens_csa_pair(self) -> None:
        from pydicom.tag import Tag

        csa = bytearray(b"SV10\x04\x03\x02\x01")
        csa.extend(struct.pack("<II", 1, 77))
        name = b"NumberOfImagesInMosaic"
        csa.extend(name + b"\0" * (64 - len(name)))
        csa.extend(struct.pack("<i4siii", 1, b"US\0\0", 0, 1, 77))
        csa.extend(struct.pack("<iiii", 2, 2, 77, 0))
        csa.extend(b"4\0\0\0")

        def mutate(dataset) -> None:
            dataset.add_new(Tag(0x0029, 0x0010), "LO", "SIEMENS CSA HEADER")
            dataset.add_new(Tag(0x0029, 0x1010), "OB", bytes(csa))

        self.rewrite_dicom(mutate)
        manifest = archive_manifest(self.dicom)
        manifest["deidentification"]["safe_private_exceptions"] = [
            "siemens_csa_image_header_numeric_v1"
        ]
        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_rejects_noncanonical_siemens_csa_framing(self) -> None:
        mutations = {
            "wrong_numeric_vr": lambda value: value.__setitem__(
                slice(84, 88), b"IS\0\0"
            ),
            "vm_item_count_mismatch": lambda value: struct.pack_into(
                "<i", value, 80, 2
            ),
            "malformed_reserved_trailer": lambda value: struct.pack_into(
                "<i", value, len(value) - 8, 78
            ),
        }
        for case, corrupt in mutations.items():
            with self.subTest(case=case):
                self.dicom = make_dicom(self.root / "source.dcm")

                def mutate(dataset, corrupt=corrupt) -> None:
                    value = bytearray(dataset[0x00291010].value)
                    corrupt(value)
                    dataset[0x00291010].value = bytes(value)

                self.rewrite_dicom(mutate)
                self.assert_privacy_rejected()

    def test_accepts_only_exact_philips_private_conversion_attributes(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            dataset.add_new(Tag(0x0018, 0x9018), "CS", "YES")
            dataset.add_new(Tag(0x0018, 0x9078), "CS", "SENSE")
            dataset.add_new(Tag(0x2005, 0x0010), "LO", "Philips MR Imaging DD 001")
            dataset.add_new(Tag(0x2005, 0x100D), "FL", 0.0)
            dataset.add_new(Tag(0x2005, 0x100E), "FL", 0.00363177)
            dataset.add_new(Tag(0x2001, 0x0010), "LO", "Philips Imaging DD 001")
            dataset.add_new(Tag(0x2001, 0x1018), "SL", 32)
            dataset.add_new(Tag(0x2001, 0x1022), "FL", 0.75)
            dataset.add_new(Tag(0x2005, 0x0014), "LO", "Philips MR Imaging DD 005")
            item = Dataset()
            item.add_new(Tag(0x2005, 0x0010), "LO", "Philips MR Imaging DD 001")
            item.add_new(Tag(0x2005, 0x100E), "FL", 0.00363177)
            dataset.add_new(Tag(0x2005, 0x140F), "SQ", Sequence([item]))

        self.rewrite_philips_dicom(mutate)
        manifest = self.philips_manifest()
        manifest["deidentification"]["safe_private_exceptions"] = [
            "dicom_ps3.15_philips_scale_intercept_slope",
            "dicom_ps3.15_philips_number_of_slices",
            "dicom_ps3.15_philips_water_fat_shift",
            "dicom_ps3.15_philips_per_frame_scale_slope",
        ]
        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_accepts_repeated_philips_scale_in_enhanced_frames(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            frames = []
            for slope in (0.00363177, 0.004):
                frame = Dataset()
                frame.add_new(Tag(0x2005, 0x0010), "LO", "Philips MR Imaging DD 001")
                frame.add_new(Tag(0x2005, 0x100E), "FL", slope)
                frames.append(frame)
            dataset.PerFrameFunctionalGroupsSequence = Sequence(frames)

        self.rewrite_philips_dicom(mutate)
        self.assertEqual(self.extract(self.philips_manifest()).dicom_count, 1)

    def test_rejects_philips_numeric_private_neighbor(self) -> None:
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            dataset.add_new(Tag(0x2001, 0x0010), "LO", "Philips Imaging DD 001")
            dataset.add_new(Tag(0x2001, 0x1018), "SL", 32)
            dataset.add_new(Tag(0x2001, 0x1019), "SL", 20_260_718)

        self.rewrite_philips_dicom(mutate)
        manifest = self.philips_manifest()
        manifest["deidentification"]["safe_private_exceptions"] = [
            "dicom_ps3.15_philips_number_of_slices"
        ]
        with self.assertRaisesRegex(InvalidArchive, "DICOM_PRIVACY_AUDIT_FAILED"):
            self.extract(manifest)

    def test_rejects_extra_child_in_philips_per_frame_sequence(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            dataset.add_new(Tag(0x2005, 0x0014), "LO", "Philips MR Imaging DD 005")
            item = Dataset()
            item.add_new(Tag(0x2005, 0x0010), "LO", "Philips MR Imaging DD 001")
            item.add_new(Tag(0x2005, 0x100E), "FL", 0.00363177)
            item.add_new(Tag(0x2005, 0x100F), "FL", 20_260_718.0)
            dataset.add_new(Tag(0x2005, 0x140F), "SQ", Sequence([item]))

        self.rewrite_dicom(mutate)
        manifest = archive_manifest(self.dicom)
        manifest["deidentification"]["safe_private_exceptions"] = [
            "dicom_ps3.15_philips_per_frame_scale_slope"
        ]
        with self.assertRaisesRegex(InvalidArchive, "DICOM_PRIVACY_AUDIT_FAILED"):
            self.extract(manifest)

    def test_rejects_unattested_safe_private_exception(self) -> None:
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            dataset.add_new(Tag(0x2005, 0x0010), "LO", "Philips MR Imaging DD 001")
            dataset.add_new(Tag(0x2005, 0x100E), "FL", 0.00363177)

        self.rewrite_philips_dicom(mutate)
        manifest = self.philips_manifest()
        manifest["deidentification"]["safe_private_exceptions"].remove(
            "dicom_ps3.15_philips_scale_intercept_slope"
        )
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_DEIDENTIFICATION_UNVERIFIED"
        ):
            self.extract(manifest)

    def test_accepts_exact_philips_trigger_time_suppression_attestation(self) -> None:
        self.rewrite_philips_dicom()
        manifest = self.philips_manifest()
        manifest["deidentification"]["metadata_transformations"] = [
            "suppressed_redundant_philips_dynamic_trigger_time"
        ]
        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_accepts_classic_image_type_replacement_attestation(self) -> None:
        self.dicom = make_structural_dicom(self.root / "source.dcm")
        self.rewrite_dicom(
            lambda dataset: setattr(
                dataset, "ImageType", ["ORIGINAL", "PRIMARY", "M", "OTHER"]
            )
        )
        manifest = archive_manifest_v2(self.dicom)
        manifest["deidentification"]["metadata_transformations"] = [
            "replaced_unknown_classic_image_type_components_with_other"
        ]

        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_accepts_ordered_image_type_and_philips_trigger_transformations(
        self,
    ) -> None:
        def mutate(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "EPI", "OTHER"]
            dataset.DeidentificationMethod = (
                "Scaling Neuro scaling-neuro.dicom-deidentification 2.0.0"
            )

        self.rewrite_philips_dicom(mutate)
        legacy = self.philips_manifest()
        manifest = archive_manifest_v2(
            self.dicom,
            series_kind="functional_epi",
            processing_route="functional-epi-v1",
            evidence_code="functional_epi_confirmed",
            source=legacy["source"],
            safe_private_exceptions=legacy["deidentification"][
                "safe_private_exceptions"
            ],
        )
        manifest["deidentification"]["metadata_transformations"] = [
            "replaced_unknown_classic_image_type_components_with_other",
            "suppressed_redundant_philips_dynamic_trigger_time",
        ]

        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_classic_image_type_vm_boundary_matches_client_contract(self) -> None:
        for count in (17, 64):
            with self.subTest(count=count):
                self.dicom = make_structural_dicom(self.root / "source.dcm")
                self.rewrite_dicom(
                    lambda dataset, count=count: setattr(
                        dataset,
                        "ImageType",
                        ["ORIGINAL", "PRIMARY"] + ["OTHER"] * (count - 2),
                    )
                )
                self.assertEqual(
                    self.extract(archive_manifest_v2(self.dicom)).dicom_count, 1
                )

        self.dicom = make_structural_dicom(self.root / "source.dcm")
        self.rewrite_dicom(
            lambda dataset: setattr(
                dataset,
                "ImageType",
                ["ORIGINAL", "PRIMARY"] + ["OTHER"] * 63,
            )
        )
        self.assert_privacy_rejected()

    def test_rejects_suppression_attestation_when_trigger_time_survives(self) -> None:
        def mutate(dataset) -> None:
            dataset.TriggerTime = "800"

        self.rewrite_philips_dicom(mutate)
        manifest = self.philips_manifest()
        manifest["deidentification"]["metadata_transformations"] = [
            "suppressed_redundant_philips_dynamic_trigger_time"
        ]
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_DEIDENTIFICATION_UNVERIFIED"
        ):
            self.extract(manifest)

    def test_accepts_missing_burned_in_only_with_original_primary_status(self) -> None:
        def mutate(dataset) -> None:
            del dataset.BurnedInAnnotation

        self.rewrite_dicom(mutate)
        manifest = archive_manifest(self.dicom)
        manifest["deidentification"]["burned_in_annotation_status"] = "not_declared"
        self.assertEqual(self.extract(manifest).dicom_count, 1)

    def test_accepts_bounded_encapsulated_pixel_data(self) -> None:
        from pydicom.encaps import encapsulate
        from pydicom.uid import JPEG2000Lossless

        def mutate(dataset) -> None:
            dataset.file_meta.TransferSyntaxUID = JPEG2000Lossless
            dataset.PixelData = encapsulate([b"synthetic-jpeg2000-frame"])
            dataset["PixelData"].is_undefined_length = True

        self.rewrite_dicom(mutate)
        self.assertEqual(self.extract(archive_manifest(self.dicom)).dicom_count, 1)

    def test_accepts_structurally_verified_extended_offset_tables(self) -> None:
        from pydicom.encaps import encapsulate_extended
        from pydicom.tag import Tag
        from pydicom.uid import EnhancedMRImageStorage, JPEG2000Lossless

        def mutate(dataset) -> None:
            pixel_data, offsets, lengths = encapsulate_extended(
                [b"synthetic-frame-one", b"synthetic-frame-two-x"]
            )
            dataset.file_meta.TransferSyntaxUID = JPEG2000Lossless
            dataset.SOPClassUID = EnhancedMRImageStorage
            dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "FMRI", "NONE"]
            dataset.NumberOfFrames = "2"
            dataset.add_new(Tag(0x7FE0, 0x0001), "OV", offsets)
            dataset.add_new(Tag(0x7FE0, 0x0002), "OV", lengths)
            dataset.PixelData = pixel_data
            dataset["PixelData"].VR = "OB"
            dataset["PixelData"].is_undefined_length = True

        self.rewrite_dicom(mutate)
        self.assertEqual(self.extract(archive_manifest(self.dicom)).dicom_count, 1)

    def test_rejects_extended_offset_table_that_does_not_index_pixel_data(self) -> None:
        from pydicom.encaps import encapsulate_extended
        from pydicom.tag import Tag
        from pydicom.uid import EnhancedMRImageStorage, JPEG2000Lossless

        def mutate(dataset) -> None:
            pixel_data, offsets, lengths = encapsulate_extended(
                [b"synthetic-frame-one", b"synthetic-frame-two-x"]
            )
            dataset.file_meta.TransferSyntaxUID = JPEG2000Lossless
            dataset.SOPClassUID = EnhancedMRImageStorage
            dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "FMRI", "NONE"]
            dataset.NumberOfFrames = "2"
            dataset.add_new(Tag(0x7FE0, 0x0001), "OV", offsets)
            corrupted = bytearray(lengths)
            corrupted[0] ^= 1
            dataset.add_new(Tag(0x7FE0, 0x0002), "OV", bytes(corrupted))
            dataset.PixelData = pixel_data
            dataset["PixelData"].VR = "OB"
            dataset["PixelData"].is_undefined_length = True

        self.rewrite_dicom(mutate)
        self.assert_privacy_rejected()

    def test_rejects_missing_burned_in_on_derived_image(self) -> None:
        def mutate(dataset) -> None:
            del dataset.BurnedInAnnotation
            dataset.ImageType = ["DERIVED", "SECONDARY", "M", "EPI"]

        self.rewrite_dicom(mutate)
        manifest = archive_manifest(self.dicom)
        manifest["deidentification"]["burned_in_annotation_status"] = "not_declared"
        with self.assertRaisesRegex(InvalidArchive, "DICOM_PRIVACY_AUDIT_FAILED"):
            self.extract(manifest)

    def test_rejects_unexpected_file_meta_identity(self) -> None:
        def mutate(dataset) -> None:
            dataset.file_meta.SourceApplicationEntityTitle = "RESEARCH_SITE"

        self.rewrite_dicom(mutate)
        self.assert_privacy_rejected()

    def test_rejects_calendar_date(self) -> None:
        def mutate(dataset) -> None:
            dataset.StudyDate = "20260718"

        self.rewrite_dicom(mutate)
        self.assert_privacy_rejected()

    def test_rejects_unremapped_referenced_uid(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag
        from pydicom.uid import MRImageStorage

        def mutate(dataset) -> None:
            nested = Dataset()
            nested.ReferencedSOPClassUID = MRImageStorage
            nested.ReferencedSOPInstanceUID = "1.2.840.113619.2.55.3.604688435.1"
            dataset.add_new(Tag(0x0008, 0x1140), "SQ", Sequence([nested]))

        self.rewrite_dicom(mutate)
        self.assert_privacy_rejected()

    def test_rejects_private_uid_in_referenced_class_slot(self) -> None:
        from pydicom.dataset import Dataset
        from pydicom.sequence import Sequence
        from pydicom.tag import Tag

        def mutate(dataset) -> None:
            nested = Dataset()
            nested.ReferencedSOPClassUID = "1.2.840.113619.4.2"
            nested.ReferencedSOPInstanceUID = "2.25.12345"
            dataset.add_new(Tag(0x0008, 0x1140), "SQ", Sequence([nested]))

        self.rewrite_dicom(mutate)
        self.assert_privacy_rejected()


if __name__ == "__main__":
    unittest.main()
