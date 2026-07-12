import { env } from "cloudflare:workers";
import Ajv2020 from "ajv/dist/2020";
import {
  createExecutionContext,
  waitOnExecutionContext,
} from "cloudflare:test";
import { describe, expect, it, vi } from "vitest";
import scanSidecarExample from "../../schemas/examples/scan-sidecar-v1.example.json";
import archiveManifestSchema from "../../schemas/archive-manifest-v1.schema.json";
import commonSchema from "../../schemas/common-v1.schema.json";
import enrollmentResponseSchema from "../../schemas/enrollment-response-v1.schema.json";
import { canonicalJson, sha256Hex } from "../src/crypto";
import { fetchHandler } from "../src/index";
import { cleanupAbandoned } from "../src/service";

const ADMIN_TOKEN = "test-admin-token-with-sufficient-entropy";
const archiveAjv = new Ajv2020({ strict: true, validateFormats: false });
archiveAjv.addSchema(commonSchema);
const validateArchiveManifest = archiveAjv.compile(archiveManifestSchema);
const validateEnrollmentResponse = archiveAjv.compile(
  enrollmentResponseSchema,
);

async function functionalNiftiFixture(): Promise<{
  compressed: Uint8Array<ArrayBuffer>;
  uncompressedSha256: string;
  image: {
    dimensions: number[];
    voxel_size_mm: number[];
    datatype: string;
    bits_per_voxel: number;
    affine: number[][];
    orientation: string;
    volume_count: number;
    tr_seconds: number;
  };
}> {
  const dimensions = [8, 8, 8, 10];
  const uncompressed = new Uint8Array(
    352 + dimensions.reduce((product, value) => product * value, 1) * 2,
  );
  const view = new DataView(uncompressed.buffer);
  view.setInt32(0, 348, true);
  view.setInt16(40, 4, true);
  dimensions.forEach((value, index) =>
    view.setInt16(42 + index * 2, value, true),
  );
  view.setInt16(70, 4, true);
  view.setInt16(72, 16, true);
  view.setFloat32(80, 2, true);
  view.setFloat32(84, 2, true);
  view.setFloat32(88, 2, true);
  view.setFloat32(92, 1.5, true);
  view.setFloat32(108, 352, true);
  uncompressed[123] = 10; // millimeters + seconds
  view.setInt16(254, 1, true);
  view.setFloat32(280, 2, true);
  view.setFloat32(300, 2, true);
  view.setFloat32(320, 2, true);
  uncompressed.set([0x6e, 0x2b, 0x31, 0], 344);
  const body = new Response(uncompressed).body;
  if (!body) throw new Error("fixture stream unavailable");
  const compressed = new Uint8Array(
    await new Response(body.pipeThrough(new CompressionStream("gzip"))).arrayBuffer(),
  );
  return {
    compressed,
    uncompressedSha256: await sha256Hex(uncompressed),
    image: {
      dimensions,
      voxel_size_mm: [2, 2, 2],
      datatype: "int16",
      bits_per_voxel: 16,
      affine: [
        [2, 0, 0, 0],
        [0, 2, 0, 0],
        [0, 0, 2, 0],
        [0, 0, 0, 1],
      ],
      orientation: "RAS",
      volume_count: 10,
      tr_seconds: 1.5,
    },
  };
}

async function call(
  method: string,
  path: string,
  body?: unknown,
  token?: string,
): Promise<Response> {
  const headers = new Headers();
  if (body !== undefined) headers.set("content-type", "application/json");
  if (token) headers.set("authorization", `Bearer ${token}`);
  const init: RequestInit = {
    method,
    headers,
  };
  if (body !== undefined) init.body = JSON.stringify(body);
  const request = new Request(`https://scalingneuro.com${path}`, init);
  const ctx = createExecutionContext();
  const response = await fetchHandler(request, env, ctx);
  await waitOnExecutionContext(ctx);
  return response;
}

