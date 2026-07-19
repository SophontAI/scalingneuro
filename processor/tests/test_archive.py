from __future__ import annotations

import copy
import io
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch

from scaling_neuro_processor.archive import (
    PHILIPS_REQUIRED_PRIVATE_FIELDS,
    SCANNER_MODELS,
    SANDBOX_ZSTD_INVALID_EXIT,
    _check_zstd_returncode,
    extract_archive,
    zstd_decompression_command,
)
from scaling_neuro_processor.config import Config
from scaling_neuro_processor.errors import ConverterFailure, InvalidArchive
from scaling_neuro_processor.sandbox import NATIVE_ZSTD
from scaling_neuro_processor.dicom_privacy import CANONICAL_MODELS

from tests.helpers import ARCHIVE_ID, archive_manifest, make_archive, make_dicom


class ArchiveTests(unittest.TestCase):
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
    ):
        archive = (
            self.root / f"archive-{len(list(self.root.glob('archive-*')))}.tar.zst"
        )
        make_archive(
            archive,
            self.dicom,
            manifest,
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
            expected_dicom_count=1,
        )

    def rewrite_dicom(self, mutate) -> None:
        from pydicom import dcmread

        path = self.root / "source.dcm"
        dataset = dcmread(path)
        mutate(dataset)
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

    def assert_functional_purpose_rejected(self, mutate) -> None:
        self.rewrite_dicom(mutate)
        with self.assertRaisesRegex(InvalidArchive, "FUNCTIONAL_EPI_NOT_CONFIRMED"):
            self.extract(archive_manifest(self.dicom))

    def test_extracts_and_verifies_every_instance(self) -> None:
        result = self.extract(archive_manifest(self.dicom))
        self.assertEqual(result.dicom_count, 1)
        self.assertEqual(result.value["source"]["manufacturer"], "SIEMENS")

    def test_scanner_model_vocabulary_matches_header_audit(self) -> None:
        self.assertEqual(SCANNER_MODELS, CANONICAL_MODELS)
        self.assertIn("Achieva dStream", SCANNER_MODELS)

    def test_accepts_only_exact_release_routes(self) -> None:
        self.assertEqual(self.extract(archive_manifest(self.dicom)).dicom_count, 1)

        self.dicom = make_dicom(self.root / "source.dcm")
        self.rewrite_philips_dicom()
        self.assertEqual(self.extract(self.philips_manifest()).dicom_count, 1)

    def test_rejects_siemens_without_mosaic_or_sanitized_csa(self) -> None:
        def remove_mosaic(dataset) -> None:
            dataset.ImageType = [
                value for value in dataset.ImageType if str(value) != "MOSAIC"
            ]

        self.rewrite_dicom(remove_mosaic)
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_SIEMENS_CSA_REQUIRED"):
            self.extract(archive_manifest(self.dicom))

        self.dicom = make_dicom(self.root / "source.dcm")

        def remove_csa(dataset) -> None:
            del dataset[0x00290010]
            del dataset[0x00291010]

        self.rewrite_dicom(remove_csa)
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_SIEMENS_CSA_REQUIRED"):
            self.extract(archive_manifest(self.dicom))

    def test_rejects_philips_without_every_reviewed_private_field(self) -> None:
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
                with self.assertRaisesRegex(
                    InvalidArchive, "ARCHIVE_PHILIPS_PRIVATE_METADATA_REQUIRED"
                ):
                    self.extract(self.philips_manifest())

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

        self.assert_functional_purpose_rejected(mutate)

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

        self.assert_functional_purpose_rejected(mutate)

    def test_rejects_perfusion_headers_despite_functional_manifest(self) -> None:
        def mutate(dataset) -> None:
            dataset.AcquisitionContrast = "PERFUSION"

        self.assert_functional_purpose_rejected(mutate)

    def test_rejects_asl_like_perfusion_headers(self) -> None:
        def mutate(dataset) -> None:
            dataset.AcquisitionContrast = "PERFUSION"
            dataset.SequenceName = "ep2d"
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "EPI", "MOSAIC"]

        self.assert_functional_purpose_rejected(mutate)

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

        self.assert_functional_purpose_rejected(mutate)

    def test_rejects_non_epi_and_missing_temporal_structure(self) -> None:
        def non_epi(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "BOLD", "MOSAIC"]
            dataset.ScanningSequence = "GR"
            del dataset.SequenceName

        self.assert_functional_purpose_rejected(non_epi)

        self.dicom = make_dicom(self.root / "source.dcm")

        def no_temporal_structure(dataset) -> None:
            del dataset.NumberOfTemporalPositions

        self.assert_functional_purpose_rejected(no_temporal_structure)

    def test_rejects_localizer_like_non_epi_headers(self) -> None:
        def localizer(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "MOSAIC"]
            dataset.ScanningSequence = "GR"
            del dataset.SequenceName

        self.assert_functional_purpose_rejected(localizer)

    def test_functional_label_alone_does_not_substitute_for_epi_evidence(self) -> None:
        def bold_label_without_epi(dataset) -> None:
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "M", "BOLD", "MOSAIC"]
            dataset.ScanningSequence = "GR"
            dataset.SequenceName = "bold"

        self.assert_functional_purpose_rejected(bold_label_without_epi)

    def test_rejects_enhanced_mr_until_separately_validated(self) -> None:
        from pydicom.uid import EnhancedMRImageStorage

        def enhanced(dataset) -> None:
            dataset.SOPClassUID = EnhancedMRImageStorage
            dataset.file_meta.MediaStorageSOPClassUID = EnhancedMRImageStorage

        self.rewrite_dicom(enhanced)
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_UNSUPPORTED_DICOM_FORM"):
            self.extract(archive_manifest(self.dicom))

    def test_rejects_manifest_and_dicom_manufacturer_mismatch(self) -> None:
        manifest = archive_manifest(self.dicom)
        manifest["source"]["manufacturer"] = "Philips Medical Systems"
        manifest["source"]["model"] = "Achieva dStream"
        manifest["source"]["software_versions"] = ["Philips 5.1.1"]
        with self.assertRaisesRegex(InvalidArchive, "ARCHIVE_MANUFACTURER_MISMATCH"):
            self.extract(manifest)

    def test_rejects_unverified_scanner_model_or_software(self) -> None:
        def mutate(dataset) -> None:
            dataset.ManufacturerModelName = "MAGNETOM Prisma"

        self.rewrite_dicom(mutate)
        manifest = archive_manifest(self.dicom)
        manifest["source"]["model"] = "MAGNETOM Prisma"
        with self.assertRaisesRegex(
            InvalidArchive, "ARCHIVE_UNVERIFIED_SCANNER_FAMILY"
        ):
            self.extract(manifest)

    def test_production_zstd_runs_in_tokenless_readonly_container(self) -> None:
        archive = self.root / "input.tar.zst"
        archive.write_bytes(b"test")
        config = Config(
            api_url="https://scalingneuro.com",
            token="controller-secret-not-for-zstd",
            work_root=self.root / "controller-work",
            processor_id="zstd-sandbox-test",
            native_tools_slurm_image=Path("/release/native-tools.sqsh"),
            enroot_runtime_root=self.root / "enroot",
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
            enroot_runtime_root=self.root / "enroot",
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
