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
const CLIENT_VERSION = scanSidecarExample.conversion.client_version;
const archiveAjv = new Ajv2020({ strict: true, validateFormats: false });
archiveAjv.addSchema(commonSchema);
const validateArchiveManifest = archiveAjv.compile(archiveManifestSchema);
const validateEnrollmentResponse = archiveAjv.compile(enrollmentResponseSchema);

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
    await new Response(
      body.pipeThrough(new CompressionStream("gzip")),
    ).arrayBuffer(),
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
    client_version: CLIENT_VERSION,
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
    const staleEnrollmentRequest = enrollmentRequest(
      invite.invite_code as string,
    );
    staleEnrollmentRequest.client_version = "0.1.0";
    const staleEnrollment = await call(
      "POST",
      "/v1/enroll",
      staleEnrollmentRequest,
    );
    expect(staleEnrollment.status).toBe(426);
    expect(await staleEnrollment.json()).toMatchObject({
      error: {
        code: "CLIENT_UPDATE_REQUIRED",
        details: { minimum_client_version: "0.1.1" },
      },
    });
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

    const staleAllocation = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle], client_version: "0.1.0" },
      deviceToken,
    );
    expect(staleAllocation.status).toBe(426);
    expect(await staleAllocation.json()).toMatchObject({
      error: {
        code: "CLIENT_UPDATE_REQUIRED",
        details: { minimum_client_version: "0.1.1" },
      },
    });

    const allocation = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle], client_version: CLIENT_VERSION },
      deviceToken,
    );
    expect(allocation.status).toBe(201);
    const allocated = await allocation.json<Record<string, unknown>>();
    expect(Object.keys(allocated).sort()).toEqual(
      ["multipart_objects", "object_prefix", "status", "upload_id"].sort(),
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

    // Allocate the same scientific bundle from another enrolled workstation
    // before either upload commits. The later completion must converge on the
    // winner and purge its now-redundant R2 prefix without manual recovery.
    const raceInvite = await createInvite();
    const raceDevice = await enrollDevice(raceInvite.invite_code as string);
    const raceToken = raceDevice.device_token as string;
    const raceAllocationResponse = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle], client_version: CLIENT_VERSION },
      raceToken,
    );
    expect(raceAllocationResponse.status).toBe(201);
    const raceAllocation =
      await raceAllocationResponse.json<Record<string, unknown>>();
    const raceUploadId = raceAllocation.upload_id as string;
    const racePrefix = raceAllocation.object_prefix as string;
    const raceMultipartObjects = raceAllocation.multipart_objects as Array<{
      key: string;
      upload_id: string;
      part_size: number;
    }>;
    const raceNiiKey = `${racePrefix}${bundle.nii.relative_key}`;
    const raceMetadataKey = `${racePrefix}${bundle.metadata.relative_key}`;
    const raceNiiMultipart = raceMultipartObjects.find(
      (object) => object.key === raceNiiKey,
    )!;
    const raceMetadataMultipart = raceMultipartObjects.find(
      (object) => object.key === raceMetadataKey,
    )!;

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
    expect(signedUrl.searchParams.get("uploadId")).toBe(niiMultipart.upload_id);
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
    const headSpy = vi.spyOn(env.ARCHIVE, "head").mockResolvedValue(null);
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

    // The first bounded finalization request completed the NIfTI in R2 before
    // the simulated visibility failure. Materialize the sidecar as well to
    // reproduce the live upload: both objects exist, but D1 has only a legacy
    // sidecar receipt and no scientifically verified pair.
    await env.ARCHIVE.resumeMultipartUpload(
      metadataKey,
      metadataMultipart.upload_id,
    ).complete([
      {
        partNumber: metadataPart.partNumber,
        etag: metadataPart.etag.replaceAll('"', ""),
      },
    ]);

    // Reproduce the production recovery shape left by the former verifier:
    // one small sidecar has an object-completion receipt, but no atomic
    // NIfTI/sidecar scientific-validation checkpoint. The new verifier must
    // not mistake that legacy per-object marker for a verified bundle.
    const completedMetadata = await env.ARCHIVE.head(metadataKey);
    expect(completedMetadata).not.toBeNull();
    await env.DB.prepare(
      `UPDATE upload_objects
       SET completed_at = ?1, etag = ?2
       WHERE upload_id = ?3 AND object_key = ?4`,
    )
      .bind(
        Math.floor(Date.now() / 1000),
        completedMetadata!.etag,
        uploadId,
        metadataKey,
      )
      .run();
    const verifyingStatus = await call(
      "GET",
      `/v1/uploads/${uploadId}`,
      undefined,
      deviceToken,
    );
    expect(await verifyingStatus.json()).toMatchObject({
      status: "uploading",
      verification: {
        phase: "finalizing_objects",
        finalized_series: 0,
        verified_series: 0,
        total_series: 1,
      },
    });

    // An active verifier is normal progress, not an API conflict. This makes
    // concurrent/lost-response recovery a lightweight status poll.
    const busyUntil = Math.floor(Date.now() / 1000) + 60;
    await env.DB.prepare(
      `UPDATE uploads SET operation_token = 'busy-test',
         operation_kind = 'verify', operation_expires_at = ?1
       WHERE id = ?2`,
    )
      .bind(busyUntil, uploadId)
      .run();
    const busy = await call(
      "POST",
      `/v1/uploads/${uploadId}/complete`,
      completionBody,
      deviceToken,
    );
    expect(busy.status).toBe(200);
    expect(await busy.json()).toMatchObject({
      status: "uploading",
      verification: {
        phase: "finalizing_objects",
        finalized_series: 0,
        verified_series: 0,
      },
    });
    await env.DB.prepare(
      `UPDATE uploads SET operation_token = NULL, operation_kind = NULL,
         operation_expires_at = NULL WHERE id = ?1`,
    )
      .bind(uploadId)
      .run();

    const resumeSpy = vi.spyOn(env.ARCHIVE, "resumeMultipartUpload");
    const finalization = await call(
      "POST",
      `/v1/uploads/${uploadId}/complete`,
      completionBody,
      deviceToken,
    );
    expect(finalization.status, await finalization.clone().text()).toBe(200);
    expect(await finalization.json()).toMatchObject({
      upload_id: uploadId,
      status: "uploading",
      verification: {
        phase: "validating_scans",
        finalized_series: 1,
        verified_series: 0,
        total_series: 1,
      },
    });
    expect(resumeSpy).not.toHaveBeenCalled();
    resumeSpy.mockRestore();
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM upload_objects
         WHERE upload_id = ?1 AND completed_at IS NOT NULL AND etag IS NOT NULL`,
      )
        .bind(uploadId)
        .first<number>("count"),
    ).toBe(2);

    const verification = await call(
      "POST",
      `/v1/uploads/${uploadId}/complete`,
      completionBody,
      deviceToken,
    );
    expect(verification.status, await verification.clone().text()).toBe(200);
    expect(await verification.json()).toMatchObject({
      upload_id: uploadId,
      status: "uploading",
      verification: {
        phase: "committing_archive",
        finalized_series: 1,
        verified_series: 1,
        total_series: 1,
      },
    });
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM catalog_series WHERE upload_id = ?1",
      )
        .bind(uploadId)
        .first<number>("count"),
    ).toBe(0);

    const completion = await call(
      "POST",
      `/v1/uploads/${uploadId}/complete`,
      completionBody,
      deviceToken,
    );
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
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM upload_objects WHERE upload_id = ?1 AND verified_at IS NOT NULL",
      )
        .bind(uploadId)
        .first<number>("count"),
    ).toBe(2);

    const raceNiiPart = await env.ARCHIVE.resumeMultipartUpload(
      raceNiiKey,
      raceNiiMultipart.upload_id,
    ).uploadPart(1, niiBytes);
    const raceMetadataPart = await env.ARCHIVE.resumeMultipartUpload(
      raceMetadataKey,
      raceMetadataMultipart.upload_id,
    ).uploadPart(1, metadataBytes);
    const raceCompletionBody = {
      objects: [
        {
          key: raceNiiKey,
          size: bundle.nii.size,
          sha256: bundle.nii.sha256,
          parts: [
            {
              part_number: raceNiiPart.partNumber,
              etag: raceNiiPart.etag,
            },
          ],
        },
        {
          key: raceMetadataKey,
          size: bundle.metadata.size,
          sha256: bundle.metadata.sha256,
          parts: [
            {
              part_number: raceMetadataPart.partNumber,
              etag: raceMetadataPart.etag,
            },
          ],
        },
      ],
    };
    const racedFinalization = await call(
      "POST",
      `/v1/uploads/${raceUploadId}/complete`,
      raceCompletionBody,
      raceToken,
    );
    expect(racedFinalization.status).toBe(200);
    expect(await racedFinalization.json()).toMatchObject({
      status: "uploading",
      verification: { finalized_series: 1, verified_series: 0 },
    });
    const racedVerification = await call(
      "POST",
      `/v1/uploads/${raceUploadId}/complete`,
      raceCompletionBody,
      raceToken,
    );
    expect(racedVerification.status).toBe(200);
    expect(await racedVerification.json()).toMatchObject({
      status: "uploading",
      verification: { finalized_series: 1, verified_series: 1 },
    });
    const racedCompletion = await call(
      "POST",
      `/v1/uploads/${raceUploadId}/complete`,
      raceCompletionBody,
      raceToken,
    );
    expect(racedCompletion.status).toBe(409);
    expect(await racedCompletion.json()).toMatchObject({
      error: {
        code: "DUPLICATE_BUNDLE",
        details: {
          reason: "active_exact_match",
          existing_bundles: [
            { bundle_id: bundle.bundle_id, upload_id: uploadId },
          ],
        },
      },
    });
    expect(
      await env.DB.prepare("SELECT status FROM uploads WHERE id = ?1")
        .bind(raceUploadId)
        .first<string>("status"),
    ).toBe("expired");
    expect(await env.ARCHIVE.head(raceNiiKey)).toBeNull();
    expect(await env.ARCHIVE.head(raceMetadataKey)).toBeNull();

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
    expect(
      validateArchiveManifest(manifest),
      archiveAjv.errorsText(validateArchiveManifest.errors),
    ).toBe(true);
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
      { bundles: [bundle], client_version: CLIENT_VERSION },
      deviceToken,
    );
    expect(replay.status).toBe(200);
    expect(await replay.json()).toMatchObject({
      upload_id: uploadId,
      status: "committed",
    });

    const duplicateInvite = await createInvite();
    const duplicateDevice = await enrollDevice(
      duplicateInvite.invite_code as string,
    );
    const duplicateToken = duplicateDevice.device_token as string;
    const duplicateResponse = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle], client_version: "0.2.0" },
      duplicateToken,
    );
    expect(duplicateResponse.status).toBe(409);
    expect(await duplicateResponse.json()).toMatchObject({
      error: {
        code: "DUPLICATE_BUNDLE",
        details: {
          reason: "active_exact_match",
          existing_bundles: [
            {
              bundle_id: bundle.bundle_id,
              series_id: bundle.series_id,
              subject_id: bundle.subject_id,
              session_id: bundle.session_id,
              protocol_group_id: bundle.protocol_group_id,
              upload_id: uploadId,
              nii_uncompressed_sha256: bundle.nii.uncompressed_sha256,
            },
          ],
        },
      },
    });

    const newBundle = structuredClone(bundle);
    newBundle.bundle_id = "6".repeat(24);
    newBundle.series_id = "7".repeat(24);
    newBundle.protocol_group_id = "8".repeat(24);
    newBundle.nii.relative_key = `${newBundle.bundle_id}/new_bold.nii.gz`;
    newBundle.metadata.relative_key = `${newBundle.bundle_id}/new_bold.json`;
    const mixedResponse = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle, newBundle], client_version: "0.2.0" },
      duplicateToken,
    );
    expect(mixedResponse.status).toBe(409);
    expect(await mixedResponse.json()).toMatchObject({
      error: {
        code: "DUPLICATE_BUNDLE",
        details: {
          reason: "active_exact_match",
          existing_bundles: [{ bundle_id: bundle.bundle_id }],
        },
      },
    });
    const newOnlyResponse = await call(
      "POST",
      "/v1/uploads",
      { bundles: [newBundle], client_version: "0.2.0" },
      duplicateToken,
    );
    expect(newOnlyResponse.status).toBe(201);
    const newOnly = await newOnlyResponse.json<Record<string, unknown>>();
    expect(newOnly).toMatchObject({
      status: "uploading",
    });

    const conflictingBundle = structuredClone(bundle);
    conflictingBundle.nii.uncompressed_sha256 = "f".repeat(64);
    const conflictingResponse = await call(
      "POST",
      "/v1/uploads",
      { bundles: [conflictingBundle], client_version: "0.2.0" },
      duplicateToken,
    );
    expect(conflictingResponse.status).toBe(409);
    expect(await conflictingResponse.json()).toMatchObject({
      error: {
        code: "DUPLICATE_BUNDLE",
        details: {
          reason: "identity_conflict",
          bundle_id: bundle.bundle_id,
        },
      },
    });

    await env.DB.prepare(
      "UPDATE catalog_series SET metadata_policy_version = '1.0.0' WHERE upload_id = ?1",
    )
      .bind(uploadId)
      .run();
    const stalePolicyResponse = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle], client_version: CLIENT_VERSION },
      duplicateToken,
    );
    expect(stalePolicyResponse.status).toBe(409);
    expect(await stalePolicyResponse.json()).toMatchObject({
      error: {
        code: "DUPLICATE_BUNDLE",
        details: {
          reason: "privacy_contract_stale",
          bundle_id: bundle.bundle_id,
        },
      },
    });
    await env.DB.prepare(
      "UPDATE catalog_series SET metadata_policy_version = '1.1.0' WHERE upload_id = ?1",
    )
      .bind(uploadId)
      .run();

    const oldActiveUploadId = newOnly.upload_id as string;
    const oldActivePrefix = newOnly.object_prefix as string;
    await env.DB.prepare(
      "UPDATE uploads SET client_version = '0.1.0' WHERE id = ?1",
    )
      .bind(oldActiveUploadId)
      .run();
    const replacementBundle = structuredClone(newBundle);
    replacementBundle.bundle_id = "9".repeat(24);
    replacementBundle.series_id = "a".repeat(24);
    replacementBundle.protocol_group_id = "b".repeat(24);
    replacementBundle.nii.relative_key = `${replacementBundle.bundle_id}/replacement_bold.nii.gz`;
    replacementBundle.metadata.relative_key = `${replacementBundle.bundle_id}/replacement_bold.json`;
    const replacementAllocation = await call(
      "POST",
      "/v1/uploads",
      { bundles: [replacementBundle], client_version: CLIENT_VERSION },
      duplicateToken,
    );
    expect(replacementAllocation.status).toBe(201);
    expect(await replacementAllocation.json()).toMatchObject({
      status: "uploading",
    });
    expect(
      await env.DB.prepare("SELECT status FROM uploads WHERE id = ?1")
        .bind(oldActiveUploadId)
        .first<string>("status"),
    ).toBe("expired");
    expect(
      await env.DB.prepare(
        "SELECT purged_at IS NOT NULL FROM uploads WHERE id = ?1",
      )
        .bind(oldActiveUploadId)
        .first<number>("purged_at IS NOT NULL"),
    ).toBe(1);
    expect(
      await env.DB.prepare(
        "SELECT request_hash LIKE '%:privacy:' || id AS retired FROM uploads WHERE id = ?1",
      )
        .bind(oldActiveUploadId)
        .first<number>("retired"),
    ).toBe(1);
    expect(
      (await env.ARCHIVE.list({ prefix: oldActivePrefix })).objects,
    ).toHaveLength(0);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM audit_events WHERE upload_id = ?1 AND detail_code = 'client_privacy_contract_superseded'",
      )
        .bind(oldActiveUploadId)
        .first<number>("count"),
    ).toBe(1);

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
    expect(
      await env.DB.prepare(
        "SELECT metadata_policy_version FROM catalog_series WHERE upload_id = ?1",
      )
        .bind(uploadId)
        .first<string>("metadata_policy_version"),
    ).toBe("1.1.0");

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

    const sameDeviceTombstonedReplay = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle], client_version: CLIENT_VERSION },
      deviceToken,
    );
    expect(sameDeviceTombstonedReplay.status).toBe(409);
    expect(await sameDeviceTombstonedReplay.json()).toMatchObject({
      error: {
        code: "DUPLICATE_BUNDLE",
        details: {
          reason: "withdrawn_tombstone",
          bundle_id: bundle.bundle_id,
        },
      },
    });

    const replacementInvite = await createInvite();
    const replacement = await enrollDevice(
      replacementInvite.invite_code as string,
    );
    const tombstonedReplay = await call(
      "POST",
      "/v1/uploads",
      { bundles: [bundle], client_version: CLIENT_VERSION },
      replacement.device_token as string,
    );
    expect(tombstonedReplay.status).toBe(409);
    expect(await tombstonedReplay.json()).toMatchObject({
      error: {
        code: "DUPLICATE_BUNDLE",
        details: {
          reason: "withdrawn_tombstone",
          bundle_id: bundle.bundle_id,
        },
      },
    });
  });

  it("bounds multi-series completion to one durable pair phase per request", async () => {
    // Match the 15-series production upload that exposed the original
    // monolithic-verifier failure.
    const bundleCount = 15;
    const fixture = await functionalNiftiFixture();
    const compressedSha256 = await sha256Hex(fixture.compressed);
    const subjectId = "a".repeat(24);
    const sessionId = "b".repeat(24);
    const payloads = new Map<
      string,
      { bytes: Uint8Array<ArrayBuffer>; size: number; sha256: string }
    >();
    const bundles = [];

    for (let index = 0; index < bundleCount; index += 1) {
      const bundleId = (0xc0 + index).toString(16).padStart(24, "0");
      const seriesId = (0xd0 + index).toString(16).padStart(24, "0");
      const protocolGroupId = (0xe0 + index)
        .toString(16)
        .padStart(24, "0");
      const basename = `scan-${index + 1}_bold`;
      const niiRelativeKey = `${bundleId}/${basename}.nii.gz`;
      const metadataRelativeKey = `${bundleId}/${basename}.json`;
      const sidecar = structuredClone(scanSidecarExample);
      sidecar.bundle_id = bundleId;
      sidecar.series_id = seriesId;
      sidecar.subject_id = subjectId;
      sidecar.session_id = sessionId;
      sidecar.protocol_group_id = protocolGroupId;
      sidecar.files.nifti.filename = `${basename}.nii.gz`;
      sidecar.files.nifti.size_bytes = fixture.compressed.byteLength;
      sidecar.files.nifti.sha256 = compressedSha256;
      sidecar.files.nifti.uncompressed_sha256 = fixture.uncompressedSha256;
      Object.assign(sidecar.image, fixture.image);
      const metadataBytes = new TextEncoder().encode(JSON.stringify(sidecar));
      const metadataSha256 = await sha256Hex(metadataBytes);
      const bundle = {
        bundle_id: bundleId,
        series_id: seriesId,
        subject_id: subjectId,
        session_id: sessionId,
        protocol_group_id: protocolGroupId,
        nii: {
          relative_key: niiRelativeKey,
          size: fixture.compressed.byteLength,
          sha256: compressedSha256,
          uncompressed_sha256: fixture.uncompressedSha256,
        },
        metadata: {
          relative_key: metadataRelativeKey,
          size: metadataBytes.byteLength,
          sha256: metadataSha256,
        },
      };
      bundles.push(bundle);
      payloads.set(niiRelativeKey, {
        bytes: fixture.compressed,
        size: bundle.nii.size,
        sha256: bundle.nii.sha256,
      });
      payloads.set(metadataRelativeKey, {
        bytes: metadataBytes,
        size: bundle.metadata.size,
        sha256: bundle.metadata.sha256,
      });
    }

    const invite = await createInvite();
    const enrollment = await enrollDevice(invite.invite_code as string);
    const deviceToken = enrollment.device_token as string;
    const allocationResponse = await call(
      "POST",
      "/v1/uploads",
      { bundles, client_version: CLIENT_VERSION },
      deviceToken,
    );
    expect(
      allocationResponse.status,
      await allocationResponse.clone().text(),
    ).toBe(201);
    const allocation = await allocationResponse.json<Record<string, unknown>>();
    const uploadId = allocation.upload_id as string;
    const objectPrefix = allocation.object_prefix as string;
    const multipartObjects = allocation.multipart_objects as Array<{
      key: string;
      upload_id: string;
      part_size: number;
    }>;
    expect(multipartObjects).toHaveLength(bundleCount * 2);

    const completionObjects = [];
    for (const multipartObject of multipartObjects) {
      expect(multipartObject.key.startsWith(objectPrefix)).toBe(true);
      const relativeKey = multipartObject.key.slice(objectPrefix.length);
      const payload = payloads.get(relativeKey);
      expect(payload).toBeDefined();
      const part = await env.ARCHIVE.resumeMultipartUpload(
        multipartObject.key,
        multipartObject.upload_id,
      ).uploadPart(1, payload!.bytes);
      completionObjects.push({
        key: multipartObject.key,
        size: payload!.size,
        sha256: payload!.sha256,
        parts: [{ part_number: part.partNumber, etag: part.etag }],
      });
    }
    const completionBody = { objects: completionObjects };
    const manifestKey =
      `manifests/v1/${enrollment.site_id as string}/` +
      `${enrollment.project_id as string}/${uploadId}.json`;

    const resumeSpy = vi.spyOn(env.ARCHIVE, "resumeMultipartUpload");
    const headSpy = vi.spyOn(env.ARCHIVE, "head");
    const getSpy = vi.spyOn(env.ARCHIVE, "get");
    let completionCalls = 0;

    const expectArchiveNotCommitted = async (): Promise<void> => {
      expect(
        await env.DB.prepare(
          "SELECT COUNT(*) AS count FROM catalog_series WHERE upload_id = ?1",
        )
          .bind(uploadId)
          .first<number>("count"),
      ).toBe(0);
      expect(
        await env.DB.prepare(
          "SELECT manifest_object_key FROM uploads WHERE id = ?1",
        )
          .bind(uploadId)
          .first<string | null>("manifest_object_key"),
      ).toBeNull();
    };

    for (let index = 1; index <= bundleCount; index += 1) {
      resumeSpy.mockClear();
      headSpy.mockClear();
      getSpy.mockClear();
      const response = await call(
        "POST",
        `/v1/uploads/${uploadId}/complete`,
        completionBody,
        deviceToken,
      );
      completionCalls += 1;
      expect(response.status, await response.clone().text()).toBe(200);
      expect(await response.json()).toMatchObject({
        upload_id: uploadId,
        status: "uploading",
        verification: {
          phase:
            index < bundleCount ? "finalizing_objects" : "validating_scans",
          finalized_series: index,
          verified_series: 0,
          total_series: bundleCount,
        },
      });
      expect(resumeSpy).toHaveBeenCalledTimes(2);
      expect(new Set(resumeSpy.mock.calls.map((call) => call[0])).size).toBe(2);
      expect(new Set(headSpy.mock.calls.map((call) => call[0])).size).toBe(2);
      expect(getSpy).not.toHaveBeenCalled();
      await expectArchiveNotCommitted();
    }

    for (let index = 1; index <= bundleCount; index += 1) {
      resumeSpy.mockClear();
      headSpy.mockClear();
      getSpy.mockClear();
      const response = await call(
        "POST",
        `/v1/uploads/${uploadId}/complete`,
        completionBody,
        deviceToken,
      );
      completionCalls += 1;
      expect(response.status, await response.clone().text()).toBe(200);
      expect(await response.json()).toMatchObject({
        upload_id: uploadId,
        status: "uploading",
        verification: {
          phase:
            index < bundleCount ? "validating_scans" : "committing_archive",
          finalized_series: bundleCount,
          verified_series: index,
          total_series: bundleCount,
        },
      });
      expect(resumeSpy).not.toHaveBeenCalled();
      expect(headSpy).not.toHaveBeenCalled();
      expect(getSpy).toHaveBeenCalledTimes(2);
      expect(new Set(getSpy.mock.calls.map((call) => call[0])).size).toBe(2);
      await expectArchiveNotCommitted();
    }

    resumeSpy.mockClear();
    headSpy.mockClear();
    getSpy.mockClear();
    const committedResponse = await call(
      "POST",
      `/v1/uploads/${uploadId}/complete`,
      completionBody,
      deviceToken,
    );
    completionCalls += 1;
    expect(
      committedResponse.status,
      await committedResponse.clone().text(),
    ).toBe(200);
    expect(await committedResponse.json()).toMatchObject({
      upload_id: uploadId,
      status: "committed",
      manifest: { key: manifestKey },
    });
    expect(completionCalls).toBe(bundleCount * 2 + 1);
    expect(resumeSpy).not.toHaveBeenCalled();
    expect(headSpy).not.toHaveBeenCalled();
    expect(getSpy).not.toHaveBeenCalled();
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM catalog_series WHERE upload_id = ?1",
      )
        .bind(uploadId)
        .first<number>("count"),
    ).toBe(bundleCount);
    resumeSpy.mockRestore();
    headSpy.mockRestore();
    getSpy.mockRestore();
    expect(await env.ARCHIVE.head(manifestKey)).not.toBeNull();
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
      enrollmentRequest(invite.invite_code as string, "revoked-invite-device"),
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
      { bundles: [bundle], client_version: CLIENT_VERSION },
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
