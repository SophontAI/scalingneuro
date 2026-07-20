import { describe, expect, it } from "vitest";
import example from "../../schemas/examples/scan-sidecar-v1.example.json";
import { validateSidecarBytes } from "../src/sidecar";

const encoder = new TextEncoder();

function expectation() {
  return {
    bundle_id: example.bundle_id,
    series_id: example.series_id,
    subject_id: example.subject_id,
    session_id: example.session_id,
    protocol_group_id: example.protocol_group_id,
    client_version: example.conversion.client_version,
    nii_relative_key: `bundle/${example.files.nifti.filename}`,
    nii_size: example.files.nifti.size_bytes,
    nii_sha256: example.files.nifti.sha256,
    nii_uncompressed_sha256: example.files.nifti.uncompressed_sha256,
  };
}

describe("stored scan sidecars", () => {
  it("accepts the published minimized sidecar", () => {
    expect(() =>
      validateSidecarBytes(
        encoder.encode(JSON.stringify(example)),
        expectation(),
      ),
    ).not.toThrow();
  });

  it("rejects unknown fields at the privacy boundary", () => {
    const unsafe = { ...example, PatientName: "Research^Participant" };
    expect(() =>
      validateSidecarBytes(
        encoder.encode(JSON.stringify(unsafe)),
        expectation(),
      ),
    ).toThrowError(/scan-sidecar contract/u);
  });

  it("rejects identifier-shaped values in every retained DICOM text class", () => {
    const fields = [
      (value: typeof example) => {
        value.source.manufacturer = "John Doe";
      },
      (value: typeof example) => {
        value.source.model = "PATIENT_JOHN_DOE";
      },
      (value: typeof example) => {
        value.source.software_versions = ["PATIENT_JOHN_DOE"];
      },
      (value: typeof example) => {
        value.source.sequence_name = "JOHN_DOE";
      },
      (value: typeof example) => {
        value.source.receive_coil_name = "JOHN_DOE";
      },
      (value: typeof example) => {
        value.source.scanning_sequence = ["JOHN_DOE"];
      },
      (value: typeof example) => {
        value.image.imaged_nucleus = "JOHN_DOE";
      },
    ];
    for (const mutate of fields) {
      const unsafe = structuredClone(example);
      mutate(unsafe);
      expect(() =>
        validateSidecarBytes(
          encoder.encode(JSON.stringify(unsafe)),
          expectation(),
        ),
      ).toThrowError(/scan-sidecar contract/u);
    }
  });

  it("rejects a valid sidecar attached to the wrong allocated bundle", () => {
    expect(() =>
      validateSidecarBytes(encoder.encode(JSON.stringify(example)), {
        ...expectation(),
        series_id: "ser_different",
      }),
    ).toThrowError(/allocated scan bundle/u);
  });
});
