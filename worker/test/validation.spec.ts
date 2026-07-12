import { describe, expect, it } from "vitest";
import enrollmentRequestExample from "../../schemas/examples/enrollment-request-v1.example.json";
import uploadCompleteExample from "../../schemas/examples/upload-complete-v1.example.json";
import uploadInitExample from "../../schemas/examples/upload-init-v1.example.json";
import {
  parseCompleteUploadRequest,
  parseCreateUploadRequest,
  parseEnrollRequest,
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
