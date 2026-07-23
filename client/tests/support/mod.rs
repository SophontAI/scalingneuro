#![allow(dead_code)]

use std::{fs, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureVendor {
    Generic,
    Siemens,
    PhilipsClassic,
    PhilipsEnhanced,
    Ge,
    Uih,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixturePurpose {
    FunctionalEpi,
    StructuralT1w,
    StructuralT2w,
    StructuralOther,
    Diffusion,
    AslPerfusion,
    Fieldmap,
    Sbref,
    Localizer,
    DerivedMr,
    OtherMr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameVoiFixture {
    Absent,
    Valid,
    ValidWithExplanation,
    MissingWidth,
    ExtraAttribute,
    MultipleItems,
    OffContext,
    DirectNestedWindow,
    VoiLutFunction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DimensionIndexFixture {
    ValidAttributePointer,
    FunctionalGroupSequencePointer,
    RootAttributePointer,
    PrivateIndexPointer,
    PrivateGroupPointer,
    PrivateCreator,
    MissingTarget,
    ZeroIndexValue,
}

#[derive(Debug, Clone)]
pub struct FunctionalDicomOptions {
    pub burned_in_annotation: bool,
    pub omit_burned_in_annotation: bool,
    pub include_privacy_leaks: bool,
    pub hostile_free_text: bool,
    pub invalid_image_type: bool,
    pub unknown_optional_image_type: bool,
    pub pixel_bytes: usize,
    pub vendor: FixtureVendor,
    pub purpose: FixturePurpose,
    pub sop_class_override: Option<&'static str>,
    pub enhanced_image_type_override: Option<&'static str>,
    pub enhanced_frame_type_override: Option<&'static str>,
    pub enhanced_pulse_sequence_name_override: Option<&'static str>,
    pub model_override: Option<&'static str>,
    pub software_versions_override: Option<&'static str>,
    pub include_pixel_value_transform: bool,
    pub incomplete_pixel_value_transform: bool,
    pub include_real_world_value_mapping: bool,
    pub include_modality_lut: bool,
    pub frame_voi_lut: FrameVoiFixture,
    pub dimension_index: DimensionIndexFixture,
    pub asl_crusher: bool,
    pub omit_asl_crusher_description: bool,
    pub asl_bolus_cutoff: bool,
    pub siemens_mosaic: bool,
    /// Emit a Siemens CSA B-matrix rather than the mutually exclusive
    /// diffusion-gradient-direction representation.
    pub siemens_csa_b_matrix: bool,
    pub encapsulated_pixel_data: bool,
    pub extended_offset_table: bool,
    /// Emit a complete, internally consistent Philips classic dynamic-series
    /// timing contract. `instance` is interpreted as one-based slice-major
    /// acquisition order.
    pub philips_dynamic_timing: bool,
    pub philips_dynamic_timing_malformed: bool,
    pub philips_private_metadata_malformed: bool,
    pub philips_omit_private_scale_intercept: bool,
    pub philips_omit_private_scale_slope: bool,
    pub philips_omit_number_of_slices: bool,
    pub philips_omit_water_fat_shift: bool,
    pub philips_omit_public_pixel_scaling: bool,
    pub philips_diffusion_direction_override: Option<&'static str>,
    /// Emit the real-world broad `(2005,xx0F)` DD 005 per-frame container
    /// without a creator-mapped scale slope. It must be dropped, not mistaken
    /// for the narrow PS3.15 scale-slope exception.
    pub philips_non_scaling_per_frame_container: bool,
    /// Emit the complete standard rescale triplet found inside real Philips
    /// DD 005 per-frame scale items.
    pub philips_per_frame_standard_rescale: bool,
    /// Omit Rescale Type from that nested triplet to exercise fail-closed
    /// validation without making the reviewed private scale slope malformed.
    pub philips_per_frame_incomplete_standard_rescale: bool,
    pub ge_asl_private_metadata: bool,
    pub philips_trigger_offset_ms: f64,
    pub philips_temporal_positions: u32,
    pub philips_slices: u32,
}

impl Default for FunctionalDicomOptions {
    fn default() -> Self {
        Self {
            burned_in_annotation: false,
            omit_burned_in_annotation: false,
            include_privacy_leaks: false,
            hostile_free_text: false,
            invalid_image_type: false,
            unknown_optional_image_type: false,
            pixel_bytes: 64 * 64 * 2,
            vendor: FixtureVendor::Siemens,
            purpose: FixturePurpose::FunctionalEpi,
            sop_class_override: None,
            enhanced_image_type_override: None,
            enhanced_frame_type_override: None,
            enhanced_pulse_sequence_name_override: None,
            model_override: None,
            software_versions_override: None,
            include_pixel_value_transform: false,
            incomplete_pixel_value_transform: false,
            include_real_world_value_mapping: false,
            include_modality_lut: false,
            frame_voi_lut: FrameVoiFixture::Absent,
            dimension_index: DimensionIndexFixture::ValidAttributePointer,
            asl_crusher: false,
            omit_asl_crusher_description: false,
            asl_bolus_cutoff: false,
            siemens_mosaic: true,
            siemens_csa_b_matrix: false,
            encapsulated_pixel_data: false,
            extended_offset_table: false,
            philips_dynamic_timing: false,
            philips_dynamic_timing_malformed: false,
            philips_private_metadata_malformed: false,
            philips_omit_private_scale_intercept: false,
            philips_omit_private_scale_slope: false,
            philips_omit_number_of_slices: false,
            philips_omit_water_fat_shift: false,
            philips_omit_public_pixel_scaling: false,
            philips_diffusion_direction_override: None,
            philips_non_scaling_per_frame_container: false,
            philips_per_frame_standard_rescale: false,
            philips_per_frame_incomplete_standard_rescale: false,
            ge_asl_private_metadata: false,
            philips_trigger_offset_ms: 0.0,
            philips_temporal_positions: 3,
            philips_slices: 32,
        }
    }
}

/// Build a tiny standards-shaped Explicit VR Little Endian DICOM fixture.
/// All identifiers are synthetic and reserved for automated tests.
pub fn write_functional_epi(path: &Path, instance: u32) {
    write_functional_epi_fixture(path, instance, &FunctionalDicomOptions::default());
}

#[allow(dead_code)]
pub fn write_functional_epi_with_burned_annotation(path: &Path, instance: u32) {
    write_functional_epi_fixture(
        path,
        instance,
        &FunctionalDicomOptions {
            burned_in_annotation: true,
            ..Default::default()
        },
    );
}

pub fn write_functional_epi_fixture(path: &Path, instance: u32, options: &FunctionalDicomOptions) {
    let study_uid = "1.2.826.0.1.3680043.10.999.1";
    let series_uid = "1.2.826.0.1.3680043.10.999.1.1";
    let sop_uid = format!("1.2.826.0.1.3680043.10.999.1.1.{instance}");
    let siemens_mosaic = matches!(
        options.purpose,
        FixturePurpose::FunctionalEpi | FixturePurpose::Diffusion
    ) && options.vendor == FixtureVendor::Siemens
        && options.siemens_mosaic;
    let sop_class = options.sop_class_override.unwrap_or(match options.vendor {
        FixtureVendor::PhilipsEnhanced => "1.2.840.10008.5.1.4.1.1.4.1",
        FixtureVendor::Generic
        | FixtureVendor::Siemens
        | FixtureVendor::PhilipsClassic
        | FixtureVendor::Ge
        | FixtureVendor::Uih => "1.2.840.10008.5.1.4.1.1.4",
    });
    let color_pixels = sop_class == "1.2.840.10008.5.1.4.1.1.4.3";
    let enhanced_storage = matches!(
        sop_class,
        "1.2.840.10008.5.1.4.1.1.4.1"
            | "1.2.840.10008.5.1.4.1.1.4.3"
            | "1.2.840.10008.5.1.4.1.1.4.4"
    );
    let current_enhanced_mr = sop_class == "1.2.840.10008.5.1.4.1.1.4.1";
    let legacy_converted_mr = sop_class == "1.2.840.10008.5.1.4.1.1.4.4";
    let enhanced_origin = options
        .enhanced_image_type_override
        .unwrap_or_else(|| enhanced_frame_type(options.purpose))
        .split('\\')
        .next()
        .unwrap_or_default();
    let current_original_or_mixed =
        current_enhanced_mr && matches!(enhanced_origin, "ORIGINAL" | "MIXED");
    let native_frames = if enhanced_storage { 12_usize } else { 1 };
    let native_samples = if color_pixels { 3_usize } else { 1 };
    let native_bytes_per_sample = if color_pixels { 1_usize } else { 2 };
    let native_pixel_bytes = if enhanced_storage
        && !options.encapsulated_pixel_data
        && !options.extended_offset_table
        && options.pixel_bytes == 64 * 64 * 2
    {
        native_frames * 64 * 64 * native_samples * native_bytes_per_sample
    } else {
        options.pixel_bytes
    };
    let (fixture_rows, fixture_columns) =
        if options.encapsulated_pixel_data || options.extended_offset_table {
            (64_u16, 64_u16)
        } else {
            fixture_native_pixel_matrix(
                native_pixel_bytes,
                native_frames,
                native_samples,
                native_bytes_per_sample,
            )
        };
    let manufacturer = if options.hostile_free_text {
        "DR PAUL SCOTT MRN 12345"
    } else {
        match options.vendor {
            FixtureVendor::Generic => "FIXTURE_VENDOR",
            FixtureVendor::Siemens => "SIEMENS",
            FixtureVendor::PhilipsClassic | FixtureVendor::PhilipsEnhanced => {
                "Philips Medical Systems"
            }
            FixtureVendor::Ge => "GE MEDICAL SYSTEMS",
            FixtureVendor::Uih => "United Imaging Healthcare",
        }
    };
    let model = if options.hostile_free_text {
        "MAGNETOM Prisma / Paul Scott"
    } else if let Some(model) = options.model_override {
        model
    } else {
        match options.vendor {
            FixtureVendor::Generic => "FIXTURE_MODEL",
            FixtureVendor::Siemens => "Prisma_fit",
            FixtureVendor::PhilipsClassic | FixtureVendor::PhilipsEnhanced => "Achieva dStream",
            FixtureVendor::Ge => "Discovery MR750",
            FixtureVendor::Uih => "uMR 790",
        }
    };

    let mut meta_body = Vec::new();
    element(&mut meta_body, 0x0002, 0x0001, b"OB", &[0, 1]);
    text_element(&mut meta_body, 0x0002, 0x0002, b"UI", sop_class);
    text_element(&mut meta_body, 0x0002, 0x0003, b"UI", &sop_uid);
    text_element(
        &mut meta_body,
        0x0002,
        0x0010,
        b"UI",
        if options.encapsulated_pixel_data || options.extended_offset_table {
            "1.2.840.10008.1.2.4.50"
        } else {
            "1.2.840.10008.1.2.1"
        },
    );
    text_element(
        &mut meta_body,
        0x0002,
        0x0012,
        b"UI",
        "1.2.826.0.1.3680043.10.999.2",
    );

    let mut bytes = vec![0_u8; 128];
    bytes.extend_from_slice(b"DICM");
    ul_element(&mut bytes, 0x0002, 0x0000, meta_body.len() as u32);
    bytes.extend(meta_body);
    text_element(
        &mut bytes,
        0x0008,
        0x0008,
        b"CS",
        if options.invalid_image_type {
            "ORIGINAL\\UNKNOWN_PATIENT\\M\\EPI"
        } else if options.unknown_optional_image_type {
            "ORIGINAL\\PRIMARY\\UNKNOWN_PATIENT\\EPI"
        } else if matches!(
            sop_class,
            "1.2.840.10008.5.1.4.1.1.4.1"
                | "1.2.840.10008.5.1.4.1.1.4.3"
                | "1.2.840.10008.5.1.4.1.1.4.4"
        ) {
            options
                .enhanced_image_type_override
                .unwrap_or_else(|| enhanced_frame_type(options.purpose))
        } else if options.vendor == FixtureVendor::Uih {
            fixture_uih_image_type(options.purpose)
        } else {
            fixture_image_type(options.purpose, siemens_mosaic)
        },
    );
    text_element(&mut bytes, 0x0008, 0x0016, b"UI", sop_class);
    text_element(&mut bytes, 0x0008, 0x0018, b"UI", &sop_uid);
    if enhanced_storage {
        text_element(&mut bytes, 0x0008, 0x0023, b"DA", "20260718");
        text_element(&mut bytes, 0x0008, 0x0033, b"TM", "120000");
    }
    if options.include_privacy_leaks {
        text_element(&mut bytes, 0x0008, 0x0020, b"DA", "20260718");
    }
    text_element(&mut bytes, 0x0008, 0x0060, b"CS", "MR");
    text_element(&mut bytes, 0x0008, 0x0070, b"LO", manufacturer);
    if options.include_privacy_leaks {
        text_element(&mut bytes, 0x0008, 0x0080, b"LO", "FIXTURE SECRET HOSPITAL");
        text_element(&mut bytes, 0x0008, 0x0090, b"PN", "FIXTURE^PHYSICIAN");
    }
    text_element(
        &mut bytes,
        0x0008,
        0x103E,
        b"LO",
        fixture_series_description(options.purpose),
    );
    text_element(&mut bytes, 0x0008, 0x1090, b"LO", model);
    if enhanced_storage {
        text_element(&mut bytes, 0x0008, 0x9205, b"CS", "MONOCHROME");
        text_element(&mut bytes, 0x0008, 0x9206, b"CS", "VOLUME");
        text_element(&mut bytes, 0x0008, 0x9207, b"CS", "NONE");
        text_element(&mut bytes, 0x0008, 0x9208, b"CS", "MAGNITUDE");
        text_element(
            &mut bytes,
            0x0008,
            0x9209,
            b"CS",
            match options.purpose {
                FixturePurpose::StructuralT1w => "T1",
                FixturePurpose::StructuralT2w => "T2",
                FixturePurpose::Diffusion => "DIFFUSION",
                FixturePurpose::AslPerfusion => "PERFUSION",
                FixturePurpose::Fieldmap => "T2_STAR",
                _ => "UNKNOWN",
            },
        );
    }
    if options.include_privacy_leaks && !enhanced_storage {
        let mut referenced_item = Vec::new();
        // Keep reference semantics independently conformant while the fixture's
        // root identity fields exercise de-identification. Reference items are
        // now an atomic scientific contract, not a generic PHI scrub container.
        text_element(&mut referenced_item, 0x0008, 0x1150, b"UI", sop_class);
        text_element(&mut referenced_item, 0x0008, 0x1155, b"UI", &sop_uid);
        sequence_element(&mut bytes, 0x0008, 0x1140, &[referenced_item]);
    }
    // Synthetic values only: tests must never depend on real patient data.
    text_element(&mut bytes, 0x0010, 0x0010, b"PN", "FIXTURE^SUBJECT");
    text_element(&mut bytes, 0x0010, 0x0020, b"LO", "FIXTURE-SUBJECT-001");
    text_element(
        &mut bytes,
        0x0018,
        0x0020,
        b"CS",
        fixture_scanning_sequence(options.purpose),
    );
    text_element(&mut bytes, 0x0018, 0x0021, b"CS", "NONE");
    text_element(&mut bytes, 0x0018, 0x0022, b"CS", "");
    text_element(
        &mut bytes,
        0x0018,
        0x0023,
        b"CS",
        if matches!(
            options.purpose,
            FixturePurpose::StructuralT1w
                | FixturePurpose::StructuralT2w
                | FixturePurpose::StructuralOther
        ) {
            "3D"
        } else {
            "2D"
        },
    );
    if current_original_or_mixed {
        text_element(
            &mut bytes,
            0x0018,
            0x9005,
            b"SH",
            options
                .enhanced_pulse_sequence_name_override
                .unwrap_or("ep2d"),
        );
        text_element(&mut bytes, 0x0018, 0x9008, b"CS", "GRADIENT");
        text_element(&mut bytes, 0x0018, 0x9012, b"CS", "NO");
        text_element(&mut bytes, 0x0018, 0x9014, b"CS", "NO");
        text_element(&mut bytes, 0x0018, 0x9015, b"CS", "NO");
        text_element(&mut bytes, 0x0018, 0x9017, b"CS", "NONE");
        text_element(&mut bytes, 0x0018, 0x9018, b"CS", "YES");
        text_element(&mut bytes, 0x0018, 0x9024, b"CS", "NO");
        text_element(&mut bytes, 0x0018, 0x9025, b"CS", "NONE");
        text_element(&mut bytes, 0x0018, 0x9029, b"CS", "NONE");
        text_element(&mut bytes, 0x0018, 0x9032, b"CS", "RECTILINEAR");
        text_element(&mut bytes, 0x0018, 0x9033, b"CS", "SINGLE");
        text_element(&mut bytes, 0x0018, 0x9034, b"CS", "LINEAR");
        us_element(&mut bytes, 0x0018, 0x9093, 1);
    }
    text_element(
        &mut bytes,
        0x0018,
        0x0024,
        b"SH",
        if options.hostile_free_text {
            "PAUL_MRN_12345"
        } else {
            fixture_sequence_name(options.purpose)
        },
    );
    if options.vendor != FixtureVendor::PhilipsEnhanced {
        text_element(&mut bytes, 0x0018, 0x0080, b"DS", "800");
        text_element(&mut bytes, 0x0018, 0x0081, b"DS", "30");
    }
    text_element(&mut bytes, 0x0018, 0x0091, b"IS", "");
    if options.philips_dynamic_timing {
        let slices = options.philips_slices.max(1);
        let temporal_index = (instance.saturating_sub(1)) / slices;
        text_element(
            &mut bytes,
            0x0018,
            0x1060,
            b"DS",
            &(f64::from(temporal_index) * 800.0 + options.philips_trigger_offset_ms).to_string(),
        );
    }
    text_element(&mut bytes, 0x0018, 0x0087, b"DS", "3");
    if options.purpose == FixturePurpose::Diffusion
        && options.vendor != FixtureVendor::PhilipsEnhanced
    {
        fd_element(&mut bytes, 0x0018, 0x9087, 1_000.0);
        text_element(&mut bytes, 0x0018, 0x9075, b"CS", "DIRECTIONAL");
        fd_values_element(&mut bytes, 0x0018, 0x9089, &[1.0, 0.0, 0.0]);
    }
    if options.purpose == FixturePurpose::AslPerfusion {
        text_element(&mut bytes, 0x0018, 0x9250, b"CS", "PSEUDOCONTINUOUS");
    }
    let software_versions = if options.hostile_free_text {
        "MRN12345 Paul Scott"
    } else if let Some(software_versions) = options.software_versions_override {
        software_versions
    } else {
        match options.vendor {
            FixtureVendor::Generic => "fixture version",
            FixtureVendor::Siemens => "syngo MR E11",
            FixtureVendor::PhilipsClassic | FixtureVendor::PhilipsEnhanced => "5.1.1\\5.1.1.0",
            FixtureVendor::Ge => "DV26.0",
            FixtureVendor::Uih => "R006",
        }
    };
    text_element(&mut bytes, 0x0018, 0x1000, b"LO", "FIXTURE-SERIAL-001");
    text_element(&mut bytes, 0x0018, 0x1020, b"LO", software_versions);
    text_element(
        &mut bytes,
        0x0018,
        0x1030,
        b"LO",
        fixture_protocol_name(options.purpose),
    );
    text_element(
        &mut bytes,
        0x0018,
        0x1250,
        b"SH",
        if options.hostile_free_text {
            "PAUL_SCOTT_MRN123"
        } else {
            match options.vendor {
                FixtureVendor::Generic => "FIXTURE_COIL",
                FixtureVendor::Siemens => "HeadNeck_64",
                FixtureVendor::PhilipsClassic | FixtureVendor::PhilipsEnhanced => {
                    "dStream Head 32ch"
                }
                FixtureVendor::Ge => "HNU 32",
                FixtureVendor::Uih => "UIH Head 32",
            }
        },
    );
    match options.vendor {
        FixtureVendor::Siemens => {
            text_element(&mut bytes, 0x0019, 0x0010, b"LO", "SIEMENS MR HEADER");
            us_element(&mut bytes, 0x0019, 0x100A, 42);
            if options.purpose == FixturePurpose::Diffusion {
                text_element(&mut bytes, 0x0019, 0x100C, b"IS", "1000");
                if options.siemens_csa_b_matrix {
                    text_element(&mut bytes, 0x0019, 0x100D, b"CS", "BMATRIX");
                    fd_values_element(
                        &mut bytes,
                        0x0019,
                        0x1027,
                        &[1000.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                    );
                } else {
                    text_element(&mut bytes, 0x0019, 0x100D, b"CS", "DIRECTIONAL");
                    fd_values_element(&mut bytes, 0x0019, 0x100E, &[1.0, 0.0, 0.0]);
                }
            }
            element(
                &mut bytes,
                0x0019,
                0x1010,
                b"OB",
                b"FIXTURE PRIVATE CSA TEXT LEAK",
            );
            if siemens_mosaic {
                text_element(&mut bytes, 0x0029, 0x0010, b"LO", "SIEMENS CSA HEADER");
                element(
                    &mut bytes,
                    0x0029,
                    0x1010,
                    b"OB",
                    &siemens_mosaic_csa_fixture(
                        options.purpose == FixturePurpose::Diffusion,
                        options.siemens_csa_b_matrix,
                    ),
                );
            }
        }
        FixtureVendor::PhilipsClassic | FixtureVendor::PhilipsEnhanced => {
            text_element(
                &mut bytes,
                0x0019,
                0x0010,
                b"LO",
                "PHILIPS MR IMAGING DD 001",
            );
            text_element(&mut bytes, 0x0019, 0x100A, b"DS", "2.5");
            element(
                &mut bytes,
                0x0019,
                0x1010,
                b"UN",
                b"FIXTURE PHILIPS PRIVATE TEXT LEAK",
            );
            text_element(
                &mut bytes,
                0x2005,
                0x0010,
                b"LO",
                "Philips MR Imaging DD 001",
            );
            if !options.philips_omit_private_scale_intercept {
                fl_element(&mut bytes, 0x2005, 0x100D, 0.0);
            }
            if !options.philips_omit_private_scale_slope {
                if options.philips_private_metadata_malformed {
                    text_element(&mut bytes, 0x2005, 0x100E, b"LO", "not-a-scale");
                } else {
                    fl_element(&mut bytes, 0x2005, 0x100E, 0.00363177);
                }
            }
            if options.philips_dynamic_timing {
                let slices = options.philips_slices.max(1);
                let temporal_index = (instance.saturating_sub(1)) / slices;
                if options.philips_dynamic_timing_malformed {
                    text_element(&mut bytes, 0x2005, 0x10A0, b"LO", "not-a-time");
                } else {
                    fl_element(&mut bytes, 0x2005, 0x10A0, temporal_index as f32 * 0.8);
                }
            }
            // A numeric-looking private neighbor is not allowlisted.
            fl_element(&mut bytes, 0x2005, 0x100F, 20260718.0);
            text_element(&mut bytes, 0x2001, 0x0010, b"LO", "Philips Imaging DD 001");
            if options.purpose == FixturePurpose::Diffusion {
                fl_element(&mut bytes, 0x2001, 0x1003, 1000.0);
                text_element(
                    &mut bytes,
                    0x2001,
                    0x1004,
                    b"CS",
                    options.philips_diffusion_direction_override.unwrap_or("AP"),
                );
                text_element(&mut bytes, 0x2001, 0x1008, b"IS", "1");
                fl_element(&mut bytes, 0x2005, 0x10B0, 1.0);
                fl_element(&mut bytes, 0x2005, 0x10B1, 0.0);
                fl_element(&mut bytes, 0x2005, 0x10B2, 0.0);
            }
            if !options.philips_omit_number_of_slices {
                sl_element(&mut bytes, 0x2001, 0x1018, options.philips_slices as i32);
            }
            if !options.philips_omit_water_fat_shift {
                fl_element(&mut bytes, 0x2001, 0x1022, 0.75);
            }
            sl_element(&mut bytes, 0x2001, 0x1019, 20_260_718);
            text_element(
                &mut bytes,
                0x2005,
                0x0014,
                b"LO",
                "Philips MR Imaging DD 005",
            );
            if options.purpose == FixturePurpose::Diffusion {
                text_element(&mut bytes, 0x2005, 0x1412, b"IS", "7");
                text_element(&mut bytes, 0x2005, 0x1413, b"IS", "11");
                // Same reviewed creator block, but not an allowlisted semantic.
                fl_element(&mut bytes, 0x2005, 0x14B3, 20_260_718.0);
            }
            if options.purpose == FixturePurpose::AslPerfusion
                && options.vendor == FixtureVendor::PhilipsClassic
            {
                // Alias is intentionally canonicalized to LABEL in the archive.
                text_element(&mut bytes, 0x2005, 0x1429, b"CS", "LBL");
                text_element(
                    &mut bytes,
                    0x2005,
                    0x142A,
                    b"LO",
                    "FIXTURE PRIVATE ASL LEAK",
                );
            }
            let mut scale_item = Vec::new();
            if options.philips_per_frame_standard_rescale {
                text_element(&mut scale_item, 0x0028, 0x1052, b"DS", "0");
                text_element(&mut scale_item, 0x0028, 0x1053, b"DS", "0.76947496947496");
                if !options.philips_per_frame_incomplete_standard_rescale {
                    text_element(&mut scale_item, 0x0028, 0x1054, b"LO", "US");
                }
            }
            if options.philips_non_scaling_per_frame_container {
                text_element(
                    &mut scale_item,
                    0x2005,
                    0x0010,
                    b"LO",
                    "Philips MR Imaging DD 001",
                );
                text_element(
                    &mut scale_item,
                    0x2005,
                    0x0014,
                    b"LO",
                    "Philips MR Imaging DD 005",
                );
                text_element(
                    &mut scale_item,
                    0x0008,
                    0x002A,
                    b"DT",
                    "20000101120000.000000",
                );
                text_element(&mut scale_item, 0x0018, 0x9018, b"CS", "YES");
                // Real Philips classic DD 005 per-frame containers may carry
                // this public LUT label even when they contain no quantitative
                // Real World Value Mapping. The client permits this exact
                // source-only value and drops it with the unreviewed private
                // container.
                text_element(&mut scale_item, 0x0040, 0x9210, b"SH", "Philips");
            } else {
                text_element(
                    &mut scale_item,
                    0x2005,
                    0x0010,
                    b"LO",
                    "Philips MR Imaging DD 001",
                );
                fl_element(&mut scale_item, 0x2005, 0x100E, 0.00363177);
                fl_element(&mut scale_item, 0x2005, 0x100F, 20_260_718.0);
                text_element(
                    &mut scale_item,
                    0x0010,
                    0x0010,
                    b"PN",
                    "NESTED^PRIVATE^LEAK",
                );
            }
            sequence_element(&mut bytes, 0x2005, 0x140F, &[scale_item]);
        }
        FixtureVendor::Ge => {
            if options.purpose == FixturePurpose::Diffusion {
                text_element(&mut bytes, 0x0043, 0x0010, b"LO", "GEMS_PARM_01");
                text_element(&mut bytes, 0x0043, 0x1039, b"IS", "1000\\0\\0\\0");
                text_element(&mut bytes, 0x0019, 0x0010, b"LO", "GEMS_ACQU_01");
                text_element(&mut bytes, 0x0019, 0x10BB, b"DS", "1");
                text_element(&mut bytes, 0x0019, 0x10BC, b"DS", "0.0");
                text_element(&mut bytes, 0x0019, 0x10BD, b"DS", "0");
                text_element(
                    &mut bytes,
                    0x0019,
                    0x10BE,
                    b"LO",
                    "FIXTURE PRIVATE VECTOR LEAK",
                );
                // Same private block, but not part of the PS3.15 safe list.
                text_element(&mut bytes, 0x0043, 0x1040, b"LO", "FIXTURE PRIVATE LEAK");
            }
            if options.ge_asl_private_metadata {
                text_element(&mut bytes, 0x0043, 0x0010, b"LO", "GEMS_PARM_01");
                text_element(&mut bytes, 0x0043, 0x10A3, b"CS", "PSEUDOCONTINUOUS");
                text_element(&mut bytes, 0x0043, 0x10A5, b"IS", "1800");
                text_element(
                    &mut bytes,
                    0x0043,
                    0x10A6,
                    b"LO",
                    "FIXTURE PRIVATE ASL LEAK",
                );
            }
        }
        FixtureVendor::Uih => {
            text_element(&mut bytes, 0x0065, 0x0010, b"LO", "Image Private Header");
            text_element(&mut bytes, 0x0065, 0x1050, b"DS", "42.0");
            // Same creator block, but not a reviewed semantic.
            text_element(
                &mut bytes,
                0x0065,
                0x1051,
                b"LO",
                "FIXTURE PRIVATE GRID LEAK",
            );
            if options.purpose == FixturePurpose::Diffusion {
                fd_element(&mut bytes, 0x0065, 0x1009, 1_000.0);
                fd_values_element(&mut bytes, 0x0065, 0x1037, &[1.0, 0.0, 0.0]);
                fd_element(&mut bytes, 0x0065, 0x1038, 20_260_718.0);
            }
        }
        FixtureVendor::Generic => {}
    }
    text_element(&mut bytes, 0x0020, 0x000D, b"UI", study_uid);
    text_element(&mut bytes, 0x0020, 0x000E, b"UI", series_uid);
    text_element(
        &mut bytes,
        0x0020,
        0x0052,
        b"UI",
        "1.2.826.0.1.3680043.10.999.1.5",
    );
    text_element(&mut bytes, 0x0020, 0x0011, b"IS", "7");
    if options.philips_dynamic_timing {
        let slices = options.philips_slices.max(1);
        let temporal_id = (instance.saturating_sub(1)) / slices + 1;
        let slice_index = (instance.saturating_sub(1)) % slices;
        text_element(&mut bytes, 0x0020, 0x0012, b"IS", &temporal_id.to_string());
        text_element(
            &mut bytes,
            0x0020,
            0x0032,
            b"DS",
            &format!("0\\0\\{}", f64::from(slice_index) * 3.0),
        );
        text_element(&mut bytes, 0x0020, 0x0100, b"IS", &temporal_id.to_string());
    }
    text_element(&mut bytes, 0x0020, 0x0013, b"IS", &instance.to_string());
    if options.purpose == FixturePurpose::FunctionalEpi || options.philips_dynamic_timing {
        text_element(
            &mut bytes,
            0x0020,
            0x0105,
            b"IS",
            &if options.philips_dynamic_timing {
                options.philips_temporal_positions.to_string()
            } else {
                "300".to_owned()
            },
        );
    }
    if enhanced_storage {
        let dimension_organization_uid = "1.2.826.0.1.3680043.10.999.1.7";
        text_element(
            &mut bytes,
            0x0020,
            0x9164,
            b"UI",
            dimension_organization_uid,
        );
        let mut organization_item = Vec::new();
        text_element(
            &mut organization_item,
            0x0020,
            0x9164,
            b"UI",
            dimension_organization_uid,
        );
        sequence_element(&mut bytes, 0x0020, 0x9221, &[organization_item]);
        let mut dimension_item = Vec::new();
        text_element(
            &mut dimension_item,
            0x0020,
            0x9164,
            b"UI",
            dimension_organization_uid,
        );
        let (index_pointer, group_pointer) = match options.dimension_index {
            DimensionIndexFixture::FunctionalGroupSequencePointer => ((0x0018, 0x9112), None),
            DimensionIndexFixture::RootAttributePointer => ((0x0028, 0x0008), None),
            DimensionIndexFixture::PrivateIndexPointer => {
                ((0x0019, 0x1001), Some((0x0020, 0x9111)))
            }
            DimensionIndexFixture::PrivateGroupPointer => {
                ((0x0020, 0x9057), Some((0x0019, 0x1001)))
            }
            DimensionIndexFixture::MissingTarget => ((0x0020, 0x9128), Some((0x0020, 0x9111))),
            _ => ((0x0020, 0x9057), Some((0x0020, 0x9111))),
        };
        at_element(
            &mut dimension_item,
            0x0020,
            0x9165,
            index_pointer.0,
            index_pointer.1,
        );
        if let Some(group_pointer) = group_pointer {
            at_element(
                &mut dimension_item,
                0x0020,
                0x9167,
                group_pointer.0,
                group_pointer.1,
            );
        }
        if options.dimension_index == DimensionIndexFixture::PrivateCreator {
            text_element(
                &mut dimension_item,
                0x0020,
                0x9213,
                b"LO",
                "FIXTURE PRIVATE CREATOR",
            );
        }
        sequence_element(&mut bytes, 0x0020, 0x9222, &[dimension_item]);
    }
    us_element(&mut bytes, 0x0028, 0x0002, if color_pixels { 3 } else { 1 });
    text_element(
        &mut bytes,
        0x0028,
        0x0004,
        b"CS",
        if color_pixels { "RGB" } else { "MONOCHROME2" },
    );
    if color_pixels {
        us_element(&mut bytes, 0x0028, 0x0006, 0);
    }
    if options.extended_offset_table {
        text_element(&mut bytes, 0x0028, 0x0008, b"IS", "2");
    } else if enhanced_storage {
        text_element(&mut bytes, 0x0028, 0x0008, b"IS", "12");
    }
    us_element(&mut bytes, 0x0028, 0x0010, fixture_rows);
    us_element(&mut bytes, 0x0028, 0x0011, fixture_columns);
    us_element(
        &mut bytes,
        0x0028,
        0x0100,
        if color_pixels { 8 } else { 16 },
    );
    us_element(
        &mut bytes,
        0x0028,
        0x0101,
        if color_pixels { 8 } else { 16 },
    );
    us_element(
        &mut bytes,
        0x0028,
        0x0102,
        if color_pixels { 7 } else { 15 },
    );
    us_element(&mut bytes, 0x0028, 0x0103, 0);
    if options.vendor == FixtureVendor::PhilipsClassic && !options.philips_omit_public_pixel_scaling
    {
        text_element(&mut bytes, 0x0028, 0x1052, b"DS", "0");
        text_element(&mut bytes, 0x0028, 0x1053, b"DS", "0.00363177");
        text_element(&mut bytes, 0x0028, 0x1054, b"LO", "US");
    }
    if options.include_modality_lut {
        sequence_element(&mut bytes, 0x0028, 0x3000, &[Vec::new()]);
    }
    if options.include_pixel_value_transform {
        let mut transform = Vec::new();
        text_element(&mut transform, 0x0028, 0x1052, b"DS", "0");
        text_element(&mut transform, 0x0028, 0x1053, b"DS", "1");
        if !options.incomplete_pixel_value_transform {
            text_element(&mut transform, 0x0028, 0x1054, b"LO", "US");
        }
        sequence_element(&mut bytes, 0x0028, 0x9145, &[transform]);
    }
    if !options.omit_burned_in_annotation {
        text_element(
            &mut bytes,
            0x0028,
            0x0301,
            b"CS",
            if options.burned_in_annotation {
                "YES"
            } else {
                "NO"
            },
        );
    }
    if enhanced_storage {
        text_element(&mut bytes, 0x0028, 0x2110, b"CS", "00");
    }
    if options.include_privacy_leaks {
        text_element(&mut bytes, 0x0031, 0x0010, b"LO", "UNKNOWN PRIVATE CREATOR");
        us_element(&mut bytes, 0x0031, 0x1001, 99);
        text_element(
            &mut bytes,
            0x0031,
            0x1002,
            b"LO",
            "FIXTURE UNKNOWN PRIVATE TEXT LEAK",
        );
    }
    if options.include_real_world_value_mapping {
        sequence_element(&mut bytes, 0x0040, 0x9096, &[Vec::new()]);
    }
    if enhanced_storage {
        sequence_element(&mut bytes, 0x0040, 0x0555, &[]);
        text_element(&mut bytes, 0x2050, 0x0020, b"CS", "IDENTITY");
        let mut shared_item = Vec::new();

        let mut pixel_measures = Vec::new();
        text_element(&mut pixel_measures, 0x0018, 0x0050, b"DS", "3");
        text_element(&mut pixel_measures, 0x0028, 0x0030, b"DS", "2\\2");
        sequence_element(&mut shared_item, 0x0028, 0x9110, &[pixel_measures]);

        let mut plane_position = Vec::new();
        text_element(&mut plane_position, 0x0020, 0x0032, b"DS", "0\\0\\0");
        sequence_element(&mut shared_item, 0x0020, 0x9113, &[plane_position]);

        let mut plane_orientation = Vec::new();
        text_element(
            &mut plane_orientation,
            0x0020,
            0x0037,
            b"DS",
            "1\\0\\0\\0\\1\\0",
        );
        sequence_element(&mut shared_item, 0x0020, 0x9116, &[plane_orientation]);

        let mut anatomy_code = Vec::new();
        text_element(&mut anatomy_code, 0x0008, 0x0100, b"SH", "T-A0100");
        text_element(&mut anatomy_code, 0x0008, 0x0102, b"SH", "SRT");
        text_element(&mut anatomy_code, 0x0008, 0x0104, b"LO", "Brain");
        let mut frame_anatomy = Vec::new();
        sequence_element(&mut frame_anatomy, 0x0008, 0x2218, &[anatomy_code]);
        text_element(&mut frame_anatomy, 0x0020, 0x9072, b"CS", "U");
        sequence_element(&mut shared_item, 0x0020, 0x9071, &[frame_anatomy]);

        let mut transform = Vec::new();
        text_element(&mut transform, 0x0028, 0x1052, b"DS", "0");
        text_element(&mut transform, 0x0028, 0x1053, b"DS", "1");
        text_element(&mut transform, 0x0028, 0x1054, b"LO", "US");
        sequence_element(&mut shared_item, 0x0028, 0x9145, &[transform]);

        if current_original_or_mixed {
            let mut timing = Vec::new();
            text_element(&mut timing, 0x0018, 0x0080, b"DS", "800");
            text_element(&mut timing, 0x0018, 0x0091, b"IS", "1");
            text_element(&mut timing, 0x0018, 0x1314, b"DS", "70");
            us_element(&mut timing, 0x0018, 0x9240, 1);
            us_element(&mut timing, 0x0018, 0x9241, 1);
            sequence_element(&mut shared_item, 0x0018, 0x9112, &[timing]);

            let mut echo = Vec::new();
            fd_element(&mut echo, 0x0018, 0x9082, 30.0);
            sequence_element(&mut shared_item, 0x0018, 0x9114, &[echo]);

            let mut modifier = Vec::new();
            text_element(&mut modifier, 0x0018, 0x9009, b"CS", "NO");
            text_element(&mut modifier, 0x0018, 0x9010, b"CS", "NONE");
            text_element(&mut modifier, 0x0018, 0x9016, b"CS", "NONE");
            text_element(&mut modifier, 0x0018, 0x9021, b"CS", "NO");
            text_element(&mut modifier, 0x0018, 0x9026, b"CS", "NONE");
            text_element(&mut modifier, 0x0018, 0x9027, b"CS", "NONE");
            text_element(&mut modifier, 0x0018, 0x9077, b"CS", "NO");
            text_element(&mut modifier, 0x0018, 0x9081, b"CS", "NO");
            sequence_element(&mut shared_item, 0x0018, 0x9115, &[modifier]);

            let mut imaging_modifier = Vec::new();
            text_element(&mut imaging_modifier, 0x0018, 0x0095, b"DS", "2000");
            text_element(&mut imaging_modifier, 0x0018, 0x9020, b"CS", "NONE");
            text_element(&mut imaging_modifier, 0x0018, 0x9022, b"CS", "NO");
            text_element(&mut imaging_modifier, 0x0018, 0x9028, b"CS", "NONE");
            fd_element(&mut imaging_modifier, 0x0018, 0x9098, 123.25);
            sequence_element(&mut shared_item, 0x0018, 0x9006, &[imaging_modifier]);

            let mut receive = Vec::new();
            text_element(&mut receive, 0x0018, 0x1250, b"SH", "HEAD_32");
            text_element(&mut receive, 0x0018, 0x9041, b"LO", "");
            text_element(&mut receive, 0x0018, 0x9043, b"CS", "VOLUME");
            text_element(&mut receive, 0x0018, 0x9044, b"CS", "YES");
            sequence_element(&mut shared_item, 0x0018, 0x9042, &[receive]);

            let mut transmit = Vec::new();
            text_element(&mut transmit, 0x0018, 0x1251, b"SH", "BODY");
            text_element(&mut transmit, 0x0018, 0x9050, b"LO", "");
            text_element(&mut transmit, 0x0018, 0x9051, b"CS", "BODY");
            sequence_element(&mut shared_item, 0x0018, 0x9049, &[transmit]);

            let mut averages = Vec::new();
            text_element(&mut averages, 0x0018, 0x0083, b"DS", "1");
            sequence_element(&mut shared_item, 0x0018, 0x9119, &[averages]);

            let mut fov = Vec::new();
            text_element(&mut fov, 0x0018, 0x0093, b"DS", "100");
            text_element(&mut fov, 0x0018, 0x0094, b"DS", "100");
            text_element(&mut fov, 0x0018, 0x1312, b"CS", "COLUMN");
            us_element(&mut fov, 0x0018, 0x9058, 64);
            us_element(&mut fov, 0x0018, 0x9231, 64);
            sequence_element(&mut shared_item, 0x0018, 0x9125, &[fov]);
        }
        if legacy_converted_mr {
            sequence_element(&mut shared_item, 0x0020, 0x9170, &[Vec::new()]);
        }
        sequence_element(&mut bytes, 0x5200, 0x9229, &[shared_item]);
        if options.frame_voi_lut == FrameVoiFixture::OffContext {
            sequence_element(
                &mut bytes,
                0x0028,
                0x9132,
                &frame_voi_lut_items(FrameVoiFixture::Valid, 0),
            );
        }
        let frame_count = if options.extended_offset_table { 2 } else { 12 };
        let mut frames = Vec::with_capacity(frame_count);
        for frame_index in 0..frame_count {
            let mut frame_type_item = Vec::new();
            text_element(
                &mut frame_type_item,
                0x0008,
                0x9007,
                b"CS",
                options
                    .enhanced_frame_type_override
                    .unwrap_or_else(|| enhanced_frame_type(options.purpose)),
            );
            text_element(&mut frame_type_item, 0x0008, 0x9205, b"CS", "MONOCHROME");
            text_element(&mut frame_type_item, 0x0008, 0x9206, b"CS", "VOLUME");
            text_element(&mut frame_type_item, 0x0008, 0x9207, b"CS", "NONE");

            let mut frame_content_item = Vec::new();
            text_element(
                &mut frame_content_item,
                0x0018,
                0x9074,
                b"DT",
                "20260718120000",
            );
            text_element(
                &mut frame_content_item,
                0x0018,
                0x9151,
                b"DT",
                "20260718120000",
            );
            fd_element(&mut frame_content_item, 0x0018, 0x9220, 10.0);
            text_element(
                &mut frame_content_item,
                0x0020,
                0x9056,
                b"SH",
                "ORIGINAL_STACK",
            );
            ul_element(
                &mut frame_content_item,
                0x0020,
                0x9057,
                frame_index as u32 + 1,
            );
            fd_element(
                &mut frame_content_item,
                0x0020,
                0x9153,
                frame_index as f64 * 10.0,
            );
            ul_values_element(
                &mut frame_content_item,
                0x0020,
                0x9157,
                &[
                    if options.dimension_index == DimensionIndexFixture::ZeroIndexValue
                        && frame_index == 0
                    {
                        0
                    } else {
                        frame_index as u32 + 1
                    },
                ],
            );

            let mut frame_item = Vec::new();
            sequence_element(&mut frame_item, 0x0018, 0x9226, &[frame_type_item]);
            if options.purpose == FixturePurpose::Diffusion {
                let mut gradient_item = Vec::new();
                fd_values_element(&mut gradient_item, 0x0018, 0x9089, &[1.0, 0.0, 0.0]);
                let mut diffusion_item = Vec::new();
                text_element(&mut diffusion_item, 0x0018, 0x9075, b"CS", "DIRECTIONAL");
                sequence_element(&mut diffusion_item, 0x0018, 0x9076, &[gradient_item]);
                fd_element(&mut diffusion_item, 0x0018, 0x9087, 1_000.0);
                sequence_element(&mut frame_item, 0x0018, 0x9117, &[diffusion_item]);
            }
            if options.purpose == FixturePurpose::AslPerfusion {
                let mut slab_item = Vec::new();
                us_element(&mut slab_item, 0x0018, 0x9253, 1);
                fd_element(&mut slab_item, 0x0018, 0x9254, 120.0);
                fd_values_element(&mut slab_item, 0x0018, 0x9255, &[0.0, 0.0, 1.0]);
                fd_values_element(&mut slab_item, 0x0018, 0x9256, &[0.0, 0.0, 0.0]);
                ul_element(&mut slab_item, 0x0018, 0x9258, 1800);
                let mut asl_item = Vec::new();
                text_element(
                    &mut asl_item,
                    0x0018,
                    0x9252,
                    b"LO",
                    "FIXTURE ASL TECHNIQUE PHI",
                );
                text_element(&mut asl_item, 0x0018, 0x9257, b"CS", "LABEL");
                text_element(
                    &mut asl_item,
                    0x0018,
                    0x9259,
                    b"CS",
                    if options.asl_crusher { "YES" } else { "NO" },
                );
                if options.asl_crusher {
                    fd_element(&mut asl_item, 0x0018, 0x925A, 0.0);
                    if !options.omit_asl_crusher_description {
                        text_element(
                            &mut asl_item,
                            0x0018,
                            0x925B,
                            b"LO",
                            "FIXTURE CRUSHER FREE TEXT",
                        );
                    }
                }
                text_element(
                    &mut asl_item,
                    0x0018,
                    0x925C,
                    b"CS",
                    if options.asl_bolus_cutoff {
                        "YES"
                    } else {
                        "NO"
                    },
                );
                if options.asl_bolus_cutoff {
                    let mut bolus_item = Vec::new();
                    text_element(
                        &mut bolus_item,
                        0x0018,
                        0x925E,
                        b"LO",
                        "FIXTURE BOLUS TECHNIQUE",
                    );
                    ul_element(&mut bolus_item, 0x0018, 0x925F, 450);
                    sequence_element(&mut asl_item, 0x0018, 0x925D, &[bolus_item]);
                }
                sequence_element(&mut asl_item, 0x0018, 0x9260, &[slab_item]);
                sequence_element(&mut frame_item, 0x0018, 0x9251, &[asl_item]);
            }
            sequence_element(&mut frame_item, 0x0020, 0x9111, &[frame_content_item]);
            if legacy_converted_mr {
                sequence_element(&mut frame_item, 0x0020, 0x9171, &[Vec::new()]);
            }
            if options.frame_voi_lut == FrameVoiFixture::DirectNestedWindow {
                text_element(&mut frame_item, 0x0028, 0x1050, b"DS", "1019");
                text_element(&mut frame_item, 0x0028, 0x1051, b"DS", "1772");
            } else if !matches!(
                options.frame_voi_lut,
                FrameVoiFixture::Absent | FrameVoiFixture::OffContext
            ) {
                sequence_element(
                    &mut frame_item,
                    0x0028,
                    0x9132,
                    &frame_voi_lut_items(options.frame_voi_lut, frame_index),
                );
            }
            frames.push(frame_item);
        }
        sequence_element(&mut bytes, 0x5200, 0x9230, &frames);
    }
    if options.extended_offset_table {
        let (offsets, lengths, pixel_data) =
            fixture_extended_offset_table_pixel_element(options.pixel_bytes);
        ov_element(&mut bytes, 0x7FE0, 0x0001, &offsets);
        ov_element(&mut bytes, 0x7FE0, 0x0002, &lengths);
        bytes.extend_from_slice(&pixel_data);
    } else if options.encapsulated_pixel_data {
        bytes.extend_from_slice(&fixture_encapsulated_pixel_element(options.pixel_bytes));
    } else {
        let pixels = fixture_pixel_bytes(native_pixel_bytes);
        element(&mut bytes, 0x7FE0, 0x0010, b"OW", &pixels);
    }
    fs::write(path, bytes).unwrap();
}

fn frame_voi_lut_items(mode: FrameVoiFixture, frame_index: usize) -> Vec<Vec<u8>> {
    let mut item = Vec::new();
    text_element(
        &mut item,
        0x0028,
        0x1050,
        b"DS",
        &(1019 + frame_index).to_string(),
    );
    if mode != FrameVoiFixture::MissingWidth {
        text_element(&mut item, 0x0028, 0x1051, b"DS", "1772");
    }
    if mode == FrameVoiFixture::ValidWithExplanation {
        text_element(&mut item, 0x0028, 0x1055, b"LO", "FIXTURE WINDOW LABEL");
    }
    if mode == FrameVoiFixture::VoiLutFunction {
        text_element(&mut item, 0x0028, 0x1056, b"CS", "SIGMOID");
    }
    if mode == FrameVoiFixture::ExtraAttribute {
        us_element(&mut item, 0x0028, 0x0002, 1);
    }
    if mode == FrameVoiFixture::MultipleItems {
        vec![item.clone(), item]
    } else {
        vec![item]
    }
}

fn fixture_image_type(purpose: FixturePurpose, siemens_mosaic: bool) -> &'static str {
    match purpose {
        FixturePurpose::FunctionalEpi if siemens_mosaic => {
            "ORIGINAL\\PRIMARY\\M\\EPI\\BOLD\\MOSAIC"
        }
        FixturePurpose::FunctionalEpi => "ORIGINAL\\PRIMARY\\M\\EPI\\BOLD",
        FixturePurpose::Diffusion if siemens_mosaic => "ORIGINAL\\PRIMARY\\M\\DIFFUSION\\MOSAIC",
        FixturePurpose::StructuralT1w => "ORIGINAL\\PRIMARY\\M\\T1",
        FixturePurpose::StructuralT2w => "ORIGINAL\\PRIMARY\\M\\T2",
        FixturePurpose::StructuralOther => "ORIGINAL\\PRIMARY\\M",
        FixturePurpose::Diffusion => "ORIGINAL\\PRIMARY\\M\\DIFFUSION",
        FixturePurpose::Sbref => "ORIGINAL\\PRIMARY\\M\\SBREF",
        FixturePurpose::Localizer => "ORIGINAL\\PRIMARY\\M\\LOCALIZER",
        FixturePurpose::DerivedMr => "DERIVED\\SECONDARY\\M",
        FixturePurpose::AslPerfusion | FixturePurpose::Fieldmap | FixturePurpose::OtherMr => {
            "ORIGINAL\\PRIMARY\\M"
        }
    }
}

fn fixture_uih_image_type(purpose: FixturePurpose) -> &'static str {
    match purpose {
        FixturePurpose::FunctionalEpi => "ORIGINAL\\PRIMARY\\M\\GRID\\EPI\\BOLD",
        FixturePurpose::Diffusion => "ORIGINAL\\PRIMARY\\M\\GRID\\DIFFUSION",
        _ => fixture_image_type(purpose, false),
    }
}

fn fixture_series_description(purpose: FixturePurpose) -> &'static str {
    match purpose {
        FixturePurpose::FunctionalEpi => "fixture functional BOLD",
        FixturePurpose::StructuralT1w => "fixture T1w MPRAGE structural",
        FixturePurpose::StructuralT2w => "fixture T2w SPACE structural",
        FixturePurpose::StructuralOther => "fixture anatomical structural",
        FixturePurpose::Diffusion => "fixture diffusion DWI",
        FixturePurpose::AslPerfusion => "fixture pCASL perfusion",
        FixturePurpose::Fieldmap => "fixture fieldmap",
        FixturePurpose::Sbref => "fixture SBRef",
        FixturePurpose::Localizer => "fixture localizer scout",
        FixturePurpose::DerivedMr => "fixture derived MR",
        FixturePurpose::OtherMr => "fixture quantitative MR",
    }
}

fn fixture_protocol_name(purpose: FixturePurpose) -> &'static str {
    match purpose {
        FixturePurpose::FunctionalEpi => "task_fixture",
        FixturePurpose::StructuralT1w => "t1_mprage_fixture",
        FixturePurpose::StructuralT2w => "t2_space_fixture",
        FixturePurpose::StructuralOther => "anatomical_fixture",
        FixturePurpose::Diffusion => "dwi_fixture",
        FixturePurpose::AslPerfusion => "pcasl_fixture",
        FixturePurpose::Fieldmap => "fieldmap_fixture",
        FixturePurpose::Sbref => "sbref_fixture",
        FixturePurpose::Localizer => "localizer_fixture",
        FixturePurpose::DerivedMr => "derived_fixture",
        FixturePurpose::OtherMr => "other_mr_fixture",
    }
}

fn fixture_scanning_sequence(purpose: FixturePurpose) -> &'static str {
    match purpose {
        FixturePurpose::FunctionalEpi
        | FixturePurpose::Diffusion
        | FixturePurpose::AslPerfusion
        | FixturePurpose::Fieldmap
        | FixturePurpose::Sbref => "EP",
        FixturePurpose::StructuralT1w
        | FixturePurpose::StructuralT2w
        | FixturePurpose::StructuralOther
        | FixturePurpose::Localizer
        | FixturePurpose::DerivedMr => "GR",
        FixturePurpose::OtherMr => "SE",
    }
}

