from __future__ import annotations

import hashlib
from pathlib import Path
import struct
import tempfile
import unittest

from pydicom import dcmread
from pydicom.dataelem import DataElement
from pydicom.dataset import Dataset
from pydicom.sequence import Sequence
from pydicom.tag import Tag
from pydicom.uid import UID

from scaling_neuro_processor.dicom_privacy import (
    BITS_ALLOCATED,
    BITS_STORED,
    CLASSIC_MR_IMAGE_STORAGE_UID,
    CODE_VALUES,
    COLUMNS,
    ENHANCED_MR_IMAGE_STORAGE_UIDS,
    HIGH_BIT,
    LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
    NUMBER_OF_FRAMES,
    PHOTOMETRIC_INTERPRETATION,
    PIXEL_REPRESENTATION,
    PLANAR_CONFIGURATION,
    PRIVACY_ERROR,
    ROWS,
    SAMPLES_PER_PIXEL,
    audit_dicom,
    _PrivacyViolation,
    _audit_source_image_sequence,
)
from scaling_neuro_processor.errors import InvalidArchive

from tests.helpers import SUBJECT_ID, conform_enhanced_mr, make_dicom


ENHANCED_MR_IMAGE_STORAGE_UID = "1.2.840.10008.5.1.4.1.1.4.1"
ENHANCED_MR_COLOR_IMAGE_STORAGE_UID = "1.2.840.10008.5.1.4.1.1.4.3"


def siemens_csa_diffusion(
    *, b_value: float, gradient: tuple[float, float, float]
) -> bytes:
    fields = (
        ("NumberOfImagesInMosaic", b"US\0\0", (4.0,)),
        ("B_value", b"DS\0\0", (b_value,)),
        ("DiffusionGradientDirection", b"DS\0\0", gradient),
    )
    output = bytearray(b"SV10\x04\x03\x02\x01")
    output.extend(struct.pack("<II", len(fields), 77))
    for name, vr, values in fields:
        encoded_name = name.encode("ascii")
        output.extend(encoded_name + b"\0" * (64 - len(encoded_name)))
        output.extend(struct.pack("<i4siii", len(values), vr, 0, len(values), 77))
        for value in values:
            raw = f"{value:g}".encode("ascii") + b"\0"
            output.extend(struct.pack("<iiii", len(raw), len(raw), 77, 0))
            output.extend(raw)
            output.extend(b"\0" * ((-len(raw)) % 4))
    return bytes(output)


class PixelModuleTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.counter = 0

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def conformant_dicom(
        self,
        *,
        sop_class: str = CLASSIC_MR_IMAGE_STORAGE_UID,
        mutate=None,
    ) -> Path:
        self.counter += 1
        path = self.root / f"source-{self.counter}.dcm"
        make_dicom(path)
        dataset = dcmread(path)
        dataset.StudyDate = ""
        dataset.AcquisitionDate = ""
        dataset.ContentDate = ""
        dataset.StudyTime = ""
        dataset.AcquisitionTime = ""
        dataset.ContentTime = ""
        dataset.AccessionNumber = ""
        dataset.ReferringPhysicianName = ""
        dataset.PatientBirthDate = ""
        dataset.PatientSex = ""
        dataset.StudyID = ""
        dataset.FrameOfReferenceUID = "2.25.100000000000000000000000000000000003"
        dataset.PositionReferenceIndicator = ""
        dataset.InstanceNumber = "1"
        dataset.EchoTrainLength = "1"
        if sop_class != CLASSIC_MR_IMAGE_STORAGE_UID:
            dataset.SOPClassUID = sop_class
            dataset.file_meta.MediaStorageSOPClassUID = UID(sop_class)
            dataset.ImageType = ["ORIGINAL", "PRIMARY", "FMRI", "NONE"]
            dataset.NumberOfFrames = "2"
            dataset.DeviceSerialNumber = "SN-0123456789abcdef01234567"
            conform_enhanced_mr(dataset)
        if mutate is not None:
            mutate(dataset)
        dataset.save_as(path, enforce_file_format=True)
        return path

    def audit(self, path: Path):
        return audit_dicom(path, expected_subject_id=SUBJECT_ID)

    def assert_rejected(self, path: Path) -> None:
        with self.assertRaisesRegex(InvalidArchive, PRIVACY_ERROR):
            self.audit(path)

    def test_supported_profiles_keep_pixel_data_byte_exact(self) -> None:
        self.assertEqual(
            ENHANCED_MR_IMAGE_STORAGE_UIDS,
            {
                ENHANCED_MR_IMAGE_STORAGE_UID,
                LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
            },
        )
        for sop_class in (
            CLASSIC_MR_IMAGE_STORAGE_UID,
            ENHANCED_MR_IMAGE_STORAGE_UID,
            LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
        ):
            with self.subTest(sop_class=sop_class):
                path = self.conformant_dicom(sop_class=sop_class)
                before = path.read_bytes()
                before_pixels = bytes(dcmread(path).PixelData)
                audit = self.audit(path)
                after = path.read_bytes()
                after_pixels = bytes(dcmread(path).PixelData)
                self.assertEqual(audit.sop_class_uid, sop_class)
                self.assertEqual(
                    hashlib.sha256(after).digest(), hashlib.sha256(before).digest()
                )
                self.assertEqual(after_pixels, before_pixels)

    def test_enhanced_color_is_not_a_supported_server_profile(self) -> None:
        path = self.conformant_dicom(sop_class=ENHANCED_MR_COLOR_IMAGE_STORAGE_UID)
        self.assert_rejected(path)
        self.assertTrue(
            {"YBR_ICT", "YBR_RCT", "YBR_PARTIAL_420"}.issubset(
                CODE_VALUES[PHOTOMETRIC_INTERPRETATION]
            )
        )

    def test_every_required_pixel_module_attribute_is_required(self) -> None:
        for tag in (
            ROWS,
            COLUMNS,
            SAMPLES_PER_PIXEL,
            PHOTOMETRIC_INTERPRETATION,
            BITS_ALLOCATED,
            BITS_STORED,
            HIGH_BIT,
            PIXEL_REPRESENTATION,
        ):
            with self.subTest(tag=str(tag)):
                path = self.conformant_dicom(
                    mutate=lambda dataset, tag=tag: dataset.pop(tag)
                )
                self.assert_rejected(path)

    def test_rejects_unsafe_cross_field_pixel_combinations(self) -> None:
        cases = (
            (ROWS, 0),
            (COLUMNS, 0),
            (SAMPLES_PER_PIXEL, 3),
            (PHOTOMETRIC_INTERPRETATION, "RGB"),
            (BITS_ALLOCATED, 8),
            (BITS_STORED, 17),
            (HIGH_BIT, 14),
            (PIXEL_REPRESENTATION, 2),
            (PLANAR_CONFIGURATION, 0),
        )
        for tag, value in cases:
            with self.subTest(tag=str(tag), value=value):
                path = self.conformant_dicom(
                    mutate=lambda dataset, tag=tag, value=value: dataset.add_new(
                        tag,
                        "CS" if tag == PHOTOMETRIC_INTERPRETATION else "US",
                        value,
                    )
                )
                self.assert_rejected(path)

    def test_enhanced_profile_requires_valid_number_of_frames(self) -> None:
        missing = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=lambda dataset: dataset.pop(NUMBER_OF_FRAMES),
        )
        self.assert_rejected(missing)
        zero = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=lambda dataset: setattr(dataset, "NumberOfFrames", "0"),
        )
        self.assert_rejected(zero)

    def test_enhanced_mr_requires_explicit_burned_in_annotation_no(self) -> None:
        missing = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=lambda dataset: dataset.pop(Tag(0x0028, 0x0301)),
        )
        self.assert_rejected(missing)

        legacy_missing = self.conformant_dicom(
            sop_class=LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=lambda dataset: dataset.pop(Tag(0x0028, 0x0301)),
        )
        self.assert_rejected(legacy_missing)

    def test_enhanced_mr_mandatory_modules_are_enforced(self) -> None:
        mandatory_tags = (
            Tag(0x0008, 0x0023),  # Content Date
            Tag(0x0008, 0x0033),  # Content Time
            Tag(0x0008, 0x9205),  # Pixel Presentation
            Tag(0x0008, 0x9206),  # Volumetric Properties
            Tag(0x0008, 0x9207),  # Volume Based Calculation Technique
            Tag(0x0008, 0x9208),  # Complex Image Component
            Tag(0x0008, 0x9209),  # Acquisition Contrast
            Tag(0x0020, 0x9221),  # Dimension Organization Sequence
            Tag(0x0020, 0x9222),  # Dimension Index Sequence
            Tag(0x0028, 0x2110),  # Lossy Image Compression
            Tag(0x2050, 0x0020),  # Presentation LUT Shape
            Tag(0x5200, 0x9229),  # Shared Functional Groups Sequence
            Tag(0x5200, 0x9230),  # Per-frame Functional Groups Sequence
        )
        for tag in mandatory_tags:
            with self.subTest(tag=str(tag)):
                path = self.conformant_dicom(
                    sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                    mutate=lambda dataset, tag=tag: dataset.pop(tag),
                )
                self.assert_rejected(path)

    def test_enhanced_uses_effective_echo_time_without_empty_classic_shell(self) -> None:
        def remove_classic_echo_time(dataset) -> None:
            dataset.pop(Tag(0x0018, 0x0081), None)

        accepted = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=remove_classic_echo_time,
        )
        dataset = dcmread(accepted, stop_before_pixels=True)
        self.assertNotIn(Tag(0x0018, 0x0081), dataset)
        self.assertEqual(
            dataset.SharedFunctionalGroupsSequence[0].MREchoSequence[0].EffectiveEchoTime,
            30.0,
        )
        self.assertEqual(self.audit(accepted).sop_class_uid, ENHANCED_MR_IMAGE_STORAGE_UID)

        empty_classic_shell = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=lambda dataset: dataset.add_new(Tag(0x0018, 0x0081), "DS", None),
        )
        self.assert_rejected(empty_classic_shell)

    def test_enhanced_context_and_concatenations_fail_closed(self) -> None:
        def nonempty_context(dataset) -> None:
            dataset.add_new(Tag(0x0040, 0x0555), "SQ", Sequence([Dataset()]))

        self.assert_rejected(
            self.conformant_dicom(
                sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                mutate=nonempty_context,
            )
        )
        for tag, vr, value in (
            (Tag(0x0020, 0x0242), "UI", "2.25.123"),
            (Tag(0x0020, 0x9161), "UI", "2.25.124"),
            (Tag(0x0020, 0x9162), "US", 1),
            (Tag(0x0020, 0x9163), "US", 1),
            (Tag(0x0020, 0x9228), "UL", 2),
        ):
            with self.subTest(tag=tag):
                self.assert_rejected(
                    self.conformant_dicom(
                        sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                        mutate=lambda dataset, tag=tag, vr=vr, value=value: (
                            dataset.add_new(tag, vr, value)
                        ),
                    )
                )

    def test_legacy_dimensions_are_optional_but_atomic(self) -> None:
        def remove_all_dimensions(dataset) -> None:
            del dataset.DimensionOrganizationSequence
            del dataset.DimensionIndexSequence
            for frame in dataset.PerFrameFunctionalGroupsSequence:
                del frame.FrameContentSequence[0].DimensionIndexValues

        accepted = self.conformant_dicom(
            sop_class=LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=remove_all_dimensions,
        )
        self.assertEqual(
            self.audit(accepted).sop_class_uid,
            LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
        )

        def missing_index(dataset) -> None:
            del dataset.DimensionIndexSequence

        def orphan_values(dataset) -> None:
            del dataset.DimensionOrganizationSequence
            del dataset.DimensionIndexSequence

        for mutate in (missing_index, orphan_values):
            self.assert_rejected(
                self.conformant_dicom(
                    sop_class=LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
                    mutate=mutate,
                )
            )

    def test_legacy_converted_shells_and_functional_group_surface_are_strict(
        self,
    ) -> None:
        def missing_shared_shell(dataset) -> None:
            del dataset.SharedFunctionalGroupsSequence[0][Tag(0x0020, 0x9170)]

        def nonempty_shared_shell(dataset) -> None:
            item = Dataset()
            item.PatientName = "IDENTITY^LEAK"
            dataset.SharedFunctionalGroupsSequence[0].add_new(
                Tag(0x0020, 0x9170), "SQ", Sequence([item])
            )

        def missing_per_frame_shell(dataset) -> None:
            del dataset.PerFrameFunctionalGroupsSequence[0][Tag(0x0020, 0x9171)]

        def a36_only_macro(dataset) -> None:
            item = Dataset()
            item.EffectiveEchoTime = 30.0
            dataset.SharedFunctionalGroupsSequence[0].MREchoSequence = Sequence([item])

        def conversion_source(dataset) -> None:
            dataset.PerFrameFunctionalGroupsSequence[0].add_new(
                Tag(0x0020, 0x9172), "SQ", Sequence([Dataset()])
            )

        for mutate in (
            missing_shared_shell,
            nonempty_shared_shell,
            missing_per_frame_shell,
            a36_only_macro,
            conversion_source,
        ):
            self.assert_rejected(
                self.conformant_dicom(
                    sop_class=LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
                    mutate=mutate,
                )
            )

    def test_legacy_equipment_is_optional_and_mixed_empty_frame_types_survive(
        self,
    ) -> None:
        def optional_equipment_and_mixed_type(dataset) -> None:
            for tag in (
                Tag(0x0008, 0x1090),
                Tag(0x0018, 0x1000),
                Tag(0x0018, 0x1020),
            ):
                dataset.pop(tag, None)
            dataset.ImageType = ["MIXED", "PRIMARY", "FMRI", ""]
            for frame in dataset.PerFrameFunctionalGroupsSequence:
                frame.MRImageFrameTypeSequence[0].FrameType = [
                    "MIXED",
                    "PRIMARY",
                    "FMRI",
                    "",
                ]

        path = self.conformant_dicom(
            sop_class=LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=optional_equipment_and_mixed_type,
        )
        self.assertEqual(
            self.audit(path).sop_class_uid,
            LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
        )

    def test_current_enhanced_core_macro_values_and_datetime_sentinels_are_enforced(
        self,
    ) -> None:
        def zero_spacing(dataset) -> None:
            dataset.SharedFunctionalGroupsSequence[0].PixelMeasuresSequence[
                0
            ].PixelSpacing = ["0", "2"]

        def nonorthogonal_orientation(dataset) -> None:
            dataset.SharedFunctionalGroupsSequence[0].PlaneOrientationSequence[
                0
            ].ImageOrientationPatient = ["1", "0", "0", "1", "0", "0"]

        def source_datetime(dataset) -> None:
            dataset.PerFrameFunctionalGroupsSequence[0].FrameContentSequence[
                0
            ].FrameAcquisitionDateTime = "20260718120000"

        def missing_timing_macro(dataset) -> None:
            del dataset.SharedFunctionalGroupsSequence[
                0
            ].MRTimingAndRelatedParametersSequence

        for mutate in (
            zero_spacing,
            nonorthogonal_orientation,
            source_datetime,
            missing_timing_macro,
        ):
            self.assert_rejected(
                self.conformant_dicom(
                    sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                    mutate=mutate,
                )
            )

    def test_current_enhanced_accepts_only_canonical_pulse_sequence_fallback(
        self,
    ) -> None:
        other = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=lambda dataset: dataset.add_new(Tag(0x0018, 0x9005), "SH", "OTHER"),
        )
        self.assertEqual(self.audit(other).sop_class_uid, ENHANCED_MR_IMAGE_STORAGE_UID)
        arbitrary = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=lambda dataset: dataset.add_new(
                Tag(0x0018, 0x9005), "SH", "vendor_sequence"
            ),
        )
        self.assert_rejected(arbitrary)

    def test_enhanced_standard_codes_accept_valid_terms_and_reject_old_aliases(
        self,
    ) -> None:
        def valid_standard_terms(dataset) -> None:
            dataset.ImageType = ["DERIVED", "PRIMARY", "VOLUME", "NONE"]
            dataset.add_new(Tag(0x0008, 0x9207), "CS", "MPR")
            dataset.add_new(Tag(0x0008, 0x9209), "CS", "STIR")
            for frame in dataset.PerFrameFunctionalGroupsSequence:
                item = frame.MRImageFrameTypeSequence[0]
                item.FrameType = ["DERIVED", "PRIMARY", "VOLUME", "NONE"]
                item.add_new(Tag(0x0008, 0x9207), "CS", "MPR")

        valid = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=valid_standard_terms,
        )
        self.assertEqual(self.audit(valid).sop_class_uid, ENHANCED_MR_IMAGE_STORAGE_UID)

        invalid_cases = (
            (Tag(0x0008, 0x9207), "RECON_PLANAR"),
            (Tag(0x0008, 0x9209), "NONE"),
            (Tag(0x2050, 0x0020), "INVERSE"),
        )
        for tag, value in invalid_cases:
            with self.subTest(tag=str(tag), value=value):
                path = self.conformant_dicom(
                    sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                    mutate=lambda dataset, tag=tag, value=value: dataset.add_new(
                        tag, "CS", value
                    ),
                )
                self.assert_rejected(path)

    def test_enhanced_rectilinear_reordering_accepts_defined_unknown_only(self) -> None:
        accepted = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=lambda dataset: dataset.add_new(
                Tag(0x0018, 0x9034), "CS", "UNKNOWN"
            ),
        )
        self.assertEqual(self.audit(accepted).sop_class_uid, ENHANCED_MR_IMAGE_STORAGE_UID)

        for case, vr, value in (
            ("wrong-vr", "LO", "UNKNOWN"),
            ("wrong-vm", "CS", ["UNKNOWN", "LINEAR"]),
            ("free-text", "CS", "UNKNOWN SITE"),
        ):
            with self.subTest(case=case):
                rejected = self.conformant_dicom(
                    sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                    mutate=lambda dataset, vr=vr, value=value: dataset.add_new(
                        Tag(0x0018, 0x9034), vr, value
                    ),
                )
                self.assert_rejected(rejected)

    def test_enhanced_multi_coil_macro_is_atomic_and_canonical(self) -> None:
        receive_sequence = Tag(0x0018, 0x9042)
        definition_sequence = Tag(0x0018, 0x9045)

        def element(*, name="MULTI_ELEMENT", name_vr="SH", used="YES") -> Dataset:
            result = Dataset()
            result.add_new(Tag(0x0018, 0x9047), name_vr, name)
            result.add_new(Tag(0x0018, 0x9048), "CS", used)
            return result

        def install(dataset, *, elements=None, receive_name="MULTI_COIL") -> Dataset:
            receive = Dataset()
            receive.add_new(Tag(0x0018, 0x1250), "SH", receive_name)
            receive.add_new(Tag(0x0018, 0x9041), "LO", "")
            receive.add_new(Tag(0x0018, 0x9043), "CS", "MULTICOIL")
            receive.add_new(Tag(0x0018, 0x9044), "CS", "NO")
            receive.add_new(
                definition_sequence,
                "SQ",
                Sequence([element()] if elements is None else elements),
            )
            dataset.SharedFunctionalGroupsSequence[0].add_new(
                receive_sequence, "SQ", Sequence([receive])
            )
            return receive

        def accepted_macro(dataset) -> None:
            install(dataset, elements=[element(used="YES"), element(used="NO")])

        accepted = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=accepted_macro,
        )
        self.assertEqual(self.audit(accepted).sop_class_uid, ENHANCED_MR_IMAGE_STORAGE_UID)

        def mutate_parent(callback):
            def mutate(dataset) -> None:
                callback(dataset, install(dataset))

            return mutate

        extra_child = element()
        extra_child.add_new(Tag(0x0018, 0x9046), "LO", "configuration")
        private_child = element()
        private_child.add_new(Tag(0x0019, 0x1001), "LO", "hidden")
        cases = (
            (
                "arbitrary-receive-name",
                mutate_parent(
                    lambda _dataset, receive: receive.add_new(
                        Tag(0x0018, 0x1250), "SH", "SITE ARRAY"
                    )
                ),
            ),
            (
                "configuration-free-text",
                mutate_parent(
                    lambda _dataset, receive: receive.add_new(
                        Tag(0x0018, 0x9046), "LO", "site configuration"
                    )
                ),
            ),
            (
                "arbitrary-element-name",
                mutate_parent(
                    lambda _dataset, receive: receive.add_new(
                        definition_sequence,
                        "SQ",
                        Sequence([element(name="SITE ELEMENT")]),
                    )
                ),
            ),
            (
                "element-name-vr",
                mutate_parent(
                    lambda _dataset, receive: receive.add_new(
                        definition_sequence,
                        "SQ",
                        Sequence([element(name_vr="LO")]),
                    )
                ),
            ),
            (
                "element-use-code",
                mutate_parent(
                    lambda _dataset, receive: receive.add_new(
                        definition_sequence,
                        "SQ",
                        Sequence([element(used="MAYBE")]),
                    )
                ),
            ),
            (
                "empty-definition",
                mutate_parent(
                    lambda _dataset, receive: receive.add_new(
                        definition_sequence, "SQ", Sequence([])
                    )
                ),
            ),
            (
                "too-many-elements",
                mutate_parent(
                    lambda _dataset, receive: receive.add_new(
                        definition_sequence,
                        "SQ",
                        Sequence([element() for _ in range(257)]),
                    )
                ),
            ),
            (
                "extra-element-field",
                mutate_parent(
                    lambda _dataset, receive: receive.add_new(
                        definition_sequence, "SQ", Sequence([extra_child])
                    )
                ),
            ),
            (
                "private-element-field",
                mutate_parent(
                    lambda _dataset, receive: receive.add_new(
                        definition_sequence, "SQ", Sequence([private_child])
                    )
                ),
            ),
            (
                "off-context-definition",
                lambda dataset: dataset.add_new(
                    definition_sequence, "SQ", Sequence([element()])
                ),
            ),
        )
        for case, mutate in cases:
            with self.subTest(case=case):
                self.assert_rejected(
                    self.conformant_dicom(
                        sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                        mutate=mutate,
                    )
                )

    def test_enhanced_surface_transmit_alias_output_is_atomic(self) -> None:
        sequence_tag = Tag(0x0018, 0x9049)

        def install(
            dataset,
            *,
            name="SURFACE",
            name_vr="SH",
            coil_type="SURFACE",
        ) -> Dataset:
            transmit = Dataset()
            transmit.add_new(Tag(0x0018, 0x1251), name_vr, name)
            transmit.add_new(Tag(0x0018, 0x9050), "LO", "")
            transmit.add_new(Tag(0x0018, 0x9051), "CS", coil_type)
            dataset.SharedFunctionalGroupsSequence[0].add_new(
                sequence_tag, "SQ", Sequence([transmit])
            )
            return transmit

        accepted = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=install,
        )
        self.assertEqual(self.audit(accepted).sop_class_uid, ENHANCED_MR_IMAGE_STORAGE_UID)

        def mutate_transmit(callback):
            def mutate(dataset) -> None:
                callback(dataset, install(dataset))

            return mutate

        cases = (
            ("source-alias", lambda dataset: install(dataset, name="S")),
            ("free-text", lambda dataset: install(dataset, name="SITE SURFACE")),
            ("wrong-vr", lambda dataset: install(dataset, name_vr="LO")),
            ("wrong-vm", lambda dataset: install(dataset, name=["SURFACE", "BODY"])),
            ("wrong-type", lambda dataset: install(dataset, coil_type="BODY")),
            (
                "extra-field",
                mutate_transmit(
                    lambda _dataset, transmit: transmit.add_new(
                        Tag(0x0018, 0x9052), "SH", "extra"
                    )
                ),
            ),
            (
                "off-context",
                lambda dataset: dataset.add_new(
                    sequence_tag,
                    "SQ",
                    Sequence(
                        [
                            Dataset()
                        ]
                    ),
                ),
            ),
        )
        for case, mutate in cases:
            with self.subTest(case=case):
                self.assert_rejected(
                    self.conformant_dicom(
                        sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                        mutate=mutate,
                    )
                )

    def test_native_pixel_data_length_must_match_declared_matrix(self) -> None:
        too_short = self.conformant_dicom(
            mutate=lambda dataset: setattr(dataset, "PixelData", b"\0" * 126)
        )
        self.assert_rejected(too_short)
        too_long = self.conformant_dicom(
            mutate=lambda dataset: setattr(dataset, "PixelData", b"\0" * 130)
        )
        self.assert_rejected(too_long)

    def test_complete_root_rescale_and_window_pairs_are_accepted(self) -> None:
        def complete(dataset) -> None:
            dataset.RescaleIntercept = "-1024"
            dataset.RescaleSlope = "1.25"
            dataset.RescaleType = "RELATIVE_UNITS"
            dataset.WindowCenter = ["40", "80"]
            dataset.WindowWidth = ["400", "800"]

        self.assertEqual(
            self.audit(self.conformant_dicom(mutate=complete)).sop_class_uid,
            CLASSIC_MR_IMAGE_STORAGE_UID,
        )

    def test_partial_or_unsafe_root_transforms_are_rejected(self) -> None:
        def partial_rescale(dataset) -> None:
            dataset.RescaleIntercept = "0"

        def zero_rescale_slope(dataset) -> None:
            dataset.RescaleIntercept = "0"
            dataset.RescaleSlope = "0"
            dataset.RescaleType = "US"

        def partial_window(dataset) -> None:
            dataset.WindowWidth = "100"

        def window_vm_mismatch(dataset) -> None:
            dataset.WindowCenter = ["40", "80"]
            dataset.WindowWidth = "400"

        def zero_window_width(dataset) -> None:
            dataset.WindowCenter = "40"
            dataset.WindowWidth = "0"

        def excessive_window_width(dataset) -> None:
            dataset.WindowCenter = "40"
            dataset.WindowWidth = "1e13"

        mutations = (
            partial_rescale,
            zero_rescale_slope,
            partial_window,
            window_vm_mismatch,
            zero_window_width,
            excessive_window_width,
        )
        for index, mutate in enumerate(mutations):
            with self.subTest(case=index):
                self.assert_rejected(self.conformant_dicom(mutate=mutate))

    def test_complete_atomic_pixel_value_transformation_is_accepted(self) -> None:
        def complete_pvt(dataset) -> None:
            item = Dataset()
            item.RescaleIntercept = "0"
            item.RescaleSlope = "1"
            item.RescaleType = "US"
            dataset.PixelValueTransformationSequence = Sequence([item])

        path = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=complete_pvt,
        )
        self.assertEqual(self.audit(path).sop_class_uid, ENHANCED_MR_IMAGE_STORAGE_UID)

    def test_complete_per_frame_voi_windows_are_accepted_in_the_standard_macro(
        self,
    ) -> None:
        def complete_frame_voi(dataset) -> None:
            for index, frame in enumerate(dataset.PerFrameFunctionalGroupsSequence):
                item = Dataset()
                item.WindowCenter = str(1019 + index)
                item.WindowWidth = "1772"
                frame.FrameVOILUTSequence = Sequence([item])

        path = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=complete_frame_voi,
        )
        self.assertEqual(self.audit(path).sop_class_uid, ENHANCED_MR_IMAGE_STORAGE_UID)

    def test_frame_voi_windows_are_exact_cardinality_and_context_bound(self) -> None:
        def item(*, width: bool = True) -> Dataset:
            result = Dataset()
            result.WindowCenter = "1019"
            if width:
                result.WindowWidth = "1772"
            return result

        def partial(dataset) -> None:
            dataset.PerFrameFunctionalGroupsSequence[0].FrameVOILUTSequence = Sequence(
                [item(width=False)]
            )

        def extra_explanation(dataset) -> None:
            value = item()
            value.WindowCenterWidthExplanation = "scanner display"
            dataset.PerFrameFunctionalGroupsSequence[0].FrameVOILUTSequence = Sequence(
                [value]
            )

        def semantic_function(dataset) -> None:
            value = item()
            value.VOILUTFunction = "SIGMOID"
            dataset.PerFrameFunctionalGroupsSequence[0].FrameVOILUTSequence = Sequence(
                [value]
            )

        def multiple_items(dataset) -> None:
            dataset.PerFrameFunctionalGroupsSequence[0].FrameVOILUTSequence = Sequence(
                [item(), item()]
            )

        def off_context_wrapper(dataset) -> None:
            dataset.FrameVOILUTSequence = Sequence([item()])

        def direct_nested_window(dataset) -> None:
            frame = dataset.PerFrameFunctionalGroupsSequence[0]
            frame.WindowCenter = "1019"
            frame.WindowWidth = "1772"

        for case, mutate in (
            ("partial", partial),
            ("extra-explanation", extra_explanation),
            ("semantic-function", semantic_function),
            ("multiple-items", multiple_items),
            ("off-context-wrapper", off_context_wrapper),
            ("direct-nested-window", direct_nested_window),
        ):
            with self.subTest(case=case):
                self.assert_rejected(
                    self.conformant_dicom(
                        sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                        mutate=mutate,
                    )
                )

    def test_enhanced_dimension_pointers_resolve_to_retained_public_targets(
        self,
    ) -> None:
        def sequence_pointer(dataset) -> None:
            dimension = dataset.DimensionIndexSequence[0]
            dimension.DimensionIndexPointer = Tag(0x0018, 0x9114)
            del dimension[Tag(0x0020, 0x9167)]
            del dataset.SharedFunctionalGroupsSequence[0].MREchoSequence
            for frame in dataset.PerFrameFunctionalGroupsSequence:
                echo = Dataset()
                echo.EffectiveEchoTime = 30.0
                frame.MREchoSequence = Sequence([echo])

        def root_pointer(dataset) -> None:
            dimension = dataset.DimensionIndexSequence[0]
            dimension.DimensionIndexPointer = Tag(0x0028, 0x0008)
            del dimension[Tag(0x0020, 0x9167)]

        for case, mutate in (
            ("functional-group-sequence", sequence_pointer),
            ("retained-root-attribute", root_pointer),
        ):
            with self.subTest(case=case):
                path = self.conformant_dicom(
                    sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                    mutate=mutate,
                )
                self.assertEqual(
                    self.audit(path).sop_class_uid, ENHANCED_MR_IMAGE_STORAGE_UID
                )

    def test_enhanced_dimension_pointers_reject_private_broken_and_zero_values(
        self,
    ) -> None:
        def private_index(dataset) -> None:
            dataset.DimensionIndexSequence[0].DimensionIndexPointer = Tag(
                0x0019, 0x1001
            )

        def private_group(dataset) -> None:
            dataset.DimensionIndexSequence[0].FunctionalGroupPointer = Tag(
                0x0019, 0x1001
            )

        def private_creator(dataset) -> None:
            dataset.DimensionIndexSequence[0].add_new(
                Tag(0x0020, 0x9213), "LO", "PRIVATE CREATOR"
            )

        def missing_target(dataset) -> None:
            dataset.DimensionIndexSequence[0].DimensionIndexPointer = Tag(
                0x0020, 0x9128
            )

        def zero_index(dataset) -> None:
            dataset.PerFrameFunctionalGroupsSequence[0].FrameContentSequence[
                0
            ].DimensionIndexValues = [0]

        for case, mutate in (
            ("private-index", private_index),
            ("private-group", private_group),
            ("private-creator", private_creator),
            ("missing-target", missing_target),
            ("zero-index", zero_index),
        ):
            with self.subTest(case=case):
                self.assert_rejected(
                    self.conformant_dicom(
                        sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                        mutate=mutate,
                    )
                )

    def test_unsupported_derived_reference_semantics_are_rejected(self) -> None:
        for tag in (
            Tag(0x0008, 0x9124),
            Tag(0x0008, 0x9215),
            Tag(0x0040, 0xA170),
        ):
            with self.subTest(tag=tag):

                def add_unsupported_reference(dataset, tag=tag) -> None:
                    dataset.add_new(tag, "SQ", Sequence([Dataset()]))

                self.assert_rejected(
                    self.conformant_dicom(
                        sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                        mutate=add_unsupported_reference,
                    )
                )

    def test_source_image_sequence_preserves_only_exact_pseudonymous_references(
        self,
    ) -> None:
        def reference_item(index: int = 99) -> Dataset:
            item = Dataset()
            item.add_new(Tag(0x0008, 0x1150), "UI", CLASSIC_MR_IMAGE_STORAGE_UID)
            item.add_new(
                Tag(0x0008, 0x1155),
                "UI",
                f"2.25.{100000000000000000000000000000000000 + index}",
            )
            return item

        accepted = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=lambda dataset: dataset.add_new(
                Tag(0x0008, 0x2112),
                "SQ",
                Sequence([reference_item(index) for index in range(1, 52)]),
            ),
        )
        self.assertEqual(
            self.audit(accepted).sop_class_uid, ENHANCED_MR_IMAGE_STORAGE_UID
        )

        def extra_frame_reference(dataset) -> None:
            item = reference_item()
            item.add_new(Tag(0x0008, 0x1160), "IS", "1")
            dataset.add_new(Tag(0x0008, 0x2112), "SQ", Sequence([item]))

        def missing_instance_reference(dataset) -> None:
            item = reference_item()
            del item[Tag(0x0008, 0x1155)]
            dataset.add_new(Tag(0x0008, 0x2112), "SQ", Sequence([item]))

        def nonstandard_class_reference(dataset) -> None:
            item = reference_item()
            item[
                Tag(0x0008, 0x1150)
            ].value = "2.25.100000000000000000000000000000000098"
            dataset.add_new(Tag(0x0008, 0x2112), "SQ", Sequence([item]))

        for mutate in (
            lambda dataset: dataset.add_new(Tag(0x0008, 0x2112), "SQ", Sequence([])),
            extra_frame_reference,
            missing_instance_reference,
            nonstandard_class_reference,
        ):
            self.assert_rejected(
                self.conformant_dicom(
                    sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                    mutate=mutate,
                )
            )

    def test_enhanced_shared_referenced_images_preserve_exact_localizer_links(
        self,
    ) -> None:
        referenced_image_sequence = Tag(0x0008, 0x1140)

        def purpose() -> Dataset:
            item = Dataset()
            item.add_new(Tag(0x0008, 0x0100), "SH", "121311")
            item.add_new(Tag(0x0008, 0x0102), "SH", "DCM")
            item.add_new(Tag(0x0008, 0x0104), "LO", "Localizer")
            item.add_new(Tag(0x0008, 0x0117), "UI", "1.2.840.10008.6.1.508")
            return item

        def reference(index: int) -> Dataset:
            item = Dataset()
            item.add_new(Tag(0x0008, 0x1150), "UI", ENHANCED_MR_IMAGE_STORAGE_UID)
            item.add_new(
                Tag(0x0008, 0x1155),
                "UI",
                f"2.25.{200000000000000000000000000000000000 + index}",
            )
            item.add_new(Tag(0x0008, 0x1160), "IS", str(index))
            item.add_new(Tag(0x0040, 0xA170), "SQ", Sequence([purpose()]))
            return item

        def add_shared(dataset) -> None:
            dataset.SharedFunctionalGroupsSequence[0].add_new(
                referenced_image_sequence,
                "SQ",
                Sequence([reference(index) for index in range(1, 4)]),
            )

        accepted = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=add_shared,
        )
        self.assertEqual(
            self.audit(accepted).sop_class_uid,
            ENHANCED_MR_IMAGE_STORAGE_UID,
        )

        def add_root(dataset) -> None:
            dataset.add_new(referenced_image_sequence, "SQ", Sequence([reference(1)]))

        def add_per_frame(dataset) -> None:
            dataset.PerFrameFunctionalGroupsSequence[0].add_new(
                referenced_image_sequence, "SQ", Sequence([reference(1)])
            )

        def add_off_context_purpose(dataset) -> None:
            dataset.add_new(Tag(0x0040, 0xA170), "SQ", Sequence([purpose()]))

        for case, mutate in (
            ("root-reference", add_root),
            ("per-frame-reference", add_per_frame),
            ("root-purpose", add_off_context_purpose),
        ):
            with self.subTest(case=case):
                self.assert_rejected(
                    self.conformant_dicom(
                        sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                        mutate=mutate,
                    )
                )

        self.assert_rejected(
            self.conformant_dicom(
                sop_class=LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
                mutate=add_shared,
            )
        )

    def test_classic_root_referenced_images_remain_an_exact_simple_contract(
        self,
    ) -> None:
        def reference() -> Dataset:
            item = Dataset()
            item.add_new(Tag(0x0008, 0x1150), "UI", CLASSIC_MR_IMAGE_STORAGE_UID)
            item.add_new(
                Tag(0x0008, 0x1155),
                "UI",
                "2.25.200000000000000000000000000000000001",
            )
            return item

        def install(item_mutator=None):
            def mutate(dataset) -> None:
                item = reference()
                if item_mutator is not None:
                    item_mutator(item)
                dataset.add_new(Tag(0x0008, 0x1140), "SQ", Sequence([item]))

            return mutate

        accepted = self.conformant_dicom(mutate=install())
        self.assertEqual(
            self.audit(accepted).sop_class_uid, CLASSIC_MR_IMAGE_STORAGE_UID
        )

        for case, item_mutator in (
            (
                "unremapped-instance",
                lambda item: item.add_new(
                    Tag(0x0008, 0x1155), "UI", CLASSIC_MR_IMAGE_STORAGE_UID
                ),
            ),
            (
                "extra-frame",
                lambda item: item.add_new(Tag(0x0008, 0x1160), "IS", "1"),
            ),
            (
                "off-context-purpose",
                lambda item: item.add_new(
                    Tag(0x0040, 0xA170), "SQ", Sequence([Dataset()])
                ),
            ),
        ):
            with self.subTest(case=case):
                self.assert_rejected(
                    self.conformant_dicom(mutate=install(item_mutator))
                )

        source_only_group_length = reference()
        source_only_group_length.add_new(Tag(0x0008, 0x0000), "UL", 100)
        with self.assertRaises(_PrivacyViolation):
            _audit_source_image_sequence(
                DataElement(
                    Tag(0x0008, 0x1140),
                    "SQ",
                    Sequence([source_only_group_length]),
                )
            )

    def test_enhanced_shared_referenced_images_reject_nonatomic_values(self) -> None:
        referenced_image_sequence = Tag(0x0008, 0x1140)
        class_uid = Tag(0x0008, 0x1150)
        instance_uid = Tag(0x0008, 0x1155)
        frame_number = Tag(0x0008, 0x1160)
        purpose_sequence = Tag(0x0040, 0xA170)

        def purpose() -> Dataset:
            item = Dataset()
            item.add_new(Tag(0x0008, 0x0100), "SH", "121311")
            item.add_new(Tag(0x0008, 0x0102), "SH", "DCM")
            item.add_new(Tag(0x0008, 0x0104), "LO", "Localizer")
            item.add_new(Tag(0x0008, 0x0117), "UI", "1.2.840.10008.6.1.508")
            return item

        def reference() -> Dataset:
            item = Dataset()
            item.add_new(class_uid, "UI", ENHANCED_MR_IMAGE_STORAGE_UID)
            item.add_new(
                instance_uid,
                "UI",
                "2.25.200000000000000000000000000000000001",
            )
            item.add_new(frame_number, "IS", "1")
            item.add_new(purpose_sequence, "SQ", Sequence([purpose()]))
            return item

        def install(item_mutator=None, *, sequence_vr: str = "SQ", empty=False):
            def mutate(dataset) -> None:
                item = reference()
                if item_mutator is not None:
                    item_mutator(item)
                value = Sequence([] if empty else [item])
                if sequence_vr == "SQ":
                    dataset.SharedFunctionalGroupsSequence[0].add_new(
                        referenced_image_sequence, sequence_vr, value
                    )
                else:
                    dataset.SharedFunctionalGroupsSequence[0].add_new(
                        referenced_image_sequence, sequence_vr, "invalid"
                    )

            return mutate

        def remove(tag):
            return lambda item: item.pop(tag)

        def replace(tag, vr: str, value):
            return lambda item: item.add_new(tag, vr, value)

        def mutate_code(tag, *, vr: str | None = None, value=None):
            def mutation(item) -> None:
                code = item[purpose_sequence].value[0]
                element = code[tag]
                code.add_new(tag, vr or element.VR, value)

            return mutation

        cases = [
            ("empty-sequence", install(empty=True)),
            ("sequence-vr", install(sequence_vr="LO")),
            ("missing-class", install(remove(class_uid))),
            ("missing-instance", install(remove(instance_uid))),
            ("missing-frame", install(remove(frame_number))),
            ("missing-purpose", install(remove(purpose_sequence))),
            (
                "extra-public-key",
                install(lambda item: item.add_new(Tag(0x0008, 0x2112), "SQ", [])),
            ),
            (
                "private-reference-key",
                install(lambda item: item.add_new(Tag(0x0019, 0x1001), "LO", "x")),
            ),
            (
                "class-vr",
                install(replace(class_uid, "LO", ENHANCED_MR_IMAGE_STORAGE_UID)),
            ),
            (
                "class-uid",
                install(replace(class_uid, "UI", CLASSIC_MR_IMAGE_STORAGE_UID)),
            ),
            (
                "instance-vr",
                install(
                    replace(
                        instance_uid,
                        "LO",
                        "2.25.200000000000000000000000000000000001",
                    )
                ),
            ),
            (
                "instance-uid",
                install(replace(instance_uid, "UI", ENHANCED_MR_IMAGE_STORAGE_UID)),
            ),
            (
                "instance-vm",
                install(
                    replace(
                        instance_uid,
                        "UI",
                        [
                            "2.25.200000000000000000000000000000000001",
                            "2.25.200000000000000000000000000000000002",
                        ],
                    )
                ),
            ),
            ("frame-vr", install(replace(frame_number, "DS", "1"))),
            ("frame-zero", install(replace(frame_number, "IS", "0"))),
            ("frame-too-large", install(replace(frame_number, "IS", "500001"))),
            ("frame-vm", install(replace(frame_number, "IS", ["1", "2"]))),
            ("purpose-vr", install(replace(purpose_sequence, "LO", "x"))),
            (
                "purpose-vm",
                install(
                    replace(
                        purpose_sequence,
                        "SQ",
                        Sequence([purpose(), purpose()]),
                    )
                ),
            ),
            (
                "missing-code-key",
                install(
                    lambda item: (
                        item[purpose_sequence].value[0].pop(Tag(0x0008, 0x0104))
                    )
                ),
            ),
            (
                "extra-code-key",
                install(
                    lambda item: (
                        item[purpose_sequence]
                        .value[0]
                        .add_new(Tag(0x0008, 0x0103), "SH", "1")
                    )
                ),
            ),
            (
                "private-code-key",
                install(
                    lambda item: (
                        item[purpose_sequence]
                        .value[0]
                        .add_new(Tag(0x0019, 0x1001), "LO", "x")
                    )
                ),
            ),
            (
                "code-vr",
                install(mutate_code(Tag(0x0008, 0x0100), vr="LO", value="121311")),
            ),
            (
                "code-vm",
                install(mutate_code(Tag(0x0008, 0x0100), value=["121311", "121312"])),
            ),
            (
                "code-value",
                install(mutate_code(Tag(0x0008, 0x0100), value="121312")),
            ),
            (
                "coding-scheme",
                install(mutate_code(Tag(0x0008, 0x0102), value="SRT")),
            ),
            (
                "code-meaning",
                install(mutate_code(Tag(0x0008, 0x0104), value="Anatomy")),
            ),
            (
                "context-uid",
                install(
                    mutate_code(Tag(0x0008, 0x0117), value="1.2.840.10008.6.1.509")
                ),
            ),
        ]
        for case, mutate in cases:
            with self.subTest(case=case):
                self.assert_rejected(
                    self.conformant_dicom(
                        sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                        mutate=mutate,
                    )
                )

    def test_current_enhanced_per_frame_metabolite_map_is_exact_water(self) -> None:
        sequence_tag = Tag(0x0018, 0x9152)
        description_tag = Tag(0x0018, 0x9080)

        def item(*, vr: str = "ST", value="WATER") -> Dataset:
            result = Dataset()
            result.add_new(description_tag, vr, value)
            return result

        def add_to_every_frame(dataset) -> None:
            for frame in dataset.PerFrameFunctionalGroupsSequence:
                frame.add_new(sequence_tag, "SQ", Sequence([item()]))

        accepted = self.conformant_dicom(
            sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
            mutate=add_to_every_frame,
        )
        self.assertEqual(
            self.audit(accepted).sop_class_uid,
            ENHANCED_MR_IMAGE_STORAGE_UID,
        )

        def per_frame_with(items, *, vr: str = "SQ"):
            def mutate(dataset) -> None:
                frame = dataset.PerFrameFunctionalGroupsSequence[0]
                frame.add_new(
                    sequence_tag,
                    vr,
                    Sequence(items) if vr == "SQ" else "WATER",
                )

            return mutate

        extra = item()
        extra.add_new(Tag(0x0018, 0x9081), "CS", "NO")
        private = item()
        private.add_new(Tag(0x0019, 0x1001), "LO", "hidden")

        def add_root_sequence(dataset) -> None:
            dataset.add_new(sequence_tag, "SQ", Sequence([item()]))

        def add_shared_sequence(dataset) -> None:
            dataset.SharedFunctionalGroupsSequence[0].add_new(
                sequence_tag, "SQ", Sequence([item()])
            )

        def add_root_description(dataset) -> None:
            dataset.add_new(description_tag, "ST", "WATER")

        cases = (
            ("root-sequence", add_root_sequence),
            ("shared-sequence", add_shared_sequence),
            ("root-description", add_root_description),
            ("sequence-vr", per_frame_with([item()], vr="LO")),
            ("empty-sequence", per_frame_with([])),
            ("two-items", per_frame_with([item(), item()])),
            ("missing-description", per_frame_with([Dataset()])),
            ("extra-public", per_frame_with([extra])),
            ("extra-private", per_frame_with([private])),
            ("description-vr", per_frame_with([item(vr="LO")])),
            ("description-vm", per_frame_with([item(value=["WATER", "FAT"])])),
            ("wrong-value", per_frame_with([item(value="FAT")])),
            ("free-text", per_frame_with([item(value="WATER participant")])),
        )
        for case, mutate in cases:
            with self.subTest(case=case):
                self.assert_rejected(
                    self.conformant_dicom(
                        sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                        mutate=mutate,
                    )
                )

        self.assert_rejected(
            self.conformant_dicom(
                sop_class=LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
                mutate=add_to_every_frame,
            )
        )

    def test_incomplete_pvt_and_recursive_rwvm_are_rejected(self) -> None:
        def incomplete_pvt(dataset) -> None:
            item = Dataset()
            item.RescaleSlope = "1"
            dataset.PixelValueTransformationSequence = Sequence([item])

        self.assert_rejected(
            self.conformant_dicom(
                sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                mutate=incomplete_pvt,
            )
        )

        def recursive_rwvm(dataset) -> None:
            mapping = Dataset()
            mapping.RealWorldValueSlope = 1.0
            functional_group = Dataset()
            functional_group.RealWorldValueMappingSequence = Sequence([mapping])
            dataset.SharedFunctionalGroupsSequence = Sequence([functional_group])

        self.assert_rejected(
            self.conformant_dicom(
                sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                mutate=recursive_rwvm,
            )
        )

    def test_lut_and_palette_transform_structures_are_rejected(self) -> None:
        def modality_lut(dataset) -> None:
            dataset.ModalityLUTSequence = Sequence([Dataset()])

        def palette_data(dataset) -> None:
            dataset.add_new(Tag(0x0028, 0x1201), "OW", b"\0\0")

        def palette_uid_surface(dataset) -> None:
            dataset.add_new(
                Tag(0x0028, 0x1114),
                "UI",
                "2.25.100000000000000000000000000000000099",
            )

        def wrong_vr_icc_surface(dataset) -> None:
            dataset.add_new(
                Tag(0x0028, 0x2000),
                "UI",
                "2.25.100000000000000000000000000000000099",
            )

        for mutate in (
            modality_lut,
            palette_data,
            palette_uid_surface,
            wrong_vr_icc_surface,
        ):
            self.assert_rejected(self.conformant_dicom(mutate=mutate))

    def test_type_two_privacy_shells_must_be_present_and_empty(self) -> None:
        for keyword in (
            "StudyDate",
            "AcquisitionDate",
            "ContentDate",
            "StudyTime",
            "AcquisitionTime",
            "ContentTime",
            "AccessionNumber",
            "ReferringPhysicianName",
            "PatientBirthDate",
            "PatientSex",
            "StudyID",
            "PositionReferenceIndicator",
        ):
            with self.subTest(keyword=keyword):
                self.assert_rejected(
                    self.conformant_dicom(
                        mutate=lambda dataset, keyword=keyword: dataset.pop(
                            dataset.data_element(keyword).tag
                        )
                    )
                )
        self.assert_rejected(
            self.conformant_dicom(
                mutate=lambda dataset: setattr(dataset, "PatientBirthDate", "19800101")
            )
        )

    def test_common_frame_of_reference_conformance_is_enforced(self) -> None:
        self.assert_rejected(
            self.conformant_dicom(
                mutate=lambda dataset: dataset.pop(
                    dataset.data_element("FrameOfReferenceUID").tag
                )
            )
        )
        self.assert_rejected(
            self.conformant_dicom(
                mutate=lambda dataset: setattr(
                    dataset, "FrameOfReferenceUID", "1.2.840.10008.1.2.3"
                )
            )
        )

    def test_classic_mr_type_one_and_type_two_modules_are_enforced(self) -> None:
        for keyword in ("ScanningSequence", "SequenceVariant"):
            with self.subTest(keyword=keyword):
                self.assert_rejected(
                    self.conformant_dicom(
                        mutate=lambda dataset, keyword=keyword: dataset.pop(
                            dataset.data_element(keyword).tag
                        )
                    )
                )
        for keyword in (
            "ScanOptions",
            "MRAcquisitionType",
            "EchoTime",
            "EchoTrainLength",
        ):
            with self.subTest(keyword=keyword):
                self.assert_rejected(
                    self.conformant_dicom(
                        mutate=lambda dataset, keyword=keyword: dataset.pop(
                            dataset.data_element(keyword).tag
                        )
                    )
                )

        def empty_type_two(dataset) -> None:
            dataset.ScanOptions = ""
            dataset.MRAcquisitionType = ""
            dataset.EchoTime = ""
            dataset.EchoTrainLength = ""

        self.assertEqual(
            self.audit(self.conformant_dicom(mutate=empty_type_two)).sop_class_uid,
            CLASSIC_MR_IMAGE_STORAGE_UID,
        )

    def test_enhanced_equipment_identity_is_required_and_pseudonymous(self) -> None:
        for keyword in (
            "Manufacturer",
            "ManufacturerModelName",
            "DeviceSerialNumber",
            "SoftwareVersions",
        ):
            with self.subTest(keyword=keyword):
                self.assert_rejected(
                    self.conformant_dicom(
                        sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                        mutate=lambda dataset, keyword=keyword: dataset.pop(
                            dataset.data_element(keyword).tag
                        ),
                    )
                )
        self.assert_rejected(
            self.conformant_dicom(
                sop_class=ENHANCED_MR_IMAGE_STORAGE_UID,
                mutate=lambda dataset: setattr(
                    dataset, "DeviceSerialNumber", "scanner-room-7"
                ),
            )
        )

    def test_numeric_type_two_shells_must_exist_but_may_be_empty(self) -> None:
        for keyword in ("SeriesNumber", "AcquisitionNumber", "InstanceNumber"):
            with self.subTest(keyword=keyword):
                self.assert_rejected(
                    self.conformant_dicom(
                        mutate=lambda dataset, keyword=keyword: dataset.pop(
                            dataset.data_element(keyword).tag
                        )
                    )
                )

        def empty_numbers(dataset) -> None:
            dataset.SeriesNumber = ""
            dataset.AcquisitionNumber = ""
            dataset.InstanceNumber = ""

        self.assertEqual(
            self.audit(self.conformant_dicom(mutate=empty_numbers)).series_number,
            None,
        )

    def test_unknown_current_scanners_and_common_uih_aliases_are_retained(self) -> None:
        for manufacturer in (
            "UIH",
            "UNITEDIMAGING",
            "UNITED IMAGING HEALTHCARE CO., LTD.",
        ):
            with self.subTest(manufacturer=manufacturer):

                def current_scanner(dataset, manufacturer=manufacturer) -> None:
                    dataset.Manufacturer = manufacturer
                    dataset.ManufacturerModelName = "uMR Ultra 2027"
                    dataset.SoftwareVersions = ["uMR Platform 9.2", "Recon_2027.1"]

                audit = self.audit(self.conformant_dicom(mutate=current_scanner))
                self.assertEqual(audit.manufacturer, manufacturer)
                self.assertEqual(audit.model, "uMR Ultra 2027")

        path = self.conformant_dicom(
            mutate=lambda dataset: setattr(dataset, "Manufacturer", "")
        )
        self.assertIsNone(self.audit(path).manufacturer)

    def test_malicious_scanner_text_and_wrong_type_vr_are_rejected(self) -> None:
        for value in (
            "<script>",
            "/etc/passwd",
            ".. / patient",
            "https://scanner.invalid",
            "operator@example.org",
            "C:/scanner",
            "Participant Scanner",
            "Scanner 1234567",
        ):
            with self.subTest(value=value):
                self.assert_rejected(
                    self.conformant_dicom(
                        mutate=lambda dataset, value=value: setattr(
                            dataset, "ManufacturerModelName", value
                        )
                    )
                )

        def wrong_vr(dataset) -> None:
            del dataset.Manufacturer
            dataset.add_new(Tag(0x0008, 0x0070), "SH", "UIH")

        self.assert_rejected(self.conformant_dicom(mutate=wrong_vr))

    def test_philips_private_scaling_is_atomic_or_uses_public_fallback(self) -> None:
        def private_conversion_without_scale(dataset) -> None:
            dataset.Manufacturer = "Philips Medical Systems"
            dataset.add_new(Tag(0x2001, 0x0010), "LO", "Philips Imaging DD 001")
            dataset.add_new(Tag(0x2001, 0x1018), "SL", 32)
            dataset.add_new(Tag(0x2001, 0x1022), "FL", 0.75)

        self.assert_rejected(
            self.conformant_dicom(mutate=private_conversion_without_scale)
        )

        def with_public_fallback(dataset) -> None:
            private_conversion_without_scale(dataset)
            dataset.RescaleIntercept = "0"
            dataset.RescaleSlope = "1"
            dataset.RescaleType = "US"

        self.assertEqual(
            self.audit(self.conformant_dicom(mutate=with_public_fallback)).manufacturer,
            "Philips Medical Systems",
        )

        def partial_private_pair(dataset) -> None:
            dataset.Manufacturer = "Philips Medical Systems"
            dataset.add_new(Tag(0x2005, 0x0010), "LO", "Philips MR Imaging DD 001")
            dataset.add_new(Tag(0x2005, 0x100E), "FL", 1.0)

        self.assert_rejected(self.conformant_dicom(mutate=partial_private_pair))

    def test_public_and_private_diffusion_values_must_match(self) -> None:
        def ge_sources(dataset, *, private_b=1000, private_x=1.0) -> None:
            dataset.add_new(Tag(0x0018, 0x9087), "FD", 1000.0)
            dataset.add_new(Tag(0x0018, 0x9075), "CS", "DIRECTIONAL")
            dataset.add_new(Tag(0x0018, 0x9089), "FD", [1.0, 0.0, 0.0])
            dataset.add_new(Tag(0x0043, 0x0010), "LO", "GEMS_PARM_01")
            dataset.add_new(Tag(0x0043, 0x1039), "IS", [private_b, 0, 0, 0])
            dataset.add_new(Tag(0x0019, 0x0010), "LO", "GEMS_ACQU_01")
            dataset.add_new(Tag(0x0019, 0x10BB), "DS", str(private_x))
            dataset.add_new(Tag(0x0019, 0x10BC), "DS", "0")
            dataset.add_new(Tag(0x0019, 0x10BD), "DS", "0")

        matching = self.audit(
            self.conformant_dicom(mutate=lambda dataset: ge_sources(dataset))
        )
        self.assertTrue(matching.diffusion_metadata_contract_verified)

        b_mismatch = self.audit(
            self.conformant_dicom(
                mutate=lambda dataset: ge_sources(dataset, private_b=1200)
            )
        )
        self.assertFalse(b_mismatch.diffusion_metadata_contract_verified)

        vector_mismatch = self.audit(
            self.conformant_dicom(
                mutate=lambda dataset: ge_sources(dataset, private_x=-1.0)
            )
        )
        self.assertFalse(vector_mismatch.diffusion_metadata_contract_verified)

    def test_public_and_private_diffusion_matrices_must_match(self) -> None:
        def matrix_sources(dataset, *, private_first: float) -> None:
            dataset.add_new(Tag(0x0018, 0x9087), "FD", 1000.0)
            dataset.add_new(Tag(0x0018, 0x9075), "CS", "BMATRIX")
            for offset, value in enumerate((1.0, 0.0, 0.0, 1.0, 0.0, 1.0)):
                dataset.add_new(Tag(0x0018, 0x9602 + offset), "FD", value)
            dataset.add_new(Tag(0x0019, 0x0010), "LO", "SIEMENS MR HEADER")
            dataset.add_new(Tag(0x0019, 0x100C), "IS", "1000")
            dataset.add_new(Tag(0x0019, 0x100D), "CS", "BMATRIX")
            dataset.add_new(
                Tag(0x0019, 0x1027),
                "FD",
                [private_first, 0.0, 0.0, 1.0, 0.0, 1.0],
            )

        matching = self.audit(
            self.conformant_dicom(
                mutate=lambda dataset: matrix_sources(dataset, private_first=1.0)
            )
        )
        self.assertTrue(matching.diffusion_metadata_contract_verified)
        mismatch = self.audit(
            self.conformant_dicom(
                mutate=lambda dataset: matrix_sources(dataset, private_first=2.0)
            )
        )
        self.assertFalse(mismatch.diffusion_metadata_contract_verified)

    def test_siemens_mr_header_and_csa_diffusion_values_must_match(self) -> None:
        def sources(dataset, *, csa_b: float) -> None:
            dataset[Tag(0x0029, 0x1010)].value = siemens_csa_diffusion(
                b_value=csa_b,
                gradient=(1.0, 0.0, 0.0),
            )
            dataset.add_new(Tag(0x0019, 0x0010), "LO", "SIEMENS MR HEADER")
            dataset.add_new(Tag(0x0019, 0x100C), "IS", "1000")
            dataset.add_new(Tag(0x0019, 0x100D), "CS", "DIRECTIONAL")
            dataset.add_new(Tag(0x0019, 0x100E), "FD", [1.0, 0.0, 0.0])

        matching = self.audit(
            self.conformant_dicom(mutate=lambda dataset: sources(dataset, csa_b=1000))
        )
        self.assertTrue(matching.diffusion_metadata_contract_verified)
        mismatch = self.audit(
            self.conformant_dicom(mutate=lambda dataset: sources(dataset, csa_b=1200))
        )
        self.assertFalse(mismatch.diffusion_metadata_contract_verified)

    def test_public_and_philips_asl_contexts_cannot_contradict(self) -> None:
        def asl_sources(
            dataset,
            *,
            public_context: str,
            crusher: str = "NO",
            bolus: str = "NO",
        ) -> None:
            dataset.ArterialSpinLabelingContrast = "PSEUDOCONTINUOUS"
            dataset.InversionTime = "1800"
            slab = Dataset()
            slab.add_new(Tag(0x0018, 0x9253), "US", 1)
            slab.add_new(Tag(0x0018, 0x9254), "FD", 100.0)
            slab.add_new(Tag(0x0018, 0x9255), "FD", [0.0, 0.0, 1.0])
            slab.add_new(Tag(0x0018, 0x9256), "FD", [0.0, 0.0, 0.0])
            slab.add_new(Tag(0x0018, 0x9258), "UL", 1000)
            item = Dataset()
            item.add_new(Tag(0x0018, 0x9252), "LO", "")
            item.add_new(Tag(0x0018, 0x9257), "CS", public_context)
            item.add_new(Tag(0x0018, 0x9259), "CS", crusher)
            if crusher == "YES":
                item.add_new(Tag(0x0018, 0x925A), "FD", 20.0)
                item.add_new(Tag(0x0018, 0x925B), "LO", "REDACTED")
            item.add_new(Tag(0x0018, 0x925C), "CS", bolus)
            if bolus == "YES":
                timing = Dataset()
                timing.add_new(Tag(0x0018, 0x925E), "LO", "")
                timing.add_new(Tag(0x0018, 0x925F), "UL", 1800)
                item.add_new(Tag(0x0018, 0x925D), "SQ", Sequence([timing]))
            item.add_new(Tag(0x0018, 0x9260), "SQ", Sequence([slab]))
            dataset.add_new(Tag(0x0018, 0x9251), "SQ", Sequence([item]))
            dataset.add_new(Tag(0x2005, 0x0010), "LO", "Philips MR Imaging DD 005")
            dataset.add_new(Tag(0x2005, 0x1029), "CS", "LABEL")

        matching = self.audit(
            self.conformant_dicom(
                mutate=lambda dataset: asl_sources(dataset, public_context="LABEL")
            )
        )
        self.assertTrue(matching.asl_metadata_contract_verified)
        self.assertEqual(matching.asl_technique_descriptions_emptied, 1)
        self.assertEqual(matching.asl_crusher_descriptions_redacted, 0)
        self.assertEqual(matching.asl_bolus_cutoff_techniques_emptied, 0)
        contradiction = self.audit(
            self.conformant_dicom(
                mutate=lambda dataset: asl_sources(dataset, public_context="CONTROL")
            )
        )
        self.assertFalse(contradiction.asl_metadata_contract_verified)

        positive_conditionals = self.audit(
            self.conformant_dicom(
                mutate=lambda dataset: asl_sources(
                    dataset,
                    public_context="LABEL",
                    crusher="YES",
                    bolus="YES",
                )
            )
        )
        self.assertTrue(positive_conditionals.asl_metadata_contract_verified)
        self.assertEqual(positive_conditionals.asl_technique_descriptions_emptied, 1)
        self.assertEqual(positive_conditionals.asl_crusher_descriptions_redacted, 1)
        self.assertEqual(positive_conditionals.asl_bolus_cutoff_techniques_emptied, 1)

        def source_crusher_text(dataset) -> None:
            asl_sources(dataset, public_context="LABEL", crusher="YES")
            item = dataset[Tag(0x0018, 0x9251)].value[0]
            item[Tag(0x0018, 0x925B)].value = "vendor free text"

        self.assert_rejected(self.conformant_dicom(mutate=source_crusher_text))

        def conditional_child_with_no_flag(dataset) -> None:
            asl_sources(dataset, public_context="LABEL")
            item = dataset[Tag(0x0018, 0x9251)].value[0]
            item.add_new(Tag(0x0018, 0x925A), "FD", 20.0)
            item.add_new(Tag(0x0018, 0x925B), "LO", "REDACTED")

        malformed = self.audit(
            self.conformant_dicom(mutate=conditional_child_with_no_flag)
        )
        self.assertFalse(malformed.asl_metadata_contract_verified)

        def incomplete_bolus_timing(dataset) -> None:
            asl_sources(dataset, public_context="LABEL", bolus="YES")
            timing = dataset[Tag(0x0018, 0x9251)].value[0][Tag(0x0018, 0x925D)].value[0]
            del timing[Tag(0x0018, 0x925E)]

        incomplete = self.audit(self.conformant_dicom(mutate=incomplete_bolus_timing))
        self.assertFalse(incomplete.asl_metadata_contract_verified)


if __name__ == "__main__":
    unittest.main()
