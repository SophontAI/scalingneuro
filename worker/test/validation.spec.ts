import { describe, expect, it } from "vitest";
import enrollmentRequestExample from "../../schemas/examples/enrollment-request-v1.example.json";
import dicomUploadInitExample from "../../schemas/examples/dicom-upload-init-v1.example.json";
import uploadCompleteExample from "../../schemas/examples/upload-complete-v1.example.json";
import uploadInitExample from "../../schemas/examples/upload-init-v1.example.json";
import {
  parseCompleteUploadRequest,
  parseCreateDicomUploadRequest,
  parseCreateUploadRequest,
  parseEnrollRequest,
  parseProcessorClaimRequest,
  parseProcessorCompleteRequest,
} from "../src/validation";

const bundleId = "1".repeat(24);
const validBundle = {
  bundle_id: bundleId,
  series_id: "2".repeat(24),
  subject_id: "3".repeat(24),
  session_id: "4".repeat(24),
  protocol_group_id: "5".repeat(24),
  nii: {
    relative_key: `${bundleId}/scan_bold.nii.gz`,
    size: 352,
    sha256: "a".repeat(64),
    uncompressed_sha256: "c".repeat(64),
  },
  metadata: {
    relative_key: `${bundleId}/scan_bold.json`,
    size: 2,
    sha256: "b".repeat(64),
  },
};

describe("strict request validation", () => {
  it("accepts the published upload-init and completion examples", () => {
    expect(parseEnrollRequest(enrollmentRequestExample)).toEqual(
      enrollmentRequestExample,
    );
    expect(parseCreateUploadRequest(uploadInitExample)).toEqual(
      uploadInitExample,
    );
    expect(parseCompleteUploadRequest(uploadCompleteExample)).toEqual(
      uploadCompleteExample,
    );
  });

  it("accepts the minimal pseudonymous bundle manifest", () => {
    expect(
      parseCreateUploadRequest({
        bundles: [validBundle],
        client_version: "0.1.0",
      }),
    ).toEqual({ bundles: [validBundle], client_version: "0.1.0" });
  });

  it("accepts completion receipts for the full 32-bundle legacy limit", () => {
    const objects = Array.from({ length: 64 }, (_, index) => ({
      key: `${index.toString(16).padStart(24, "0")}/dicom.tar.zst`,
      size: 32,
      sha256: index.toString(16).padStart(64, "0"),
      parts: [{ part_number: 1, etag: `etag-${index}` }],
    }));

    expect(parseCompleteUploadRequest({ objects }).objects).toHaveLength(64);
    expect(() =>
      parseCompleteUploadRequest({ objects: [...objects, objects[0]] }),
    ).toThrow(/objects must contain 1-64 entries/u);
  });

  it("enforces the eight-series raw receipt boundary", () => {
    const template = dicomUploadInitExample.series[0]!;
    const series = Array.from({ length: 8 }, (_, index) => {
      const archiveId = index.toString(16).padStart(24, "0");
      return {
        ...template,
        series_archive_id: archiveId,
        series_id: (index + 16).toString(16).padStart(24, "0"),
        archive: {
          ...template.archive,
          relative_key: `${archiveId}/dicom.tar.zst`,
        },
      };
    });
    expect(
      parseCreateDicomUploadRequest({
        ...dicomUploadInitExample,
        series,
      }).series,
    ).toHaveLength(8);
    expect(() =>
      parseCreateDicomUploadRequest({
        ...dicomUploadInitExample,
        series: [...series, series[0]],
      }),
    ).toThrow(/series must contain 1-8 entries/u);
  });

  it("enforces the 500000-instance raw DICOM boundary end to end", () => {
    const template = dicomUploadInitExample.series[0]!;
    const request = {
      ...dicomUploadInitExample,
      series: [{ ...template, dicom_count: 500_000 }],
    };
    expect(parseCreateDicomUploadRequest(request).series[0]?.dicom_count).toBe(
      500_000,
    );
    expect(() =>
      parseCreateDicomUploadRequest({
        ...request,
        series: [{ ...template, dicom_count: 500_001 }],
      }),
    ).toThrow(/dicom_count must be an integer between 1 and 500000/u);

    const completion = {
      lease_token: "0190f86f-e0de-7f2a-a24c-0a6abf16ec81",
      processor_version: "1.0.0",
      dcm2niix_version: "1.0.20260416",
      outputs: [],
      validation: {
        archive_sha256_verified: true,
        dicom_count: 500_000,
        dicom_parse_succeeded: true,
        functional_epi_confirmed: true,
      },
    };
    expect(parseProcessorCompleteRequest(completion).validation).toMatchObject({
      dicom_count: 500_000,
    });
    expect(() =>
      parseProcessorCompleteRequest({
        ...completion,
        validation: { ...completion.validation, dicom_count: 500_001 },
      }),
    ).toThrow(/dicom_count must be an integer between 1 and 500000/u);
  });

  it("accepts only exact optional processor claim input formats", () => {
    const base = { processor_id: "raw-consumer", lease_seconds: 900 };
    expect(parseProcessorClaimRequest(base)).toEqual(base);
    expect(
      parseProcessorClaimRequest({
        ...base,
        claim_input_format: "dicom-series-v1",
      }),
    ).toEqual({ ...base, claim_input_format: "dicom-series-v1" });
    expect(() =>
      parseProcessorClaimRequest({ ...base, claim_input_format: "dicom" }),
    ).toThrow(/claim_input_format must be dicom-series-v1 or nifti-v1/u);
    expect(() =>
      parseProcessorClaimRequest({ ...base, claim_input_format: null }),
    ).toThrow(/claim_input_format must be dicom-series-v1 or nifti-v1/u);
  });

  it("rejects traversal, duplicate keys, and unknown fields", () => {
    expect(() =>
      parseCreateUploadRequest({
        bundles: [
          {
            ...validBundle,
            nii: { ...validBundle.nii, relative_key: "../patient.nii.gz" },
          },
        ],
        client_version: "0.1.0",
      }),
    ).toThrow(/unsafe path segment/u);

    expect(() =>
      parseCreateUploadRequest({
        bundles: [validBundle],
        client_version: "0.1.0",
        patient_name: "must never enter the control plane",
      }),
    ).toThrow(/unknown field/u);

    expect(() =>
      parseCreateUploadRequest({
        bundles: [
          {
            ...validBundle,
            metadata: {
              ...validBundle.metadata,
              relative_key: `${bundleId}/different.json`,
            },
          },
        ],
        client_version: "0.1.0",
      }),
    ).toThrow(/same-basename/u);
  });

  it("rejects enrollment payload extensions", () => {
    expect(() =>
      parseEnrollRequest({
        ...enrollmentRequestExample,
        patient_id: "not-allowed",
      }),
    ).toThrow(/unknown field/u);

    expect(() =>
      parseEnrollRequest({
        ...enrollmentRequestExample,
        enrollment_id: "0190f86f-e0de-7f2a-a24c-0a6abf16ec8Z",
      }),
    ).toThrow(/enrollment_id has an invalid format/u);

    expect(() =>
      parseEnrollRequest({
        ...enrollmentRequestExample,
        device_token: `sn_device_${"a".repeat(42)}`,
      }),
    ).toThrow(/device_token must contain 53-53 characters/u);
  });
});