fn fixture_sequence_name(purpose: FixturePurpose) -> &'static str {
    match purpose {
        FixturePurpose::FunctionalEpi => "ep2d_bold",
        FixturePurpose::StructuralT1w => "mprage",
        FixturePurpose::StructuralT2w => "space",
        FixturePurpose::StructuralOther => "anatomical",
        FixturePurpose::Diffusion => "ep2d_diff",
        FixturePurpose::AslPerfusion => "pcasl",
        FixturePurpose::Fieldmap => "gre_fieldmap",
        FixturePurpose::Sbref => "ep2d_sbref",
        FixturePurpose::Localizer => "localizer",
        FixturePurpose::DerivedMr => "derived",
        FixturePurpose::OtherMr => "quantitative",
    }
}

fn enhanced_frame_type(purpose: FixturePurpose) -> &'static str {
    match purpose {
        FixturePurpose::FunctionalEpi => "ORIGINAL\\PRIMARY\\FMRI\\NONE",
        FixturePurpose::StructuralT1w => "ORIGINAL\\PRIMARY\\T1\\NONE",
        FixturePurpose::StructuralT2w => "ORIGINAL\\PRIMARY\\T2\\NONE",
        FixturePurpose::Diffusion => "ORIGINAL\\PRIMARY\\DIFFUSION\\NONE",
        FixturePurpose::AslPerfusion => "ORIGINAL\\PRIMARY\\ASL\\NONE",
        FixturePurpose::Fieldmap => "ORIGINAL\\PRIMARY\\VOLUME\\NONE",
        FixturePurpose::Localizer => "ORIGINAL\\PRIMARY\\LOCALIZER\\NONE",
        FixturePurpose::DerivedMr => "DERIVED\\PRIMARY\\VOLUME\\RESAMPLED",
        FixturePurpose::StructuralOther | FixturePurpose::Sbref | FixturePurpose::OtherMr => {
            "ORIGINAL\\PRIMARY\\VOLUME\\NONE"
        }
    }
}