async function createInvite(): Promise<Record<string, unknown>> {
  const response = await call(
    "POST",
    "/v1/admin/invites",
    {
      site_slug: "princeton",
      site_name: "Princeton Neuroscience Institute",
      project_slug: "epi-pilot",
      project_name: "EPI Pilot",
      consent_policy_version: "pilot-1",
      expires_in_seconds: 3600,
      max_uses: 1,
    },
    ADMIN_TOKEN,
  );
  expect(response.status).toBe(201);
  return response.json<Record<string, unknown>>();
}

function freshDeviceToken(): string {
  const entropy =
    crypto.randomUUID().replaceAll("-", "") +
    crypto.randomUUID().replaceAll("-", "").slice(0, 11);
  return `sn_device_${entropy}`;
}

function enrollmentRequest(
  inviteCode: string,
  deviceName = "scanner-console",
): Record<string, string> {
  return {
    invite_code: inviteCode,
    enrollment_id: crypto.randomUUID(),
    device_token: freshDeviceToken(),
    device_name: deviceName,
    client_version: "0.1.0",
    platform: "linux-x64",
  };
}

async function enrollDevice(
  inviteCode: string,
): Promise<Record<string, unknown>> {
  const response = await call(
    "POST",
    "/v1/enroll",
    enrollmentRequest(inviteCode),
  );
  expect(response.status).toBe(201);
  return response.json<Record<string, unknown>>();
}

