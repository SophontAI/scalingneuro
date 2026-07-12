import { describe, expect, it } from "vitest";
import {
  credentialTtl,
  deleteObject,
  deletePrefix,
  presignUploadPart,
} from "../src/r2";

describe("R2 UploadPart query signing", () => {
  it("keeps exact-part grants fixed to fifteen minutes", () => {
    expect(credentialTtl({} as never)).toBe(900);
    expect(() =>
      credentialTtl({ CREDENTIAL_TTL_SECONDS: "901" } as never),
    ).toThrow(/TTL configuration/u);
  });

  it("matches the frozen SigV4 vector verified against live R2", async () => {
    const signed = await presignUploadPart(
      {
        R2_ACCOUNT_ID: "0123456789abcdef0123456789abcdef",
        R2_PARENT_ACCESS_KEY_ID: "TESTACCESSKEY",
        R2_PARENT_SECRET_ACCESS_KEY: "test-secret-key",
        R2_BUCKET_NAME: "test-bucket",
        CREDENTIAL_TTL_SECONDS: "900",
      } as never,
      {
        key: "archive/v1/site/project/upload/scan bold.nii.gz",
        uploadId: "multipart+/=",
        partNumber: 7,
        size: 123_456,
        sha256: "a".repeat(64),
      },
      new Date("2026-07-12T03:04:05Z"),
    );
    const url = new URL(signed.url);
    expect(url.searchParams.get("X-Amz-Signature")).toBe(
      "58c5174f71743d86463c30c1709c2714eb741af265598fd8ad88bf899d588caa",
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

describe("verified R2 deletion", () => {
  const environment = (archive: R2Bucket) =>
    ({
      ARCHIVE: archive,
      R2_ACCOUNT_ID: "0123456789abcdef0123456789abcdef",
      R2_PARENT_ACCESS_KEY_ID: "TESTACCESSKEY",
      R2_PARENT_SECRET_ACCESS_KEY: "test-secret-key",
      R2_BUCKET_NAME: "test-bucket",
    }) as never;
  const acceptedNoOp = (async () =>
    new Response(null, { status: 204 })) as typeof fetch;

  it("deletes every prefix object individually and relists from the start", async () => {
    const objects = new Set(["archive/upload/a", "archive/upload/b"]);
    const deleteArguments: string[] = [];
    const bucket = {
      async list({ prefix }: { prefix: string }) {
        return {
          objects: [...objects]
            .filter((key) => key.startsWith(prefix))
            .map((key) => ({ key })),
          truncated: false,
        };
      },
      async delete(key: string) {
        deleteArguments.push(key);
        objects.delete(key);
      },
      async head(key: string) {
        return objects.has(key) ? ({ key } as R2Object) : null;
      },
    } as unknown as R2Bucket;

    await deletePrefix(environment(bucket), "archive/upload/");

    expect(deleteArguments).toEqual([
      "archive/upload/a",
      "archive/upload/b",
    ]);
    expect(objects.size).toBe(0);
  });

  it("fails closed when a prefix deletion silently leaves an object", async () => {
    const bucket = {
      async list() {
        return { objects: [{ key: "archive/upload/a" }], truncated: false };
      },
      async delete() {},
      async head() {
        return { key: "archive/upload/a" } as R2Object;
      },
    } as unknown as R2Bucket;
    await expect(
      deletePrefix(environment(bucket), "archive/upload/", acceptedNoOp),
    ).rejects.toThrow(
      /remained/u,
    );
  });

  it("falls back to a signed S3 DELETE and still requires absence", async () => {
    let present = true;
    const bucket = {
      async delete() {},
      async head() {
        return present ? ({ key: "manifest" } as R2Object) : null;
      },
    } as unknown as R2Bucket;
    const requests: Array<{ url: string; method: string | undefined }> = [];
    const successfulFallback = (async (
      input: RequestInfo | URL,
      init?: RequestInit,
    ) => {
      requests.push({ url: String(input), method: init?.method });
      present = false;
      return new Response(null, { status: 204 });
    }) as typeof fetch;
    await deleteObject(environment(bucket), "manifest", successfulFallback);
    expect(requests).toEqual([
      {
        url: "https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com/test-bucket/manifest",
        method: "DELETE",
      },
    ]);

    const noOpBucket = {
      async delete() {},
      async head() {
        return { key: "manifest" } as R2Object;
      },
    } as unknown as R2Bucket;
    await expect(
      deleteObject(environment(noOpBucket), "manifest", acceptedNoOp),
    ).rejects.toThrow(/remained/u);
  });
});