pub fn fixture_pixel_bytes(length: usize) -> Vec<u8> {
    let even_length = length + length % 2;
    (0..even_length)
        .map(|index| ((index * 31 + 7) % 251) as u8)
        .collect()
}

fn fixture_native_pixel_matrix(
    value_length: usize,
    frames: usize,
    samples: usize,
    bytes_per_sample: usize,
) -> (u16, u16) {
    let denominator = frames * samples * bytes_per_sample;
    assert_eq!(value_length % denominator, 0);
    let pixels_per_frame = value_length / denominator;
    if pixels_per_frame % 64 == 0 && pixels_per_frame / 64 <= usize::from(u16::MAX) {
        return (64, u16::try_from(pixels_per_frame / 64).unwrap());
    }
    for rows in (1..=pixels_per_frame.min(usize::from(u16::MAX))).rev() {
        if pixels_per_frame % rows == 0 && pixels_per_frame / rows <= usize::from(u16::MAX) {
            return (
                u16::try_from(rows).unwrap(),
                u16::try_from(pixels_per_frame / rows).unwrap(),
            );
        }
    }
    panic!("fixture PixelData cannot be represented by a valid DICOM matrix");
}

pub fn fixture_encapsulated_pixel_element(length: usize) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(&0x7FE0_u16.to_le_bytes());
    output.extend_from_slice(&0x0010_u16.to_le_bytes());
    output.extend_from_slice(b"OB");
    output.extend_from_slice(&[0, 0]);
    output.extend_from_slice(&u32::MAX.to_le_bytes());

    // Basic Offset Table, followed by two opaque compressed fragments. The
    // parser does not decode them; this fixture exercises exact byte-span copy.
    output.extend_from_slice(&0xFFFE_u16.to_le_bytes());
    output.extend_from_slice(&0xE000_u16.to_le_bytes());
    output.extend_from_slice(&4_u32.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());

    let even_length = length.max(8) + length.max(8) % 2;
    let mut payload = fixture_pixel_bytes(even_length);
    payload[..4].copy_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0]);
    payload[even_length - 2..].copy_from_slice(&[0xFF, 0xD9]);
    let split = (even_length / 2) & !1;
    for fragment in [&payload[..split], &payload[split..]] {
        output.extend_from_slice(&0xFFFE_u16.to_le_bytes());
        output.extend_from_slice(&0xE000_u16.to_le_bytes());
        output.extend_from_slice(&(fragment.len() as u32).to_le_bytes());
        output.extend_from_slice(fragment);
    }
    output.extend_from_slice(&0xFFFE_u16.to_le_bytes());
    output.extend_from_slice(&0xE0DD_u16.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output
}