describe("ingestion control plane", () => {
  it("runs enrollment, strict upload commit, idempotency, and withdrawal end to end", async () => {
    const health = await call("GET", "/health");
    expect(health.status).toBe(200);
    expect(await health.json()).toMatchObject({
      status: "ok",
      service: "scaling-neuro-ingest",
    });

    const invite = await createInvite();
    const enrollment = await enrollDevice(invite.invite_code as string);
    expect(
      validateEnrollmentResponse(enrollment),
      archiveAjv.errorsText(validateEnrollmentResponse.errors),
    ).toBe(true);
    const deviceToken = enrollment.device_token as string;
    expect(deviceToken).toMatch(/^sn_device_/u);
    expect(enrollment.pseudonym_key_b64).toMatch(/^[A-Za-z0-9+/]{43}=$/u);

    const reusedInvite = await call(
      "POST",
      "/v1/enroll",
      enrollmentRequest(invite.invite_code as string, "second-console"),
    );
    expect(reusedInvite.status).toBe(401);
    expect(await reusedInvite.json()).toMatchObject({
      error: { code: "INVALID_INVITE" },
    });

    const fixture = await functionalNiftiFixture();
    const niiBytes = fixture.compressed;
    const niiSha256 = await sha256Hex(niiBytes);
    const sidecar = structuredClone(scanSidecarExample);
    sidecar.bundle_id = "1".repeat(24);
    sidecar.series_id = "2".repeat(24);
    sidecar.subject_id = "3".repeat(24);
    sidecar.session_id = "4".repeat(24);
    sidecar.protocol_group_id = "5".repeat(24);
    sidecar.files.nifti.filename = "scan_bold.nii.gz";
    sidecar.files.nifti.size_bytes = niiBytes.byteLength;
    sidecar.files.nifti.sha256 = niiSha256;
    sidecar.files.nifti.uncompressed_sha256 = fixture.uncompressedSha256;
    Object.assign(sidecar.image, fixture.image);
    const metadataBytes = new TextEncoder().encode(JSON.stringify(sidecar));
    const bundle = {
      bundle_id: "1".repeat(24),
      series_id: "2".repeat(24),
      subject_id: "3".repeat(24),
      session_id: "4".repeat(24),
      protocol_group_id: "5".repeat(24),
      nii: {
        relative_key: `${"1".repeat(24)}/scan_bold.nii.gz`,
        size: niiBytes.byteLength,
        sha256: niiSha256,
        uncompressed_sha256: fixture.uncompressedSha256,
      },
      metadata: {
        relative_key: `${"1".repeat(24)}/scan_bold.json`,
        size: metadataBytes.byteLength,
        sha256: await sha256Hex(metadataBytes),
      },
    };

    const allocation = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle], client_version: "0.1.0" },
      deviceToken,
    );
    expect(allocation.status).toBe(201);
    const allocated = await allocation.json<Record<string, unknown>>();
    expect(Object.keys(allocated).sort()).toEqual(
      [
        "multipart_objects",
        "object_prefix",
        "status",
        "upload_id",
      ].sort(),
    );
    const uploadId = allocated.upload_id as string;
    const prefix = allocated.object_prefix as string;
    expect(prefix).toBe(
      `archive/v1/${enrollment.site_id as string}/${enrollment.project_id as string}/${uploadId}/`,
    );
    const multipartObjects = allocated.multipart_objects as Array<{
      key: string;
      upload_id: string;
      part_size: number;
    }>;
    const niiKey = `${prefix}${bundle.nii.relative_key}`;
    const metadataKey = `${prefix}${bundle.metadata.relative_key}`;
    const niiMultipart = multipartObjects.find(
      (object) => object.key === niiKey,
    )!;
    const metadataMultipart = multipartObjects.find(
      (object) => object.key === metadataKey,
    )!;
    expect(niiMultipart.part_size).toBe(64 * 1024 * 1024);
    const oversizedPart = await call(
      "POST",
      `/v1/uploads/${uploadId}/parts`,
      {
        key: niiKey,
        part_number: 1,
        size: niiBytes.byteLength + 1,
        sha256: await sha256Hex(niiBytes),
      },
      deviceToken,
    );
    expect(oversizedPart.status).toBe(409);
    const signedPart = await call(
      "POST",
      `/v1/uploads/${uploadId}/parts`,
      {
        key: niiKey,
        part_number: 1,
        size: niiBytes.byteLength,
        sha256: await sha256Hex(niiBytes),
      },
      deviceToken,
    );
    expect(signedPart.status).toBe(200);
    const signed = await signedPart.json<{
      url: string;
      headers: Record<string, string>;
      expires_at: string;
    }>();
    const signedUrl = new URL(signed.url);
    expect(signedUrl.hostname).toBe("test-account.r2.cloudflarestorage.com");
    expect(signedUrl.searchParams.get("partNumber")).toBe("1");
    expect(signedUrl.searchParams.get("uploadId")).toBe(
      niiMultipart.upload_id,
    );
    expect(signed.headers).toEqual({
      "content-length": String(niiBytes.byteLength),
      "x-amz-content-sha256": await sha256Hex(niiBytes),
    });
    expect(Date.parse(signed.expires_at)).toBeGreaterThan(Date.now());
    const niiPart = await env.ARCHIVE.resumeMultipartUpload(
      niiKey,
      niiMultipart.upload_id,
    ).uploadPart(1, niiBytes);
    const metadataPart = await env.ARCHIVE.resumeMultipartUpload(
      metadataKey,
      metadataMultipart.upload_id,
    ).uploadPart(1, metadataBytes);

    const completionBody = {
      objects: [
        {
          key: niiKey,
          size: bundle.nii.size,
          sha256: bundle.nii.sha256,
          parts: [
            {
              part_number: niiPart.partNumber,
              etag: `"${niiPart.etag.replaceAll('"', "")}"`,
            },
          ],
        },
        {
          key: metadataKey,
          size: bundle.metadata.size,
          sha256: bundle.metadata.sha256,
          parts: [
            { part_number: metadataPart.partNumber, etag: metadataPart.etag },
          ],
        },
      ],
    };
    const badComplete = await call(
      "POST",
      `/v1/uploads/${uploadId}/complete`,
      {
        objects: completionBody.objects.map((object, index) =>
          index === 0 ? { ...object, sha256: "0".repeat(64) } : object,
        ),
      },
      deviceToken,
    );
    expect(badComplete.status).toBe(409);
    expect(await badComplete.json()).toMatchObject({
      error: { code: "OBJECT_MISMATCH" },
    });
    const headSpy = vi
      .spyOn(env.ARCHIVE, "head")
      .mockResolvedValue(null);
    const temporarilyInvisible = await (async () => {
      try {
        return await call(
          "POST",
          `/v1/uploads/${uploadId}/complete`,
          completionBody,
          deviceToken,
        );
      } finally {
        headSpy.mockRestore();
      }
    })();
    expect(temporarilyInvisible.status).toBe(502);
    expect(await temporarilyInvisible.json()).toMatchObject({
      error: { code: "STORAGE_UNAVAILABLE" },
    });
    expect(
      await env.DB.prepare("SELECT status FROM uploads WHERE id = ?1")
        .bind(uploadId)
        .first<string>("status"),
    ).not.toBe("expired");

    const resumeSpy = vi
      .spyOn(env.ARCHIVE, "resumeMultipartUpload")
      .mockImplementation(
        () =>
          ({
            // Reproduce the live binding: an idempotent completion result may
            // not expose custom metadata even though persisted HEAD does.
            complete: async () => ({}) as R2Object,
          }) as unknown as R2MultipartUpload,
      );
    const completion = await (async () => {
      try {
        return await call(
          "POST",
          `/v1/uploads/${uploadId}/complete`,
          completionBody,
          deviceToken,
        );
      } finally {
        resumeSpy.mockRestore();
      }
    })();
    expect(completion.status, await completion.clone().text()).toBe(200);
    const committed = await completion.json<Record<string, unknown>>();
    expect(committed).toMatchObject({
      upload_id: uploadId,
      status: "committed",
    });
    expect(Object.keys(committed).sort()).toEqual(
      [
        "committed_at",
        "consent_policy_version",
        "created_at",
        "manifest",
        "object_prefix",
        "series_count",
        "status",
        "total_bytes",
        "updated_at",
        "upload_id",
      ].sort(),
    );

    const manifestInfo = committed.manifest as { key: string; sha256: string };
    const manifestObject = await env.ARCHIVE.get(manifestInfo.key);
    expect(manifestObject).not.toBeNull();
    const manifestBytes = await manifestObject!.arrayBuffer();
    expect(await sha256Hex(new Uint8Array(manifestBytes))).toBe(
      manifestInfo.sha256,
    );
    const manifest = JSON.parse(
      new TextDecoder().decode(manifestBytes),
    ) as Record<string, unknown>;
    expect(manifest).toMatchObject({
      schema_version: "scaling-neuro.archive-manifest.v1",
      upload_id: uploadId,
    });
    expect(validateArchiveManifest(manifest), archiveAjv.errorsText(validateArchiveManifest.errors)).toBe(
      true,
    );
    expect(Object.keys(manifest).sort()).toEqual(
      [
        "archive_prefix",
        "bundles",
        "client_version",
        "committed_at",
        "consent_policy_version",
        "control_plane",
        "created_at",
        "project_id",
        "schema_version",
        "site_id",
        "upload_id",
      ].sort(),
    );
    const archivedBundle = (
      manifest.bundles as Array<Record<string, unknown>>
    )[0]!;
    expect(archivedBundle.nii).toMatchObject({
      uncompressed_sha256: bundle.nii.uncompressed_sha256,
    });
    expect(archivedBundle.bundle_hash).toBe(
      await sha256Hex(
        canonicalJson({
          series_id: bundle.series_id,
          subject_id: bundle.subject_id,
          session_id: bundle.session_id,
          nii: { uncompressed_sha256: bundle.nii.uncompressed_sha256 },
        }),
      ),
    );

    const replay = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle], client_version: "0.1.0" },
      deviceToken,
    );
    expect(replay.status).toBe(200);
    expect(await replay.json()).toMatchObject({
      upload_id: uploadId,
      status: "committed",
    });

    const tokenRow = await env.DB.prepare(
      "SELECT token_hash FROM devices WHERE id = ?1",
    )
      .bind(enrollment.device_id)
      .first<{ token_hash: string }>();
    expect(tokenRow?.token_hash).toHaveLength(64);
    expect(tokenRow?.token_hash).not.toContain(deviceToken);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM catalog_series WHERE upload_id = ?1",
      )
        .bind(uploadId)
        .first<{ count: number }>("count"),
    ).toBe(1);
    expect(
      await env.DB.prepare(
        "SELECT protocol_group_id FROM catalog_series WHERE upload_id = ?1",
      )
        .bind(uploadId)
        .first<string>("protocol_group_id"),
    ).toBe(bundle.protocol_group_id);

    const withdrawal = await call(
      "POST",
      `/v1/admin/uploads/${uploadId}/withdraw`,
      undefined,
      ADMIN_TOKEN,
    );
    expect(withdrawal.status).toBe(200);
    expect(await withdrawal.json()).toMatchObject({
      upload_id: uploadId,
      status: "withdrawn",
    });
    expect(await env.ARCHIVE.head(niiKey)).toBeNull();
    expect(await env.ARCHIVE.head(metadataKey)).toBeNull();
    expect(await env.ARCHIVE.head(manifestInfo.key)).toBeNull();

    const replacementInvite = await createInvite();
    const replacement = await enrollDevice(
      replacementInvite.invite_code as string,
    );
    const tombstonedReplay = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle], client_version: "0.1.0" },
      replacement.device_token as string,
    );
    expect(tombstonedReplay.status).toBe(409);
    expect(await tombstonedReplay.json()).toMatchObject({
      error: { code: "DUPLICATE_BUNDLE" },
    });
  });

  it("replays a lost enrollment response without consuming the invite twice", async () => {
    const invite = await createInvite();
    const request = enrollmentRequest(invite.invite_code as string);

    // Treat the first successful response as lost, then send the exact pending
    // operation again. The Worker must recover the same device and secrets.
    const firstResponse = await call("POST", "/v1/enroll", request);
    expect(firstResponse.status).toBe(201);
    const first = await firstResponse.json<Record<string, unknown>>();
    const replayResponse = await call("POST", "/v1/enroll", request);
    expect(replayResponse.status).toBe(201);
    const replay = await replayResponse.json<Record<string, unknown>>();
    expect(replay).toEqual(first);
    expect(first).toMatchObject({
      enrollment_id: request.enrollment_id,
      device_token: request.device_token,
    });

    const consumed = await env.DB.prepare(
      "SELECT uses FROM invites WHERE id = ?1",
    )
      .bind(invite.invite_id)
      .first<number>("uses");
    expect(consumed).toBe(1);
    const devices = await env.DB.prepare(
      "SELECT COUNT(*) AS count FROM devices WHERE enrollment_id = ?1",
    )
      .bind(request.enrollment_id)
      .first<number>("count");
    expect(devices).toBe(1);
    const auditEvents = await env.DB.prepare(
      "SELECT COUNT(*) AS count FROM audit_events WHERE event_type = 'device.enrolled' AND subject_id = ?1",
    )
      .bind(first.device_id)
      .first<number>("count");
    expect(auditEvents).toBe(1);
    const storedTokenHash = await env.DB.prepare(
      "SELECT token_hash FROM devices WHERE enrollment_id = ?1",
    )
      .bind(request.enrollment_id)
      .first<string>("token_hash");
    expect(storedTokenHash).toMatch(/^[a-f0-9]{64}$/u);
    expect(storedTokenHash).not.toBe(request.device_token);

    const mismatchedReplay = await call("POST", "/v1/enroll", {
      ...request,
      device_token: freshDeviceToken(),
    });
    expect(mismatchedReplay.status).toBe(401);
    expect(await mismatchedReplay.json()).toMatchObject({
      error: { code: "INVALID_INVITE" },
    });
    expect(
      await env.DB.prepare("SELECT uses FROM invites WHERE id = ?1")
        .bind(invite.invite_id)
        .first<number>("uses"),
    ).toBe(1);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM devices WHERE invite_id = ?1",
      )
        .bind(invite.invite_id)
        .first<number>("count"),
    ).toBe(1);
  });

  it("rejects unscoped access and unsafe request extensions", async () => {
    const unauthorized = await call(
      "GET",
      "/v1/uploads/00000000-0000-4000-8000-000000000000",
    );
    expect(unauthorized.status).toBe(401);

    const invalid = await call(
      "POST",
      "/v1/admin/invites",
      {
        site_slug: "lab",
        site_name: "Lab",
        project_slug: "pilot",
        project_name: "Pilot",
        consent_policy_version: "v1",
        patient_id: "forbidden",
      },
      ADMIN_TOKEN,
    );
    expect(invalid.status).toBe(400);
    expect(await invalid.json()).toMatchObject({
      error: { code: "INVALID_REQUEST" },
    });
  });

  it("revokes unused invites before enrollment", async () => {
    const invite = await createInvite();
    const revocation = await call(
      "POST",
      `/v1/admin/invites/${invite.invite_id as string}/revoke`,
      undefined,
      ADMIN_TOKEN,
    );
    expect(revocation.status).toBe(200);
    expect(await revocation.json()).toMatchObject({ status: "revoked" });

    const enrollment = await call(
      "POST",
      "/v1/enroll",
      enrollmentRequest(
        invite.invite_code as string,
        "revoked-invite-device",
      ),
    );
    expect(enrollment.status).toBe(401);
    expect(await enrollment.json()).toMatchObject({
      error: { code: "INVALID_INVITE" },
    });
  });

  it("revokes devices and purges their abandoned upload prefixes", async () => {
    const invite = await createInvite();
    const enrollment = await enrollDevice(invite.invite_code as string);
    const deviceToken = enrollment.device_token as string;
    const bundle = {
      bundle_id: "5".repeat(24),
      series_id: "6".repeat(24),
      subject_id: "7".repeat(24),
      session_id: "8".repeat(24),
      protocol_group_id: "9".repeat(24),
      nii: {
        relative_key: `${"5".repeat(24)}/bold.nii.gz`,
        size: 352,
        sha256: "c".repeat(64),
        uncompressed_sha256: "e".repeat(64),
      },
      metadata: {
        relative_key: `${"5".repeat(24)}/bold.json`,
        size: 2,
        sha256: "d".repeat(64),
      },
    };
    const allocation = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle], client_version: "0.1.0" },
      deviceToken,
    );
    expect(allocation.status).toBe(201);
    const allocated = await allocation.json<Record<string, unknown>>();
    const uploadId = allocated.upload_id as string;
    const multipartObject = (
      allocated.multipart_objects as Array<{
        key: string;
        upload_id: string;
      }>
    )[0]!;
    await env.ARCHIVE.resumeMultipartUpload(
      multipartObject.key,
      multipartObject.upload_id,
    ).uploadPart(1, "partial");

    const revocation = await call(
      "POST",
      `/v1/admin/devices/${enrollment.device_id as string}/revoke`,
      undefined,
      ADMIN_TOKEN,
    );
    expect(revocation.status).toBe(200);
    const blocked = await call(
      "GET",
      `/v1/uploads/${uploadId}`,
      undefined,
      deviceToken,
    );
    expect(blocked.status).toBe(403);

    await cleanupAbandoned(env);
    await expect(
      env.ARCHIVE.resumeMultipartUpload(
        multipartObject.key,
        multipartObject.upload_id,
      ).uploadPart(1, "after-abort"),
    ).rejects.toThrow();
    expect(
      await env.DB.prepare("SELECT status FROM uploads WHERE id = ?1")
        .bind(uploadId)
        .first<string>("status"),
    ).toBe("expired");
  });
});
