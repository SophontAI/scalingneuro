import { describe, expect, it } from "vitest";
import {
  credentialTtl,
  presignUploadPart,
} from "../src/r2";

describe("R2 UploadPart query signing", () => {
  it("keeps exact-part grants fixed to fifteen minutes", () => {
    expect(credentialTtl({} as never)).toBe(900);
    expect(() =>
      credentialTtl({ CREDENTIAL_TTL_SECONDS: "901" } as never),
    ).toThrow(/TTL configuration/u);
  });

  it("matches the frozen SigV4 vector for an EPI archive part", async () => {
    const signed = await presignUploadPart(
      {
        R2_ACCOUNT_ID: "0123456789abcdef0123456789abcdef",
        R2_PARENT_ACCESS_KEY_ID: "TESTACCESSKEY",
        R2_PARENT_SECRET_ACCESS_KEY: "test-secret-key",
        R2_BUCKET_NAME: "test-bucket",
        CREDENTIAL_TTL_SECONDS: "900",
      } as never,
      {
        key: "archive/v1/site/project/upload/series.dicom.tar.zst",
        uploadId: "multipart+/=",
        partNumber: 7,
        size: 123_456,
        sha256: "a".repeat(64),
      },
      new Date("2026-07-12T03:04:05Z"),
    );
    const url = new URL(signed.url);
    expect(url.searchParams.get("X-Amz-Signature")).toBe(
      "b69995221aecbd0d544930c1e57551029f402899c7ca4ab5044620ad1fd9121d",
    );
    expect(url.searchParams.get("X-Amz-SignedHeaders")).toBe(
      "content-length;host;x-amz-content-sha256",
    );
    expect(signed.headers).toEqual({
      "content-length": "123456",
      "x-amz-content-sha256": "a".repeat(64),
    });
  });
});