pub fn fixture_extended_offset_table_pixel_element(length: usize) -> (Vec<u64>, Vec<u64>, Vec<u8>) {
    let even_length = length.max(8) + length.max(8) % 2;
    let mut payload = fixture_pixel_bytes(even_length);
    payload[..4].copy_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0]);
    payload[even_length - 2..].copy_from_slice(&[0xFF, 0xD9]);
    let split = (even_length / 2) & !1;
    let fragments = [&payload[..split], &payload[split..]];
    let offsets = vec![0, 8 + fragments[0].len() as u64];
    let lengths = fragments
        .iter()
        .map(|fragment| fragment.len() as u64)
        .collect::<Vec<_>>();

    let mut output = Vec::new();
    output.extend_from_slice(&0x7FE0_u16.to_le_bytes());
    output.extend_from_slice(&0x0010_u16.to_le_bytes());
    output.extend_from_slice(b"OB");
    output.extend_from_slice(&[0, 0]);
    output.extend_from_slice(&u32::MAX.to_le_bytes());
    output.extend_from_slice(&0xFFFE_u16.to_le_bytes());
    output.extend_from_slice(&0xE000_u16.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    for fragment in fragments {
        output.extend_from_slice(&0xFFFE_u16.to_le_bytes());
        output.extend_from_slice(&0xE000_u16.to_le_bytes());
        output.extend_from_slice(&(fragment.len() as u32).to_le_bytes());
        output.extend_from_slice(fragment);
    }
    output.extend_from_slice(&0xFFFE_u16.to_le_bytes());
    output.extend_from_slice(&0xE0DD_u16.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    (offsets, lengths, output)
}

fn siemens_mosaic_csa_fixture(diffusion: bool, b_matrix: bool) -> Vec<u8> {
    let times = (0..36)
        .map(|value| (value * 25).to_string())
        .collect::<Vec<_>>();
    let time_refs = times.iter().map(String::as_str).collect::<Vec<_>>();
    let mut tags = vec![
        ("NumberOfImagesInMosaic", "IS", vec!["36"]),
        ("SliceNormalVector", "DS", vec!["0", "0", "1"]),
        ("SliceMeasurementDuration", "DS", vec!["800000"]),
        ("BandwidthPerPixelPhaseEncode", "DS", vec!["22.5"]),
        ("MosaicRefAcqTimes", "DS", time_refs),
        ("PhaseEncodingDirectionPositive", "IS", vec!["1"]),
        (
            "MrPhoenixProtocol",
            "OB",
            vec!["### ASCCONV BEGIN ###\nsPatientName = Paul Scott\n"],
        ),
    ];
    if diffusion {
        tags.push(("B_value", "IS", vec!["1000"]));
        if b_matrix {
            tags.push(("B_matrix", "DS", vec!["1000", "0", "0", "0", "0", "0"]));
        } else {
            tags.push(("DiffusionGradientDirection", "DS", vec!["1", "0", "0"]));
        }
    }
    csa2(&tags)
}

fn csa2(tags: &[(&str, &str, Vec<&str>)]) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(b"SV10");
    output.extend_from_slice(&[4, 3, 2, 1]);
    output.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    output.extend_from_slice(&77_u32.to_le_bytes());
    for (name, vr, values) in tags {
        let mut serialized_values = values.clone();
        let declared_vm = if *name == "MosaicRefAcqTimes" {
            serialized_values.resize(54, "");
            0
        } else {
            let declared = values.len();
            if matches!(*name, "NumberOfImagesInMosaic" | "SliceNormalVector") {
                serialized_values.resize(6, "");
            }
            declared
        };
        let mut name_bytes = [0_u8; 64];
        name_bytes[..name.len()].copy_from_slice(name.as_bytes());
        output.extend_from_slice(&name_bytes);
        output.extend_from_slice(&(declared_vm as i32).to_le_bytes());
        let mut vr_bytes = [0_u8; 4];
        vr_bytes[..vr.len()].copy_from_slice(vr.as_bytes());
        output.extend_from_slice(&vr_bytes);
        output.extend_from_slice(&0_i32.to_le_bytes());
        output.extend_from_slice(&(serialized_values.len() as i32).to_le_bytes());
        output.extend_from_slice(&77_i32.to_le_bytes());
        for value in serialized_values {
            let mut bytes = value.as_bytes().to_vec();
            bytes.push(0);
            let length = bytes.len() as i32;
            output.extend_from_slice(&length.to_le_bytes());
            output.extend_from_slice(&length.to_le_bytes());
            output.extend_from_slice(&77_i32.to_le_bytes());
            output.extend_from_slice(&0_i32.to_le_bytes());
            output.extend_from_slice(&bytes);
            output.resize(output.len() + (4 - bytes.len() % 4) % 4, 0);
        }
    }
    output
}

