use std::{fs, path::Path};

/// Build a tiny standards-shaped Explicit VR Little Endian DICOM fixture.
/// All identifiers are synthetic and reserved for automated tests.
pub fn write_functional_epi(path: &Path, instance: u32) {
    write_functional_epi_fixture(path, instance, false);
}

#[allow(dead_code)]
pub fn write_functional_epi_with_burned_annotation(path: &Path, instance: u32) {
    write_functional_epi_fixture(path, instance, true);
}

fn write_functional_epi_fixture(path: &Path, instance: u32, burned_in_annotation: bool) {
    let study_uid = "1.2.826.0.1.3680043.10.999.1";
    let series_uid = "1.2.826.0.1.3680043.10.999.1.1";
    let sop_uid = format!("1.2.826.0.1.3680043.10.999.1.1.{instance}");
    let sop_class = "1.2.840.10008.5.1.4.1.1.4";

    let mut meta_body = Vec::new();
    element(&mut meta_body, 0x0002, 0x0001, b"OB", &[0, 1]);
    text_element(&mut meta_body, 0x0002, 0x0002, b"UI", sop_class);
    text_element(&mut meta_body, 0x0002, 0x0003, b"UI", &sop_uid);
    text_element(&mut meta_body, 0x0002, 0x0010, b"UI", "1.2.840.10008.1.2.1");
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
        "ORIGINAL\\PRIMARY\\M\\EPI",
    );
    text_element(&mut bytes, 0x0008, 0x0016, b"UI", sop_class);
    text_element(&mut bytes, 0x0008, 0x0018, b"UI", &sop_uid);
    text_element(&mut bytes, 0x0008, 0x0060, b"CS", "MR");
    text_element(&mut bytes, 0x0008, 0x0070, b"LO", "FIXTURE_VENDOR");
    text_element(&mut bytes, 0x0008, 0x103E, b"LO", "fixture functional BOLD");
    // Synthetic values only: tests must never depend on real patient data.
    text_element(&mut bytes, 0x0010, 0x0010, b"PN", "FIXTURE^SUBJECT");
    text_element(&mut bytes, 0x0010, 0x0020, b"LO", "FIXTURE-SUBJECT-001");
    text_element(&mut bytes, 0x0018, 0x0020, b"CS", "EP");
    text_element(&mut bytes, 0x0018, 0x0023, b"CS", "2D");
    text_element(&mut bytes, 0x0018, 0x0024, b"SH", "ep2d_bold");
    text_element(&mut bytes, 0x0018, 0x0080, b"DS", "800");
    text_element(&mut bytes, 0x0018, 0x0081, b"DS", "30");
    text_element(&mut bytes, 0x0018, 0x0087, b"DS", "3");
    text_element(&mut bytes, 0x0018, 0x1030, b"LO", "task_fixture");
    text_element(&mut bytes, 0x0020, 0x000D, b"UI", study_uid);
    text_element(&mut bytes, 0x0020, 0x000E, b"UI", series_uid);
    text_element(&mut bytes, 0x0020, 0x0011, b"IS", "7");
    text_element(&mut bytes, 0x0020, 0x0013, b"IS", &instance.to_string());
    text_element(&mut bytes, 0x0020, 0x0105, b"IS", "300");
    us_element(&mut bytes, 0x0028, 0x0010, 64);
    us_element(&mut bytes, 0x0028, 0x0011, 64);
    if burned_in_annotation {
        text_element(&mut bytes, 0x0028, 0x0301, b"CS", "YES");
    }
    element(&mut bytes, 0x7FE0, 0x0010, b"OW", &[]);
    fs::write(path, bytes).unwrap();
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

fn ul_element(output: &mut Vec<u8>, group: u16, item: u16, value: u32) {
    element(output, group, item, b"UL", &value.to_le_bytes());
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
