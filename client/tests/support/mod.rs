#![allow(dead_code)]

use std::{fs, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureVendor {
    Generic,
    Siemens,
    PhilipsClassic,
    PhilipsEnhanced,
    Ge,
}

#[derive(Debug, Clone)]
pub struct FunctionalDicomOptions {
    pub burned_in_annotation: bool,
    pub omit_burned_in_annotation: bool,
    pub include_privacy_leaks: bool,
    pub hostile_free_text: bool,
    pub pixel_bytes: usize,
    pub vendor: FixtureVendor,
    pub model_override: Option<&'static str>,
    pub software_versions_override: Option<&'static str>,
    pub siemens_mosaic: bool,
    pub encapsulated_pixel_data: bool,
    /// Emit a complete, internally consistent Philips classic dynamic-series
    /// timing contract. `instance` is interpreted as one-based slice-major
    /// acquisition order.
    pub philips_dynamic_timing: bool,
    pub philips_dynamic_timing_malformed: bool,
    pub philips_private_metadata_malformed: bool,
    /// Emit the real-world broad `(2005,xx0F)` DD 005 per-frame container
    /// without a creator-mapped scale slope. It must be dropped, not mistaken
    /// for the narrow PS3.15 scale-slope exception.
    pub philips_non_scaling_per_frame_container: bool,
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
            pixel_bytes: 64 * 64 * 2,
            vendor: FixtureVendor::Siemens,
            model_override: None,
            software_versions_override: None,
            siemens_mosaic: true,
            encapsulated_pixel_data: false,
            philips_dynamic_timing: false,
            philips_dynamic_timing_malformed: false,
            philips_private_metadata_malformed: false,
            philips_non_scaling_per_frame_container: false,
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
    let siemens_mosaic = options.vendor == FixtureVendor::Siemens && options.siemens_mosaic;
    let sop_class = match options.vendor {
        FixtureVendor::PhilipsEnhanced => "1.2.840.10008.5.1.4.1.1.4.1",
        FixtureVendor::Generic
        | FixtureVendor::Siemens
        | FixtureVendor::PhilipsClassic
        | FixtureVendor::Ge => "1.2.840.10008.5.1.4.1.1.4",
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
        if options.encapsulated_pixel_data {
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
        if options.hostile_free_text {
            "ORIGINAL\\PRIMARY\\M\\EPI\\BOLD\\PAUL_MRN_12345"
        } else if siemens_mosaic {
            "ORIGINAL\\PRIMARY\\M\\EPI\\BOLD\\MOSAIC"
        } else {
            "ORIGINAL\\PRIMARY\\M\\EPI\\BOLD"
        },
    );
    text_element(&mut bytes, 0x0008, 0x0016, b"UI", sop_class);
    text_element(&mut bytes, 0x0008, 0x0018, b"UI", &sop_uid);
    if options.include_privacy_leaks {
        text_element(&mut bytes, 0x0008, 0x0020, b"DA", "20260718");
    }
    text_element(&mut bytes, 0x0008, 0x0060, b"CS", "MR");
    text_element(&mut bytes, 0x0008, 0x0070, b"LO", manufacturer);
    if options.include_privacy_leaks {
        text_element(&mut bytes, 0x0008, 0x0080, b"LO", "FIXTURE SECRET HOSPITAL");
        text_element(&mut bytes, 0x0008, 0x0090, b"PN", "FIXTURE^PHYSICIAN");
    }
    text_element(&mut bytes, 0x0008, 0x103E, b"LO", "fixture functional BOLD");
    text_element(&mut bytes, 0x0008, 0x1090, b"LO", model);
    if options.include_privacy_leaks {
        let mut referenced_item = Vec::new();
        text_element(&mut referenced_item, 0x0008, 0x0020, b"DA", "20260718");
        text_element(
            &mut referenced_item,
            0x0008,
            0x0090,
            b"PN",
            "NESTED^FIXTURE^PHYSICIAN",
        );
        text_element(&mut referenced_item, 0x0008, 0x1150, b"UI", sop_class);
        text_element(&mut referenced_item, 0x0008, 0x1155, b"UI", &sop_uid);
        sequence_element(&mut bytes, 0x0008, 0x1140, &[referenced_item]);
    }
    // Synthetic values only: tests must never depend on real patient data.
    text_element(&mut bytes, 0x0010, 0x0010, b"PN", "FIXTURE^SUBJECT");
    text_element(&mut bytes, 0x0010, 0x0020, b"LO", "FIXTURE-SUBJECT-001");
    text_element(&mut bytes, 0x0018, 0x0020, b"CS", "EP");
    text_element(&mut bytes, 0x0018, 0x0023, b"CS", "2D");
    text_element(
        &mut bytes,
        0x0018,
        0x0024,
        b"SH",
        if options.hostile_free_text {
            "PAUL_MRN_12345"
        } else {
            "ep2d_bold"
        },
    );
    if options.vendor != FixtureVendor::PhilipsEnhanced {
        text_element(&mut bytes, 0x0018, 0x0080, b"DS", "800");
        text_element(&mut bytes, 0x0018, 0x0081, b"DS", "30");
    }
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
        }
    };
    text_element(&mut bytes, 0x0018, 0x1020, b"LO", software_versions);
    text_element(&mut bytes, 0x0018, 0x1030, b"LO", "task_fixture");
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
            }
        },
    );
    match options.vendor {
        FixtureVendor::Siemens => {
            text_element(&mut bytes, 0x0019, 0x0010, b"LO", "SIEMENS MR HEADER");
            us_element(&mut bytes, 0x0019, 0x100A, 42);
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
                    &siemens_mosaic_csa_fixture(),
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
            fl_element(&mut bytes, 0x2005, 0x100D, 0.0);
            if options.philips_private_metadata_malformed {
                text_element(&mut bytes, 0x2005, 0x100E, b"LO", "not-a-scale");
            } else {
                fl_element(&mut bytes, 0x2005, 0x100E, 0.00363177);
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
            sl_element(&mut bytes, 0x2001, 0x1018, options.philips_slices as i32);
            fl_element(&mut bytes, 0x2001, 0x1022, 0.75);
            sl_element(&mut bytes, 0x2001, 0x1019, 20_260_718);
            text_element(
                &mut bytes,
                0x2005,
                0x0014,
                b"LO",
                "Philips MR Imaging DD 005",
            );
            let mut scale_item = Vec::new();
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
        FixtureVendor::Generic | FixtureVendor::Ge => {}
    }
    text_element(&mut bytes, 0x0020, 0x000D, b"UI", study_uid);
    text_element(&mut bytes, 0x0020, 0x000E, b"UI", series_uid);
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
    us_element(&mut bytes, 0x0028, 0x0002, 1);
    text_element(&mut bytes, 0x0028, 0x0004, b"CS", "MONOCHROME2");
    if options.vendor == FixtureVendor::PhilipsEnhanced {
        text_element(&mut bytes, 0x0028, 0x0008, b"IS", "12");
    }
    us_element(&mut bytes, 0x0028, 0x0010, 64);
    us_element(&mut bytes, 0x0028, 0x0011, 64);
    us_element(&mut bytes, 0x0028, 0x0100, 16);
    us_element(&mut bytes, 0x0028, 0x0101, 16);
    us_element(&mut bytes, 0x0028, 0x0102, 15);
    us_element(&mut bytes, 0x0028, 0x0103, 0);
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
    if options.vendor == FixtureVendor::PhilipsEnhanced {
        let mut timing_item = Vec::new();
        if options.include_privacy_leaks {
            text_element(
                &mut timing_item,
                0x0008,
                0x0090,
                b"PN",
                "DEEPLY^NESTED^PHYSICIAN",
            );
        }
        text_element(&mut timing_item, 0x0018, 0x0080, b"DS", "800");
        fd_element(&mut timing_item, 0x0018, 0x9082, 30.0);
        let mut frame_item = Vec::new();
        sequence_element(&mut frame_item, 0x0018, 0x9112, &[timing_item]);
        sequence_element(&mut bytes, 0x5200, 0x9230, &[frame_item]);
    }
    if options.encapsulated_pixel_data {
        bytes.extend_from_slice(&fixture_encapsulated_pixel_element(options.pixel_bytes));
    } else {
        let pixels = fixture_pixel_bytes(options.pixel_bytes);
        element(&mut bytes, 0x7FE0, 0x0010, b"OW", &pixels);
    }
    fs::write(path, bytes).unwrap();
}

pub fn fixture_pixel_bytes(length: usize) -> Vec<u8> {
    let even_length = length + length % 2;
    (0..even_length)
        .map(|index| ((index * 31 + 7) % 251) as u8)
        .collect()
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

fn siemens_mosaic_csa_fixture() -> Vec<u8> {
    let times = (0..36)
        .map(|value| (value * 25).to_string())
        .collect::<Vec<_>>();
    let time_refs = times.iter().map(String::as_str).collect::<Vec<_>>();
    csa2(&[
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
    ])
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

#[allow(dead_code)]
pub fn write_nifti_epi(path: &Path) {
    let dimensions = [4_i16, 8, 8, 8, 300, 1, 1, 1];
    let voxel_count = 8 * 8 * 8 * 300;
    let mut bytes = vec![0_u8; 352 + voxel_count * 4];
    bytes[0..4].copy_from_slice(&348_i32.to_le_bytes());
    for (index, value) in dimensions.iter().enumerate() {
        bytes[40 + index * 2..42 + index * 2].copy_from_slice(&value.to_le_bytes());
    }
    bytes[70..72].copy_from_slice(&16_i16.to_le_bytes());
    bytes[72..74].copy_from_slice(&32_i16.to_le_bytes());
    for (index, value) in [1.0_f32, 2.0, 2.0, 2.0, 0.8].iter().enumerate() {
        bytes[76 + index * 4..80 + index * 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes[108..112].copy_from_slice(&352_f32.to_le_bytes());
    bytes[112..116].copy_from_slice(&1_f32.to_le_bytes());
    bytes[123] = 10; // millimeters + seconds
    bytes[254..256].copy_from_slice(&1_i16.to_le_bytes());
    for (index, value) in [
        2.0_f32, 0.0, 0.0, -7.0, 0.0, 2.0, 0.0, -7.0, 0.0, 0.0, 2.0, -7.0,
    ]
    .iter()
    .enumerate()
    {
        bytes[280 + index * 4..284 + index * 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes[148..167].copy_from_slice(b"SYNTHETIC TEXT LEAK");
    bytes[344..348].copy_from_slice(b"n+1\0");
    for (index, chunk) in bytes[352..].chunks_exact_mut(4).enumerate() {
        chunk.copy_from_slice(&((index % 251) as f32).to_le_bytes());
    }
    fs::write(path, bytes).unwrap();
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

fn sl_element(output: &mut Vec<u8>, group: u16, item: u16, value: i32) {
    element(output, group, item, b"SL", &value.to_le_bytes());
}

fn ul_element(output: &mut Vec<u8>, group: u16, item: u16, value: u32) {
    element(output, group, item, b"UL", &value.to_le_bytes());
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