fn text_element(output: &mut Vec<u8>, group: u16, item: u16, vr: &[u8; 2], value: &str) {
    let mut bytes = value.as_bytes().to_vec();
    if bytes.len() % 2 != 0 {
        bytes.push(if vr == b"UI" { 0 } else { b' ' });
    }
    element(output, group, item, vr, &bytes);
}

fn us_element(output: &mut Vec<u8>, group: u16, item: u16, value: u16) {
    element(output, group, item, b"US", &value.to_le_bytes());
}

fn fl_element(output: &mut Vec<u8>, group: u16, item: u16, value: f32) {
    element(output, group, item, b"FL", &value.to_le_bytes());
}

fn fd_element(output: &mut Vec<u8>, group: u16, item: u16, value: f64) {
    element(output, group, item, b"FD", &value.to_le_bytes());
}

fn fd_values_element(output: &mut Vec<u8>, group: u16, item: u16, values: &[f64]) {
    let bytes = values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    element(output, group, item, b"FD", &bytes);
}

fn sl_element(output: &mut Vec<u8>, group: u16, item: u16, value: i32) {
    element(output, group, item, b"SL", &value.to_le_bytes());
}

fn ul_element(output: &mut Vec<u8>, group: u16, item: u16, value: u32) {
    element(output, group, item, b"UL", &value.to_le_bytes());
}

