import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { buildClusterLaunchRequest } from "../src/cluster";

describe("upload-triggered cluster launch signing", () => {
  it("signs the exact canonical receipt event", async () => {
    const uploadId = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    const timestamp = "1700000000";
    const nonce = "12345678-1234-4234-9234-123456789abc";
    const request = await buildClusterLaunchRequest(
      env,
      uploadId,
      timestamp,
      nonce,
    );
    const body = await request.text();
    expect(body).toBe(
      '{"event":"dicom-upload-committed","upload_id":"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"}',
    );
    expect(request.headers.get("x-scaling-neuro-timestamp")).toBe(timestamp);
    expect(request.headers.get("x-scaling-neuro-nonce")).toBe(nonce);

    const binary = atob(env.CLUSTER_LAUNCH_HMAC_KEY);
    const raw = Uint8Array.from(binary, (character) => character.charCodeAt(0));
    const key = await crypto.subtle.importKey(
      "raw",
      raw,
      { name: "HMAC", hash: "SHA-256" },
      false,
      ["verify"],
    );
    const signature = request.headers
      .get("x-scaling-neuro-signature")!
      .replace(/^v1=/u, "");
    const signatureBytes = Uint8Array.from(
      signature.match(/.{2}/gu)!,
      (byte) => Number.parseInt(byte, 16),
    );
    expect(
      await crypto.subtle.verify(
        "HMAC",
        key,
        signatureBytes,
        new TextEncoder().encode(`${timestamp}\n${nonce}\n${body}`),
      ),
    ).toBe(true);
  });
});
