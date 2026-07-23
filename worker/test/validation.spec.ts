import { describe, expect, it } from "vitest";
import { parseArchiveAccessRequest } from "../src/archive-access";
import {
  parseCompleteUploadRequest,
  parseCreateDicomUploadRequest,
} from "../src/validation";

const archiveId = "1".repeat(24);
const request = {
  format: "dicom-series-v1",
  client_version: "0.5.0",
  deidentification: {
    policy_id: "scaling-neuro.dicom-deidentification",
    policy_version: "2.0.0",
  },
  series: [
    {
      series_archive_id: archiveId,
      series_id: "2".repeat(24),
      subject_id: "3".repeat(24),
      session_id: "4".repeat(24),
      protocol_group_id: "5".repeat(24),
      dicom_count: 240,
      series_kind: "functional_epi",
      archive_route: "functional-epi-v1",
      pixel_data_policy: "scanner-native-not-defaced",
      archive: {
        format: "dicom-tar-zstd",
        relative_key: `${archiveId}/dicom.tar.zst`,
        size: 4096,
        sha256: "a".repeat(64),
      },
    },
  ],
};

describe("minimal EPI contract", () => {
  it("accepts exactly one functional EPI archive", () => {
    expect(parseCreateDicomUploadRequest(request)).toEqual(request);
  });

  it("rejects structural and multi-series upload requests", () => {
    expect(() =>
      parseCreateDicomUploadRequest({
        ...request,
        series: [{ ...request.series[0], series_kind: "structural_t1w" }],
      }),
    ).toThrow(/functional EPI archive contract/u);
    expect(() =>
      parseCreateDicomUploadRequest({
        ...request,
        series: [...request.series, request.series[0]],
      }),
    ).toThrow(/exactly one functional EPI archive/u);
  });

  it("accepts a single multipart completion receipt", () => {
    const completion = {
      objects: [
        {
          key: `dicom/v1/site/project/upload/${archiveId}/dicom.tar.zst`,
          size: 4096,
          sha256: "a".repeat(64),
          parts: [{ part_number: 1, etag: "etag-1" }],
        },
      ],
    };
    expect(parseCompleteUploadRequest(completion)).toEqual(completion);
  });

  it("requires archive users to identify their lab and participate", () => {
    expect(
      parseArchiveAccessRequest({
        contact_name: "Researcher Name",
        contact_email: "researcher@example.edu",
        institution_name: "Example University",
        lab_name: "Example Lab",
        participation_commitment: true,
      }),
    ).toMatchObject({
      contact_email: "researcher@example.edu",
      participation_commitment: true,
    });
    expect(() =>
      parseArchiveAccessRequest({
        contact_name: "Researcher Name",
        contact_email: "researcher@example.edu",
        institution_name: "Example University",
        lab_name: "Example Lab",
        participation_commitment: false,
      }),
    ).toThrow(/participation must be confirmed/u);
  });
});