fn ul_values_element(output: &mut Vec<u8>, group: u16, item: u16, values: &[u32]) {
    let value = values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    element(output, group, item, b"UL", &value);
}

fn at_element(output: &mut Vec<u8>, group: u16, item: u16, pointed_group: u16, pointed_item: u16) {
    let mut value = Vec::with_capacity(4);
    value.extend_from_slice(&pointed_group.to_le_bytes());
    value.extend_from_slice(&pointed_item.to_le_bytes());
    element(output, group, item, b"AT", &value);
}

fn ov_element(output: &mut Vec<u8>, group: u16, item: u16, values: &[u64]) {
    let value = values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    element(output, group, item, b"OV", &value);
}

fn sequence_element(output: &mut Vec<u8>, group: u16, element_number: u16, items: &[Vec<u8>]) {
    let mut value = Vec::new();
    for item in items {
        value.extend_from_slice(&0xFFFE_u16.to_le_bytes());
        value.extend_from_slice(&0xE000_u16.to_le_bytes());
        value.extend_from_slice(&(item.len() as u32).to_le_bytes());
        value.extend_from_slice(item);
    }
    element(output, group, element_number, b"SQ", &value);
}

fn element(output: &mut Vec<u8>, group: u16, item: u16, vr: &[u8; 2], value: &[u8]) {
    output.extend_from_slice(&group.to_le_bytes());
    output.extend_from_slice(&item.to_le_bytes());
    output.extend_from_slice(vr);
    if matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UR" | b"UT" | b"UN"
    ) {
        output.extend_from_slice(&[0, 0]);
        output.extend_from_slice(&(value.len() as u32).to_le_bytes());
    } else {
        output.extend_from_slice(&(value.len() as u16).to_le_bytes());
    }
    output.extend_from_slice(value);
}
