import { env } from "cloudflare:workers";
import {
  createExecutionContext,
  waitOnExecutionContext,
} from "cloudflare:test";
import Ajv2020 from "ajv/dist/2020";
import { describe, expect, it, vi } from "vitest";
import commonSchema from "../../schemas/common-v1.schema.json";
import dicomUploadSessionSchema from "../../schemas/dicom-upload-session-v1.schema.json";
import dicomUploadStatusSchema from "../../schemas/dicom-upload-status-v1.schema.json";
import { sha256Hex } from "../src/crypto";
import { fetchHandler } from "../src/index";
import { cleanupAbandoned } from "../src/service";
import {
  REQUIRED_PROCESSOR_CONTROLLER_SHA256,
  REQUIRED_PROCESSOR_PIPELINE_VERSION,
  REQUIRED_PROCESSOR_VERSION,
} from "../src/processor-contract";

const ADMIN_TOKEN = "test-admin-token-with-sufficient-entropy";
const PROCESSOR_TOKEN = "test-processor-token-with-sufficient-entropy";
const responseAjv = new Ajv2020({ strict: true, validateFormats: false });
responseAjv.addSchema(commonSchema);
responseAjv.addSchema(dicomUploadSessionSchema);
const validateDicomUploadSession = responseAjv.getSchema(
  dicomUploadSessionSchema.$id,
)!;
const validateDicomUploadStatus = responseAjv.compile(dicomUploadStatusSchema);

function processorClaim(
  processorId: string,
  extra: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    processor_id: processorId,
    lease_seconds: 900,
    processor_version: REQUIRED_PROCESSOR_VERSION,
    pipeline_version: REQUIRED_PROCESSOR_PIPELINE_VERSION,
    controller_source_sha256: REQUIRED_PROCESSOR_CONTROLLER_SHA256,
    ...extra,
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
  const request = new Request(`https://scalingneuro.com${path}`, {
    method,
    headers,
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  const ctx = createExecutionContext();
  const response = await fetchHandler(request, env, ctx);
  await waitOnExecutionContext(ctx);
  return response;
}

function deviceToken(): string {
  return `sn_device_${crypto.randomUUID().replaceAll("-", "")}${crypto
    .randomUUID()
    .replaceAll("-", "")
    .slice(0, 11)}`;
}

async function enrolledDevice(): Promise<{ token: string }> {
  const inviteResponse = await call(
    "POST",
    "/v1/admin/invites",
    {
      site_slug: `raw-${crypto.randomUUID().slice(0, 8)}`,
      site_name: "Raw receipt test site",
      project_slug: "epi",
      project_name: "EPI",
      consent_policy_version: "pilot-1",
      expires_in_seconds: 3600,
      max_uses: 1,
    },
    ADMIN_TOKEN,
  );
  expect(inviteResponse.status).toBe(201);
  const invite = await inviteResponse.json<{ invite_code: string }>();
  const token = deviceToken();
  const enrollment = await call("POST", "/v1/enroll", {
    invite_code: invite.invite_code,
    enrollment_id: crypto.randomUUID(),
    device_token: token,
    device_name: "scanner-console",
    client_version: "0.3.0",
    platform: "linux-x64",
  });
  expect(enrollment.status).toBe(201);
  return { token };
}

async function stageSmallDicomUpload(
  token: string,
  seriesArchiveIds: string[],
): Promise<{
  uploadId: string;
  objectKeys: Map<string, string>;
  objects: Array<{
    key: string;
    size: number;
    sha256: string;
    parts: Array<{ part_number: number; etag: string }>;
  }>;
}> {
  const payloads = new Map<string, Uint8Array<ArrayBuffer>>();
  const series = await Promise.all(
    seriesArchiveIds.map(async (seriesArchiveId, index) => {
      const payload = new TextEncoder().encode(
        `privacy-cleared-dicom-${seriesArchiveId}`.padEnd(96, "."),
      );
      const relativeKey = `${seriesArchiveId}/dicom.tar.zst`;
      payloads.set(relativeKey, payload);
      return {
        series_archive_id: seriesArchiveId,
        series_id: (0x700 + index).toString(16).padStart(24, "0"),
        subject_id: "8".repeat(24),
        session_id: "9".repeat(24),
        protocol_group_id: (0xa00 + index).toString(16).padStart(24, "0"),
        dicom_count: 10,
        archive: {
          format: "dicom-tar-zstd",
          relative_key: relativeKey,
          size: payload.byteLength,
          sha256: await sha256Hex(payload),
        },
      };
    }),
  );
  const allocationResponse = await call(
    "POST",
    "/v1/dicom-uploads",
    {
      format: "dicom-series-v1",
      client_version: "0.3.0",
      deidentification: {
        policy_id: "scaling-neuro.dicom-deidentification",
        policy_version: "1.0.0",
      },
      series,
    },
    token,
  );
  expect(allocationResponse.status).toBe(201);
  const allocation = await allocationResponse.json<{
    upload_id: string;
    object_prefix: string;
    multipart_objects: Array<{
      key: string;
      upload_id: string;
    }>;
  }>();
  const objects = [];
  const objectKeys = new Map<string, string>();
  for (const multipart of allocation.multipart_objects) {
    const relativeKey = multipart.key.slice(allocation.object_prefix.length);
    const descriptor = series.find(
      (candidate) => candidate.archive.relative_key === relativeKey,
    )!;
    const payload = payloads.get(relativeKey)!;
    const part = await env.ARCHIVE.resumeMultipartUpload(
      multipart.key,
      multipart.upload_id,
    ).uploadPart(1, payload);
    objects.push({
      key: multipart.key,
      size: descriptor.archive.size,
      sha256: descriptor.archive.sha256,
      parts: [{ part_number: 1, etag: part.etag }],
    });
    objectKeys.set(descriptor.series_archive_id, multipart.key);
  }
  return { uploadId: allocation.upload_id, objectKeys, objects };
}

async function receiveSmallDicomUpload(
  token: string,
  seriesArchiveIds: string[],
): Promise<{
  uploadId: string;
  objectKeys: Map<string, string>;
}> {
  const staged = await stageSmallDicomUpload(token, seriesArchiveIds);
  const completed = await call(
    "POST",
    `/v1/dicom-uploads/${staged.uploadId}/complete`,
    { objects: staged.objects },
    token,
  );
  expect(completed.status, await completed.clone().text()).toBe(200);
  return { uploadId: staged.uploadId, objectKeys: staged.objectKeys };
}

describe("DICOM receipt and processing queue", () => {
  it("reports only a fresh, source-attested all-MR processor as ready", async () => {
    const initial = await call("GET", "/health");
    expect(await initial.json()).toMatchObject({
      processor: {
        ready: false,
        required_version: "0.2.0",
        required_pipeline_version: "dicom-mr-v2",
        active_compatible_consumers: 0,
        active_controller_source_sha256: [],
      },
    });
    const digest = REQUIRED_PROCESSOR_CONTROLLER_SHA256;
    const claim = await call(
      "POST",
      "/v1/processor/jobs/claim",
      {
        processor_id: "release-attested-consumer",
        lease_seconds: 900,
        claim_input_format: "dicom-series-v1",
        processor_version: "0.2.0",
        pipeline_version: "dicom-mr-v2",
        controller_source_sha256: digest,
      },
      PROCESSOR_TOKEN,
    );
    expect(claim.status).toBe(204);
    const ready = await call("GET", "/health");
    expect(await ready.json()).toMatchObject({
      processor: {
        ready: true,
        required_version: "0.2.0",
        required_pipeline_version: "dicom-mr-v2",
        required_controller_source_sha256: digest,
        active_compatible_consumers: 1,
        active_controller_source_sha256: [digest],
      },
    });

    await env.DB.prepare(
      "UPDATE processor_instances SET last_seen_at = ?1 WHERE processor_id = ?2",
    )
      .bind(Math.floor(Date.now() / 1000) - 181, "release-attested-consumer")
      .run();
    const stale = await call("GET", "/health");
    expect(await stale.json()).toMatchObject({
      processor: { ready: false, active_compatible_consumers: 0 },
    });
  });

  it("keeps readiness fresh only while the exact processing lease is active", async () => {
    const { token } = await enrolledDevice();
    await receiveSmallDicomUpload(token, ["1".repeat(24)]);
    const processorId = "long-running-ready-consumer";
    const claimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim(processorId, { claim_input_format: "dicom-series-v1" }),
      PROCESSOR_TOKEN,
    );
    expect(claimResponse.status).toBe(200);
    const claim = await claimResponse.json<{
      job_id: string;
      lease_token: string;
    }>();
    const staleAt = Math.floor(Date.now() / 1000) - 181;
    await env.DB.prepare(
      "UPDATE processor_instances SET last_seen_at = ?1 WHERE processor_id = ?2",
    )
      .bind(staleAt, processorId)
      .run();
    expect(await (await call("GET", "/health")).json()).toMatchObject({
      processor: { ready: false, active_compatible_consumers: 0 },
    });

    const heartbeat = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/heartbeat`,
      { lease_token: claim.lease_token, lease_seconds: 900 },
      PROCESSOR_TOKEN,
    );
    expect(heartbeat.status).toBe(200);
    expect(await (await call("GET", "/health")).json()).toMatchObject({
      processor: { ready: true, active_compatible_consumers: 1 },
    });

    // Expire the job after requireActiveJobLease's read but before the heartbeat
    // batch. Neither the lease nor processor readiness may be refreshed.
    await env.DB.prepare(
      "UPDATE processor_instances SET last_seen_at = ?1 WHERE processor_id = ?2",
    )
      .bind(staleAt, processorId)
      .run();
    const originalBatch = env.DB.batch.bind(env.DB);
    const batchSpy = vi
      .spyOn(env.DB, "batch")
      .mockImplementationOnce(async (statements) => {
        await env.DB.prepare(
          "UPDATE processing_jobs SET lease_expires_at = ?1 WHERE id = ?2",
        )
          .bind(Math.floor(Date.now() / 1000) - 1, claim.job_id)
          .run();
        return originalBatch(statements);
      });
    const staleHeartbeat = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/heartbeat`,
      { lease_token: claim.lease_token, lease_seconds: 900 },
      PROCESSOR_TOKEN,
    );
    batchSpy.mockRestore();
    expect(staleHeartbeat.status).toBe(409);
    expect(await staleHeartbeat.json()).toMatchObject({
      error: { code: "LEASE_LOST" },
    });
    expect(
      await env.DB.prepare(
        "SELECT last_seen_at FROM processor_instances WHERE processor_id = ?1",
      )
        .bind(processorId)
        .first<number>("last_seen_at"),
    ).toBe(staleAt);
    expect(await (await call("GET", "/health")).json()).toMatchObject({
      processor: { ready: false, active_compatible_consumers: 0 },
    });
    // This suite intentionally shares one Miniflare database. Leave the
    // synthetic expired lease terminal so later claim tests see only their own
    // queued jobs.
    await env.DB.prepare(
      `UPDATE processing_jobs
       SET status = 'failed', failed_at = ?1, updated_at = ?1,
           error_code = 'TEST_LEASE_EXPIRED', error_message = 'TEST_LEASE_EXPIRED',
           processor_id = NULL, lease_token = NULL, lease_expires_at = NULL
       WHERE id = ?2`,
    )
      .bind(Math.floor(Date.now() / 1000), claim.job_id)
      .run();
  });

  it("allows one workstation to checkpoint independent folders concurrently", async () => {
    const { token } = await enrolledDevice();
    const makeBody = async (digit: string) => {
      const seriesArchiveId = digit.repeat(24);
      const payload = new TextEncoder().encode(
        `independent-folder-${digit}`.padEnd(96, "."),
      );
      return {
        format: "dicom-series-v1",
        client_version: "0.3.0",
        deidentification: {
          policy_id: "scaling-neuro.dicom-deidentification",
          policy_version: "1.0.0",
        },
        series: [
          {
            series_archive_id: seriesArchiveId,
            series_id: String(Number(digit) + 2).repeat(24),
            subject_id: "a".repeat(24),
            session_id: String(Number(digit) + 4).repeat(24),
            protocol_group_id: String(Number(digit) + 6).repeat(24),
            dicom_count: 12,
            archive: {
              format: "dicom-tar-zstd",
              relative_key: `${seriesArchiveId}/dicom.tar.zst`,
              size: payload.byteLength,
              sha256: await sha256Hex(payload),
            },
          },
        ],
      };
    };
    const firstBody = await makeBody("1");
    const secondBody = await makeBody("2");
    const first = await call("POST", "/v1/dicom-uploads", firstBody, token);
    const second = await call("POST", "/v1/dicom-uploads", secondBody, token);
    expect(first.status).toBe(201);
    expect(second.status).toBe(201);
    const firstSession = await first.json<{ upload_id: string }>();
    const secondSession = await second.json<{ upload_id: string }>();
    expect(secondSession.upload_id).not.toBe(firstSession.upload_id);
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM uploads
         WHERE status IN ('created', 'uploading')
           AND ingest_format = 'dicom-series-v1'`,
      ).first<number>("count"),
    ).toBe(2);

    const replay = await call(
      "POST",
      "/v1/dicom-uploads",
      firstBody,
      token,
    );
    expect(replay.status).toBe(200);
    expect(await replay.json()).toMatchObject({
      upload_id: firstSession.upload_id,
      status: "uploading",
    });
  });

  it("accepts generic non-ASL perfusion as archive-only MR", async () => {
    const { token } = await enrolledDevice();
    const payload = new TextEncoder().encode(
      "generic-dsc-perfusion-archive".padEnd(64, "."),
    );
    const seriesArchiveId = "e".repeat(24);
    const allocationResponse = await call(
      "POST",
      "/v1/dicom-uploads",
      {
        format: "dicom-series-v1",
        client_version: "0.4.0",
        deidentification: {
          policy_id: "scaling-neuro.dicom-deidentification",
          policy_version: "2.0.0",
        },
        series: [
          {
            series_archive_id: seriesArchiveId,
            series_id: "d".repeat(24),
            subject_id: "c".repeat(24),
            session_id: "b".repeat(24),
            protocol_group_id: "a".repeat(24),
            dicom_count: 12,
            series_kind: "perfusion",
            processing_route: "archive-verify-v1",
            pixel_data_policy: "scanner-native-not-defaced",
            archive: {
              format: "dicom-tar-zstd",
              relative_key: `${seriesArchiveId}/dicom.tar.zst`,
              size: payload.byteLength,
              sha256: await sha256Hex(payload),
            },
          },
        ],
      },
      token,
    );
    expect(
      allocationResponse.status,
      await allocationResponse.clone().text(),
    ).toBe(201);
    const allocation = await allocationResponse.json<{ upload_id: string }>();
    expect(
      await env.DB.prepare(
        `SELECT series_kind, processing_route
         FROM dicom_upload_series WHERE upload_id = ?1`,
      )
        .bind(allocation.upload_id)
        .first(),
    ).toEqual({
      series_kind: "perfusion",
      processing_route: "archive-verify-v1",
    });
  });

  it("routes all-MR receipts through functional conversion or archive verification", async () => {
    const { token } = await enrolledDevice();
    const payloads = new Map<string, Uint8Array<ArrayBuffer>>();
    const descriptor = async (
      digit: string,
      seriesKind: string,
      processingRoute: "functional-epi-v1" | "archive-verify-v1",
    ) => {
      const seriesArchiveId = digit.repeat(24);
      const relativeKey = `${seriesArchiveId}/dicom.tar.zst`;
      const payload = new TextEncoder().encode(
        `all-mr-${seriesKind}-${digit}`.padEnd(128, "."),
      );
      payloads.set(relativeKey, payload);
      return {
        series_archive_id: seriesArchiveId,
        series_id: String(Number(digit) + 2).repeat(24),
        subject_id: "a".repeat(24),
        session_id: "b".repeat(24),
        protocol_group_id: String(Number(digit) + 4).repeat(24),
        dicom_count: 20,
        series_kind: seriesKind,
        processing_route: processingRoute,
        pixel_data_policy: "scanner-native-not-defaced",
        archive: {
          format: "dicom-tar-zstd",
          relative_key: relativeKey,
          size: payload.byteLength,
          sha256: await sha256Hex(payload),
        },
      };
    };
    const series = [
      await descriptor("1", "functional_epi", "functional-epi-v1"),
      await descriptor("2", "structural_t1w", "archive-verify-v1"),
    ];
    const requestBody = {
      format: "dicom-series-v1",
      client_version: "0.4.0",
      deidentification: {
        policy_id: "scaling-neuro.dicom-deidentification",
        policy_version: "2.0.0",
      },
      series,
    };

    const stale = await call(
      "POST",
      "/v1/dicom-uploads",
      { ...requestBody, client_version: "0.3.1" },
      token,
    );
    expect(stale.status).toBe(426);
    expect(await stale.json()).toMatchObject({
      error: {
        code: "CLIENT_UPDATE_REQUIRED",
        details: { minimum_client_version: "0.4.0" },
      },
    });
    const missingRouting = await call(
      "POST",
      "/v1/dicom-uploads",
      {
        ...requestBody,
        series: series.map(
          ({ series_kind: _kind, processing_route: _route,
             pixel_data_policy: _pixels, ...item }) => item,
        ),
      },
      token,
    );
    expect(missingRouting.status).toBe(400);
    const legacyWithRouting = await call(
      "POST",
      "/v1/dicom-uploads",
      {
        ...requestBody,
        client_version: "0.3.1",
        deidentification: {
          ...requestBody.deidentification,
          policy_version: "1.0.0",
        },
      },
      token,
    );
    expect(legacyWithRouting.status).toBe(400);

    const allocatedResponse = await call(
      "POST",
      "/v1/dicom-uploads",
      requestBody,
      token,
    );
    expect(
      allocatedResponse.status,
      await allocatedResponse.clone().text(),
    ).toBe(201);
    const allocated = await allocatedResponse.json<{
      upload_id: string;
      object_prefix: string;
      multipart_objects: Array<{ key: string; upload_id: string }>;
    }>();
    const storedRoutes = await env.DB.prepare(
      `SELECT series_kind, processing_route, pixel_data_policy
       FROM dicom_upload_series WHERE upload_id = ?1 ORDER BY series_kind`,
    )
      .bind(allocated.upload_id)
      .all();
    expect(storedRoutes.results).toEqual([
      {
        series_kind: "functional_epi",
        processing_route: "functional-epi-v1",
        pixel_data_policy: "scanner-native-not-defaced",
      },
      {
        series_kind: "structural_t1w",
        processing_route: "archive-verify-v1",
        pixel_data_policy: "scanner-native-not-defaced",
      },
    ]);

    const objects = [];
    for (const multipart of allocated.multipart_objects) {
      const relativeKey = multipart.key.slice(allocated.object_prefix.length);
      const payload = payloads.get(relativeKey)!;
      const seriesDescriptor = series.find(
        (item) => item.archive.relative_key === relativeKey,
      )!;
      const part = await env.ARCHIVE.resumeMultipartUpload(
        multipart.key,
        multipart.upload_id,
      ).uploadPart(1, payload);
      objects.push({
        key: multipart.key,
        size: seriesDescriptor.archive.size,
        sha256: seriesDescriptor.archive.sha256,
        parts: [{ part_number: 1, etag: part.etag }],
      });
    }
    const checkpointed = await call(
      "POST",
      `/v1/dicom-uploads/${allocated.upload_id}/checkpoint`,
      { objects },
      token,
    );
    expect(
      checkpointed.status,
      await checkpointed.clone().text(),
    ).toBe(200);
    const checkpointedBody = await checkpointed.json();
    expect(
      validateDicomUploadStatus(checkpointedBody),
      responseAjv.errorsText(validateDicomUploadStatus.errors),
    ).toBe(true);
    expect(checkpointedBody).toMatchObject({
      status: "checkpointed",
      receipt: { received_series: 2, total_series: 2 },
    });
    const checkpointedCredentials = await call(
      "POST",
      `/v1/dicom-uploads/${allocated.upload_id}/credentials`,
      undefined,
      token,
    );
    expect(
      checkpointedCredentials.status,
      await checkpointedCredentials.clone().text(),
    ).toBe(200);
    const checkpointedCredentialsBody = await checkpointedCredentials.json();
    expect(
      validateDicomUploadSession(checkpointedCredentialsBody),
      responseAjv.errorsText(validateDicomUploadSession.errors),
    ).toBe(true);
    expect(checkpointedCredentialsBody).toMatchObject({
      upload_id: allocated.upload_id,
      status: "checkpointed",
      object_prefix: allocated.object_prefix,
      multipart_objects: [],
    });
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM received_series_reservations WHERE upload_id = ?1",
      )
        .bind(allocated.upload_id)
        .first<number>("count"),
    ).toBe(0);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM processing_jobs WHERE upload_id = ?1",
      )
        .bind(allocated.upload_id)
        .first<number>("count"),
    ).toBe(0);
    for (const multipart of allocated.multipart_objects) {
      expect(await env.ARCHIVE.head(multipart.key)).not.toBeNull();
    }

    // A lost checkpoint response is replay-safe. The normal seven-day upload
    // session may then expire while the durable provisional object remains
    // eligible for the final whole-folder receipt gate.
    const checkpointReplay = await call(
      "POST",
      `/v1/dicom-uploads/${allocated.upload_id}/checkpoint`,
      { objects },
      token,
    );
    expect(checkpointReplay.status).toBe(200);
    expect(await checkpointReplay.json()).toMatchObject({
      status: "checkpointed",
    });
    await env.DB.prepare("UPDATE uploads SET expires_at = ?1 WHERE id = ?2")
      .bind(Math.floor(Date.now() / 1000) - 1, allocated.upload_id)
      .run();
    const stagedStatus = await call(
      "GET",
      `/v1/dicom-uploads/${allocated.upload_id}`,
      undefined,
      token,
    );
    const stagedStatusBody = await stagedStatus.json();
    expect(
      validateDicomUploadStatus(stagedStatusBody),
      responseAjv.errorsText(validateDicomUploadStatus.errors),
    ).toBe(true);
    expect(stagedStatusBody).toMatchObject({ status: "checkpointed" });

    const received = await call(
      "POST",
      `/v1/dicom-uploads/${allocated.upload_id}/complete`,
      { objects },
      token,
    );
    expect(received.status, await received.clone().text()).toBe(200);
    expect(await received.json()).toMatchObject({
      status: "committed",
      receipt: { received_series: 2, total_series: 2 },
      processing: {
        status: "queued",
        queued_series: 2,
        total_series: 2,
        functional_epi_series: 1,
        archive_only_series: 1,
        archive_verified_series: 0,
      },
    });
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM received_series_reservations
         WHERE upload_id = ?1 AND processing_route = 'archive-verify-v1'`,
      )
        .bind(allocated.upload_id)
        .first<number>("count"),
    ).toBe(1);

    const oldProcessor = await call(
      "POST",
      "/v1/processor/jobs/claim",
      {
        processor_id: "pre-all-mr-processor",
        lease_seconds: 900,
        claim_input_format: "dicom-series-v1",
      },
      PROCESSOR_TOKEN,
    );
    expect(oldProcessor.status).toBe(204);
    const wrongSourceProcessor = await call(
      "POST",
      "/v1/processor/jobs/claim",
      {
        ...processorClaim("wrong-source-processor", {
          claim_input_format: "dicom-series-v1",
        }),
        controller_source_sha256: "0".repeat(64),
      },
      PROCESSOR_TOKEN,
    );
    expect(wrongSourceProcessor.status).toBe(204);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM processing_jobs WHERE upload_id = ?1 AND status = 'queued'",
      )
        .bind(allocated.upload_id)
        .first<number>("count"),
    ).toBe(2);

    const claims = [];
    for (const processorId of ["all-mr-a", "all-mr-b"]) {
      const response = await call(
        "POST",
        "/v1/processor/jobs/claim",
        processorClaim(processorId),
        PROCESSOR_TOKEN,
      );
      expect(response.status).toBe(200);
      claims.push(
        await response.json<{
          job_id: string;
          lease_token: string;
          client_version: string;
          series_archive_id: string;
          series_kind: string;
          processing_route: "functional-epi-v1" | "archive-verify-v1";
          pixel_data_policy: string;
          input: { dicom_count: number };
        }>(),
      );
    }
    const archiveClaim = claims.find(
      (claim) => claim.processing_route === "archive-verify-v1",
    )!;
    expect(claims.every((claim) => claim.client_version === "0.4.0")).toBe(true);
    expect(archiveClaim).toMatchObject({
      series_kind: "structural_t1w",
      pixel_data_policy: "scanner-native-not-defaced",
    });
    const outputRejected = await call(
      "POST",
      `/v1/processor/jobs/${archiveClaim.job_id}/outputs`,
      {
        lease_token: archiveClaim.lease_token,
        outputs: [
          {
            kind: "nifti",
            size_bytes: 32,
            sha256: "a".repeat(64),
            content_type: "application/gzip",
            uncompressed_sha256: "b".repeat(64),
          },
          {
            kind: "sidecar",
            size_bytes: 2,
            sha256: "c".repeat(64),
            content_type: "application/json",
          },
          {
            kind: "processing_manifest",
            size_bytes: 2,
            sha256: "d".repeat(64),
            content_type: "application/json",
          },
        ],
      },
      PROCESSOR_TOKEN,
    );
    expect(outputRejected.status).toBe(400);

    const archiveCompletion = {
      lease_token: archiveClaim.lease_token,
      processor_version: "1.1.0",
      outputs: [],
      validation: {
        archive_sha256_verified: true,
        dicom_count: archiveClaim.input.dicom_count,
        dicom_parse_succeeded: true,
        dicom_privacy_audit_succeeded: true,
        functional_epi_confirmed: false,
      },
    };
    const falselyFunctional = await call(
      "POST",
      `/v1/processor/jobs/${archiveClaim.job_id}/complete`,
      {
        ...archiveCompletion,
        validation: {
          ...archiveCompletion.validation,
          functional_epi_confirmed: true,
        },
      },
      PROCESSOR_TOKEN,
    );
    expect(falselyFunctional.status).toBe(409);
    const archiveProcessed = await call(
      "POST",
      `/v1/processor/jobs/${archiveClaim.job_id}/complete`,
      archiveCompletion,
      PROCESSOR_TOKEN,
    );
    expect(archiveProcessed.status).toBe(200);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM catalog_series WHERE upload_id = ?1",
      )
        .bind(allocated.upload_id)
        .first<number>("count"),
    ).toBe(0);
    const status = await call(
      "GET",
      `/v1/dicom-uploads/${allocated.upload_id}`,
      undefined,
      token,
    );
    expect(await status.json()).toMatchObject({
      processing: {
        queued_series: 0,
        processing_series: 1,
        processed_series: 1,
        functional_epi_series: 1,
        archive_only_series: 1,
        archive_verified_series: 1,
      },
    });
    const functionalClaim = claims.find(
      (claim) => claim.processing_route === "functional-epi-v1",
    )!;
    const purposeDowngrade = {
      lease_token: functionalClaim.lease_token,
      processor_version: "1.1.0",
      outputs: [],
      validation: {
        archive_sha256_verified: true,
        dicom_count: functionalClaim.input.dicom_count,
        dicom_parse_succeeded: true,
        dicom_privacy_audit_succeeded: true,
        functional_epi_confirmed: false,
      },
    };
    const downgradedFunctional = await call(
      "POST",
      `/v1/processor/jobs/${functionalClaim.job_id}/complete`,
      purposeDowngrade,
      PROCESSOR_TOKEN,
    );
    expect(
      downgradedFunctional.status,
      await downgradedFunctional.clone().text(),
    ).toBe(200);
    const downgradedReplay = await call(
      "POST",
      `/v1/processor/jobs/${functionalClaim.job_id}/complete`,
      purposeDowngrade,
      PROCESSOR_TOKEN,
    );
    expect(downgradedReplay.status).toBe(200);
    expect(
      await env.DB.prepare(
        `SELECT series_kind, processing_route, effective_series_kind,
                effective_processing_route
         FROM dicom_upload_series
         WHERE upload_id = ?1 AND series_archive_id = ?2`,
      )
        .bind(allocated.upload_id, functionalClaim.series_archive_id)
        .first(),
    ).toEqual({
      series_kind: "functional_epi",
      processing_route: "functional-epi-v1",
      effective_series_kind: "other_mr",
      effective_processing_route: "archive-verify-v1",
    });
    const finalStatus = await call(
      "GET",
      `/v1/dicom-uploads/${allocated.upload_id}`,
      undefined,
      token,
    );
    expect(await finalStatus.json()).toMatchObject({
      processing: {
        status: "processed",
        queued_series: 0,
        processing_series: 0,
        processed_series: 2,
        functional_epi_series: 0,
        archive_only_series: 2,
        archive_verified_series: 2,
      },
    });
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM catalog_series WHERE upload_id = ?1",
      )
        .bind(allocated.upload_id)
        .first<number>("count"),
    ).toBe(0);
  });

  it("immediately replaces an expired folder session on the same command", async () => {
    const { token } = await enrolledDevice();
    const seriesArchiveId = "e".repeat(24);
    const payload = new TextEncoder().encode("expired-folder".padEnd(96, "."));
    const body = {
      format: "dicom-series-v1",
      client_version: "0.3.0",
      deidentification: {
        policy_id: "scaling-neuro.dicom-deidentification",
        policy_version: "1.0.0",
      },
      series: [
        {
          series_archive_id: seriesArchiveId,
          series_id: "d".repeat(24),
          subject_id: "c".repeat(24),
          session_id: "b".repeat(24),
          protocol_group_id: "a".repeat(24),
          dicom_count: 16,
          archive: {
            format: "dicom-tar-zstd",
            relative_key: `${seriesArchiveId}/dicom.tar.zst`,
            size: payload.byteLength,
            sha256: await sha256Hex(payload),
          },
        },
      ],
    };
    const allocated = await call("POST", "/v1/dicom-uploads", body, token);
    expect(allocated.status).toBe(201);
    const first = await allocated.json<{ upload_id: string }>();
    await env.DB.prepare("UPDATE uploads SET expires_at = ?1 WHERE id = ?2")
      .bind(Math.floor(Date.now() / 1000) - 1, first.upload_id)
      .run();

    const status = await call(
      "GET",
      `/v1/dicom-uploads/${first.upload_id}`,
      undefined,
      token,
    );
    expect(status.status).toBe(200);
    expect(await status.json()).toMatchObject({ status: "expired" });

    const replacement = await call(
      "POST",
      "/v1/dicom-uploads",
      body,
      token,
    );
    expect(replacement.status).toBe(201);
    const next = await replacement.json<{ upload_id: string }>();
    expect(next.upload_id).not.toBe(first.upload_id);
  });

  it("retains provisional DICOM objects past multipart expiry and purges them at their own deadline", async () => {
    const { token } = await enrolledDevice();
    const staged = await stageSmallDicomUpload(token, ["3".repeat(24)]);
    const checkpoint = await call(
      "POST",
      `/v1/dicom-uploads/${staged.uploadId}/checkpoint`,
      { objects: staged.objects },
      token,
    );
    expect(checkpoint.status, await checkpoint.clone().text()).toBe(200);
    expect(await checkpoint.json()).toMatchObject({ status: "checkpointed" });
    const key = [...staged.objectKeys.values()][0]!;
    expect(await env.ARCHIVE.head(key)).not.toBeNull();

    const now = Math.floor(Date.now() / 1000);
    await env.DB.prepare(
      `UPDATE uploads SET expires_at = ?1, provisional_expires_at = ?2
       WHERE id = ?3`,
    )
      .bind(now - 8 * 24 * 60 * 60, now + 60, staged.uploadId)
      .run();
    await cleanupAbandoned(env);
    expect(
      await env.DB.prepare("SELECT status FROM uploads WHERE id = ?1")
        .bind(staged.uploadId)
        .first<string>("status"),
    ).toBe("uploading");
    expect(await env.ARCHIVE.head(key)).not.toBeNull();

    await env.DB.prepare(
      "UPDATE uploads SET provisional_expires_at = ?1 WHERE id = ?2",
    )
      .bind(now - 1, staged.uploadId)
      .run();
    await cleanupAbandoned(env);
    expect(
      await env.DB.prepare(
        "SELECT status, purged_at FROM uploads WHERE id = ?1",
      )
        .bind(staged.uploadId)
        .first(),
    ).toMatchObject({ status: "expired" });
    expect(await env.ARCHIVE.head(key)).toBeNull();
  });

  it("replays a lost claim response without consuming another job or attempt", async () => {
    const { token } = await enrolledDevice();
    await receiveSmallDicomUpload(token, ["5".repeat(24), "6".repeat(24)]);

    const firstResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("claim-replay-consumer", {
        claim_input_format: "dicom-series-v1",
      }),
      PROCESSOR_TOKEN,
    );
    expect(firstResponse.status).toBe(200);
    const first = await firstResponse.json<{
      schema_version: string;
      job_id: string;
      lease_token: string;
      attempt: number;
    }>();
    expect(first.schema_version).toBe("1.0.0");

    await env.DB.prepare(
      `UPDATE processing_jobs SET input_format = 'nifti-v1'
       WHERE status = 'queued'`,
    ).run();

    const mismatchedReplay = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("claim-replay-consumer", {
        claim_input_format: "nifti-v1",
      }),
      PROCESSOR_TOKEN,
    );
    expect(mismatchedReplay.status).toBe(204);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM processing_jobs WHERE status = 'processing'",
      ).first<number>("count"),
    ).toBe(1);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM processing_jobs WHERE status = 'queued' AND input_format = 'nifti-v1'",
      ).first<number>("count"),
    ).toBe(1);

    // Model a response that was committed by D1 but never reached the
    // processor. Its automatic POST retry must return the same active lease.
    const replayResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("claim-replay-consumer", {
        claim_input_format: "dicom-series-v1",
      }),
      PROCESSOR_TOKEN,
    );
    expect(replayResponse.status).toBe(200);
    const replay = await replayResponse.json<{
      job_id: string;
      lease_token: string;
      attempt: number;
    }>();
    expect(replay).toMatchObject({
      job_id: first.job_id,
      lease_token: first.lease_token,
      attempt: first.attempt,
    });
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM processing_jobs WHERE status = 'processing'",
      ).first<number>("count"),
    ).toBe(1);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM processing_jobs WHERE status = 'queued'",
      ).first<number>("count"),
    ).toBe(1);

    await env.DB.prepare(
      `UPDATE processing_jobs SET input_format = 'dicom-series-v1'
       WHERE status = 'queued'`,
    ).run();

    const otherResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("independent-consumer"),
      PROCESSOR_TOKEN,
    );
    expect(otherResponse.status).toBe(200);
    const other = await otherResponse.json<{ job_id: string; attempt: number }>();
    expect(other.job_id).not.toBe(first.job_id);
    expect(other.attempt).toBe(1);
  });

  it("atomically limits a processor identity to one active lease", async () => {
    const { token } = await enrolledDevice();
    const received = await receiveSmallDicomUpload(token, [
      "a".repeat(24),
      "b".repeat(24),
    ]);
    const responses = await Promise.all(
      Array.from({ length: 2 }, () =>
        call(
          "POST",
          "/v1/processor/jobs/claim",
          processorClaim("concurrent-same-consumer", {
            claim_input_format: "dicom-series-v1",
          }),
          PROCESSOR_TOKEN,
        ),
      ),
    );
    const claims: Array<{ job_id: string; lease_token: string }> = [];
    for (const response of responses) {
      expect([200, 204]).toContain(response.status);
      if (response.status === 200) {
        claims.push(
          await response.json<{ job_id: string; lease_token: string }>(),
        );
      }
    }
    expect(claims.length).toBeGreaterThanOrEqual(1);
    expect(
      new Set(claims.map((claim) => `${claim.job_id}:${claim.lease_token}`)).size,
    ).toBe(1);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM processing_jobs WHERE upload_id = ?1 AND status = 'processing'",
      )
        .bind(received.uploadId)
        .first<number>("count"),
    ).toBe(1);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM processing_jobs WHERE upload_id = ?1 AND status = 'queued'",
      )
        .bind(received.uploadId)
        .first<number>("count"),
    ).toBe(1);
    await env.DB.prepare("DELETE FROM processing_jobs WHERE upload_id = ?1")
      .bind(received.uploadId)
      .run();
  });

  it("claims new raw DICOM work before an older legacy migration backlog", async () => {
    const { token } = await enrolledDevice();
    const received = await receiveSmallDicomUpload(token, ["7".repeat(24)]);
    const oldTimestamp = Math.floor(Date.now() / 1000) - 86_400;
    const legacyJobId = crypto.randomUUID();
    await env.DB.prepare(
      `INSERT INTO processing_jobs
         (id, upload_id, bundle_id, input_format, status, attempt,
          next_attempt_at, created_at, updated_at)
       VALUES (?1, ?2, ?3, 'nifti-v1', 'queued', 0, ?4, ?4, ?4)`,
    )
      .bind(legacyJobId, received.uploadId, "f".repeat(24), oldTimestamp)
      .run();

    const claimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      {
        processor_id: "legacy-raw-priority-consumer",
        lease_seconds: 900,
        claim_input_format: "dicom-series-v1",
      },
      PROCESSOR_TOKEN,
    );
    expect(claimResponse.status, await claimResponse.clone().text()).toBe(200);
    const claim = await claimResponse.json<{
      input_format: string;
      bundle_id: string;
    }>();
    expect(claim).toMatchObject({
      input_format: "dicom-series-v1",
      bundle_id: "7".repeat(24),
    });
    expect(
      await env.DB.prepare(
        "SELECT status FROM processing_jobs WHERE id = ?1",
      )
        .bind(legacyJobId)
        .first<string>("status"),
    ).toBe("queued");
    await env.DB.prepare("DELETE FROM processing_jobs WHERE id = ?1")
      .bind(legacyJobId)
      .run();
  });

  it("returns no work to a raw-only consumer when only legacy work is queued", async () => {
    const { token } = await enrolledDevice();
    const received = await receiveSmallDicomUpload(token, ["d".repeat(24)]);
    await env.DB.prepare(
      "UPDATE processing_jobs SET input_format = 'nifti-v1' WHERE upload_id = ?1",
    )
      .bind(received.uploadId)
      .run();

    const claimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("raw-only-consumer", {
        claim_input_format: "dicom-series-v1",
      }),
      PROCESSOR_TOKEN,
    );
    expect(claimResponse.status).toBe(204);
    expect(
      await env.DB.prepare(
        "SELECT status FROM processing_jobs WHERE upload_id = ?1",
      )
        .bind(received.uploadId)
        .first<string>("status"),
    ).toBe("queued");
    await env.DB.prepare("DELETE FROM processing_jobs WHERE upload_id = ?1")
      .bind(received.uploadId)
      .run();
  });

  it("retains a source when processor transfer integrity disagrees", async () => {
    const { token } = await enrolledDevice();
    const received = await receiveSmallDicomUpload(token, ["0".repeat(24)]);
    const claimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("archive-integrity-auditor"),
      PROCESSOR_TOKEN,
    );
    expect(claimResponse.status).toBe(200);
    const claim = await claimResponse.json<{
      job_id: string;
      bundle_id: string;
      lease_token: string;
    }>();
    const sourceKey = received.objectKeys.get(claim.bundle_id)!;
    expect(await env.ARCHIVE.head(sourceKey)).not.toBeNull();

    const failed = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/fail`,
      {
        lease_token: claim.lease_token,
        retryable: false,
        error_code: "ARCHIVE_DOWNLOAD_INTEGRITY_MISMATCH",
        error_message: "ARCHIVE_DOWNLOAD_INTEGRITY_MISMATCH",
      },
      PROCESSOR_TOKEN,
    );
    expect(failed.status, await failed.clone().text()).toBe(200);
    expect(await failed.json()).toEqual({
      job_id: claim.job_id,
      status: "failed",
    });
    expect(await env.ARCHIVE.head(sourceKey)).not.toBeNull();
    expect(
      await env.DB.prepare(
        `SELECT withdrawn_at FROM received_series_reservations
         WHERE upload_id = ?1 AND bundle_id = ?2`,
      )
        .bind(received.uploadId, claim.bundle_id)
        .first<number | null>("withdrawn_at"),
    ).toBeNull();
  });

  it("purges only after repeated full-object integrity mismatches", async () => {
    const { token } = await enrolledDevice();
    const received = await receiveSmallDicomUpload(token, ["e".repeat(24)]);
    const sourceKey = received.objectKeys.get("e".repeat(24))!;
    expect(await env.ARCHIVE.head(sourceKey)).not.toBeNull();

    let jobId = "";
    for (let attempt = 1; attempt <= 5; attempt += 1) {
      const claimResponse = await call(
        "POST",
        "/v1/processor/jobs/claim",
        processorClaim(`integrity-redownload-${attempt}`),
        PROCESSOR_TOKEN,
      );
      expect(claimResponse.status).toBe(200);
      const claim = await claimResponse.json<{
        job_id: string;
        attempt: number;
        lease_token: string;
      }>();
      jobId = claim.job_id;
      expect(claim.attempt).toBe(attempt);

      if (attempt === 1) {
        for (const retryable of [false, true]) {
          const forgedStoredMismatch = await call(
            "POST",
            `/v1/processor/jobs/${claim.job_id}/fail`,
            {
              lease_token: claim.lease_token,
              retryable,
              error_code: "STORED_OBJECT_SHA256_MISMATCH",
              error_message: "STORED_OBJECT_SHA256_MISMATCH",
            },
            PROCESSOR_TOKEN,
          );
          expect(forgedStoredMismatch.status).toBe(400);
          expect(await forgedStoredMismatch.json()).toMatchObject({
            error: { code: "INVALID_REQUEST" },
          });
          expect(await env.ARCHIVE.head(sourceKey)).not.toBeNull();
        }
      }

      const failed = await call(
        "POST",
        `/v1/processor/jobs/${claim.job_id}/fail`,
        {
          lease_token: claim.lease_token,
          retryable: true,
          error_code: "OBJECT_DOWNLOAD_INTEGRITY_MISMATCH",
          error_message: "OBJECT_DOWNLOAD_INTEGRITY_MISMATCH",
        },
        PROCESSOR_TOKEN,
      );
      expect(failed.status, await failed.clone().text()).toBe(200);
      if (attempt < 5) {
        expect(await failed.json()).toMatchObject({ status: "queued" });
        expect(await env.ARCHIVE.head(sourceKey)).not.toBeNull();
        await env.DB.prepare(
          "UPDATE processing_jobs SET next_attempt_at = ?1 WHERE id = ?2",
        )
          .bind(Math.floor(Date.now() / 1000), claim.job_id)
          .run();
      } else {
        expect(await failed.json()).toEqual({
          job_id: claim.job_id,
          status: "failed",
          input_status: "purged",
        });
      }
    }

    expect(await env.ARCHIVE.head(sourceKey)).toBeNull();
    expect(
      await env.DB.prepare(
        "SELECT status, error_code, input_purged_at FROM processing_jobs WHERE id = ?1",
      )
        .bind(jobId)
        .first<{
          status: string;
          error_code: string;
          input_purged_at: number | null;
        }>(),
    ).toMatchObject({
      status: "failed",
      error_code: "STORED_OBJECT_SHA256_MISMATCH",
    });
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM received_series_reservations
         WHERE upload_id = ?1 AND bundle_id = ?2`,
      )
        .bind(received.uploadId, "e".repeat(24))
        .first<number>("count"),
    ).toBe(0);
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM released_series_reservations
         WHERE processing_job_id = ?1`,
      )
        .bind(jobId)
        .first<number>("count"),
    ).toBe(1);
    const repairableStatus = await call(
      "GET",
      `/v1/dicom-uploads/${received.uploadId}`,
      undefined,
      token,
    );
    expect(repairableStatus.status).toBe(200);
    expect(await repairableStatus.json()).toMatchObject({
      processing: { repairable_series: 1 },
    });

    const substitutedPayload = new TextEncoder().encode(
      "substituted-payload".padEnd(96, "."),
    );
    const substituted = await call(
      "POST",
      "/v1/dicom-uploads",
      {
        format: "dicom-series-v1",
        client_version: "0.3.0",
        deidentification: {
          policy_id: "scaling-neuro.dicom-deidentification",
          policy_version: "1.0.0",
        },
        series: [
          {
            series_archive_id: "e".repeat(24),
            series_id: "700".padStart(24, "0"),
            subject_id: "8".repeat(24),
            session_id: "9".repeat(24),
            protocol_group_id: "a00".padStart(24, "0"),
            dicom_count: 10,
            archive: {
              format: "dicom-tar-zstd",
              relative_key: `${"e".repeat(24)}/dicom.tar.zst`,
              size: substitutedPayload.byteLength,
              sha256: await sha256Hex(substitutedPayload),
            },
          },
        ],
      },
      token,
    );
    expect(substituted.status).toBe(409);
    expect(await substituted.json()).toMatchObject({
      error: {
        code: "DUPLICATE_BUNDLE",
        details: {
          reason: "identity_conflict",
          series_archive_id: "e".repeat(24),
        },
      },
    });

    // The same deterministic folder receipt can replace one independently
    // proven-corrupt stored object without changing its scientific identity.
    const replacement = await receiveSmallDicomUpload(token, ["e".repeat(24)]);
    expect(replacement.uploadId).not.toBe(received.uploadId);
    const replacementKey = replacement.objectKeys.get("e".repeat(24))!;
    expect(await env.ARCHIVE.head(replacementKey)).not.toBeNull();

    // A second independently established integrity failure is terminal. This
    // bounds replacement abuse and preserves a withdrawn identity tombstone.
    await env.DB.prepare(
      "UPDATE processing_jobs SET attempt = 4 WHERE upload_id = ?1",
    )
      .bind(replacement.uploadId)
      .run();
    const replacementClaimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("integrity-replacement-redownload"),
      PROCESSOR_TOKEN,
    );
    expect(replacementClaimResponse.status).toBe(200);
    const replacementClaim = await replacementClaimResponse.json<{
      job_id: string;
      attempt: number;
      lease_token: string;
    }>();
    expect(replacementClaim.attempt).toBe(5);
    const replacementFailed = await call(
      "POST",
      `/v1/processor/jobs/${replacementClaim.job_id}/fail`,
      {
        lease_token: replacementClaim.lease_token,
        retryable: true,
        error_code: "OBJECT_DOWNLOAD_INTEGRITY_MISMATCH",
        error_message: "OBJECT_DOWNLOAD_INTEGRITY_MISMATCH",
      },
      PROCESSOR_TOKEN,
    );
    expect(replacementFailed.status).toBe(200);
    expect(await replacementFailed.json()).toEqual({
      job_id: replacementClaim.job_id,
      status: "failed",
      input_status: "purged",
    });
    expect(await env.ARCHIVE.head(replacementKey)).toBeNull();
    expect(
      await env.DB.prepare(
        `SELECT withdrawn_at FROM received_series_reservations
         WHERE upload_id = ?1 AND bundle_id = ?2`,
      )
        .bind(replacement.uploadId, "e".repeat(24))
        .first<number | null>("withdrawn_at"),
    ).not.toBeNull();
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM released_series_reservations
         WHERE site_id = (SELECT site_id FROM uploads WHERE id = ?1)
           AND project_id = (SELECT project_id FROM uploads WHERE id = ?1)
           AND series_archive_id = ?2`,
      )
        .bind(replacement.uploadId, "e".repeat(24))
        .first<number>("count"),
    ).toBe(1);
    const terminalStatus = await call(
      "GET",
      `/v1/dicom-uploads/${replacement.uploadId}`,
      undefined,
      token,
    );
    expect(terminalStatus.status).toBe(200);
    expect(await terminalStatus.json()).toMatchObject({
      processing: { repairable_series: 0 },
    });
  });

  it("turns a released integrity replacement into a permanent withdrawal tombstone", async () => {
    const { token } = await enrolledDevice();
    const seriesArchiveId = "d".repeat(24);
    const received = await receiveSmallDicomUpload(token, [seriesArchiveId]);
    await env.DB.prepare(
      "UPDATE processing_jobs SET attempt = 4 WHERE upload_id = ?1",
    )
      .bind(received.uploadId)
      .run();
    const claimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("integrity-release-before-withdrawal"),
      PROCESSOR_TOKEN,
    );
    expect(claimResponse.status).toBe(200);
    const claim = await claimResponse.json<{
      job_id: string;
      attempt: number;
      lease_token: string;
    }>();
    expect(claim.attempt).toBe(5);
    const failed = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/fail`,
      {
        lease_token: claim.lease_token,
        retryable: true,
        error_code: "OBJECT_DOWNLOAD_INTEGRITY_MISMATCH",
        error_message: "OBJECT_DOWNLOAD_INTEGRITY_MISMATCH",
      },
      PROCESSOR_TOKEN,
    );
    expect(failed.status).toBe(200);

    const withdrawal = await call(
      "POST",
      `/v1/admin/uploads/${received.uploadId}/withdraw`,
      {},
      ADMIN_TOKEN,
    );
    expect(withdrawal.status).toBe(200);
    expect(
      await env.DB.prepare(
        `SELECT withdrawn_at FROM released_series_reservations
         WHERE upload_id = ?1 AND series_archive_id = ?2`,
      )
        .bind(received.uploadId, seriesArchiveId)
        .first<number | null>("withdrawn_at"),
    ).not.toBeNull();

    const payload = new TextEncoder().encode(
      `privacy-cleared-dicom-${seriesArchiveId}`.padEnd(96, "."),
    );
    const replacement = await call(
      "POST",
      "/v1/dicom-uploads",
      {
        format: "dicom-series-v1",
        client_version: "0.3.0",
        deidentification: {
          policy_id: "scaling-neuro.dicom-deidentification",
          policy_version: "1.0.0",
        },
        series: [
          {
            series_archive_id: seriesArchiveId,
            series_id: "700".padStart(24, "0"),
            subject_id: "8".repeat(24),
            session_id: "9".repeat(24),
            protocol_group_id: "a00".padStart(24, "0"),
            dicom_count: 10,
            archive: {
              format: "dicom-tar-zstd",
              relative_key: `${seriesArchiveId}/dicom.tar.zst`,
              size: payload.byteLength,
              sha256: await sha256Hex(payload),
            },
          },
        ],
      },
      token,
    );
    expect(replacement.status).toBe(409);
    expect(await replacement.json()).toMatchObject({
      error: {
        code: "DUPLICATE_BUNDLE",
        details: {
          reason: "withdrawn_tombstone",
          series_archive_id: seriesArchiveId,
        },
      },
    });
  });

  it("re-sweeps withdrawn prefixes after outstanding output grants", async () => {
    const { token } = await enrolledDevice();
    const seriesArchiveId = "f".repeat(24);
    const received = await receiveSmallDicomUpload(token, [seriesArchiveId]);
    const claimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("withdrawal-race"),
      PROCESSOR_TOKEN,
    );
    const claim = await claimResponse.json<{
      job_id: string;
      lease_token: string;
    }>();
    const outputSha = await sha256Hex(new Uint8Array(32));
    const grants = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/outputs`,
      {
        lease_token: claim.lease_token,
        outputs: [
          {
            kind: "nifti",
            size_bytes: 32,
            sha256: outputSha,
            uncompressed_sha256: outputSha,
            content_type: "application/gzip",
          },
          {
            kind: "sidecar",
            size_bytes: 2,
            sha256: await sha256Hex(new TextEncoder().encode("{}")),
            content_type: "application/json",
          },
          {
            kind: "processing_manifest",
            size_bytes: 2,
            sha256: await sha256Hex(new TextEncoder().encode("{}")),
            content_type: "application/json",
          },
        ],
      },
      PROCESSOR_TOKEN,
    );
    expect(grants.status, await grants.clone().text()).toBe(200);

    const prefix = await env.DB.prepare(
      "SELECT archive_prefix FROM uploads WHERE id = ?1",
    )
      .bind(received.uploadId)
      .first<string>("archive_prefix");
    const lateKey = `${prefix}processed/${seriesArchiveId}/bold.nii.gz`;
    const withdrawal = await call(
      "POST",
      `/v1/admin/uploads/${received.uploadId}/withdraw`,
      undefined,
      ADMIN_TOKEN,
    );
    expect(withdrawal.status).toBe(200);
    expect(
      await env.DB.prepare("SELECT purged_at FROM uploads WHERE id = ?1")
        .bind(received.uploadId)
        .first<number | null>("purged_at"),
    ).toBeNull();

    // Model a PUT that began with a capability minted before withdrawal and
    // completed after the immediate prefix deletion.
    await env.ARCHIVE.put(lateKey, new Uint8Array(32));
    await env.DB.prepare(
      "UPDATE uploads SET updated_at = ?1 WHERE id = ?2",
    )
      .bind(Math.floor(Date.now() / 1000) - 901, received.uploadId)
      .run();
    const firstSweep = await call(
      "POST",
      "/v1/admin/cleanup",
      undefined,
      ADMIN_TOKEN,
    );
    expect(firstSweep.status).toBe(200);
    expect(await env.ARCHIVE.head(lateKey)).toBeNull();
    expect(
      await env.DB.prepare("SELECT purged_at FROM uploads WHERE id = ?1")
        .bind(received.uploadId)
        .first<number | null>("purged_at"),
    ).toBeNull();

    // Finalization waits for the maximum supported in-flight transfer window.
    const settledAt = Math.floor(Date.now() / 1000) - 90_000;
    await env.ARCHIVE.put(lateKey, new Uint8Array(32));
    await env.DB.prepare(
      "UPDATE uploads SET withdrawn_at = ?1, updated_at = ?1 WHERE id = ?2",
    )
      .bind(settledAt, received.uploadId)
      .run();
    await call("POST", "/v1/admin/cleanup", undefined, ADMIN_TOKEN);
    expect(await env.ARCHIVE.head(lateKey)).toBeNull();
    expect(
      Number(
        await env.DB.prepare("SELECT purged_at FROM uploads WHERE id = ?1")
          .bind(received.uploadId)
          .first<number | null>("purged_at"),
      ),
    ).toBeGreaterThan(0);

  });

  it("purges terminal privacy rejects but retains converter failures", async () => {
    const { token } = await enrolledDevice();
    const identifiers = ["1".repeat(24), "2".repeat(24)];
    const received = await receiveSmallDicomUpload(token, identifiers);

    const privacyClaimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("privacy-auditor"),
      PROCESSOR_TOKEN,
    );
    expect(privacyClaimResponse.status).toBe(200);
    const privacyClaim = await privacyClaimResponse.json<{
      job_id: string;
      bundle_id: string;
      lease_token: string;
    }>();
    const privacyKey = received.objectKeys.get(privacyClaim.bundle_id)!;
    expect(await env.ARCHIVE.head(privacyKey)).not.toBeNull();
    const privacyFailure = await call(
      "POST",
      `/v1/processor/jobs/${privacyClaim.job_id}/fail`,
      {
        lease_token: privacyClaim.lease_token,
        retryable: false,
        error_code: "DICOM_PRIVACY_AUDIT_FAILED",
        error_message: "DICOM_PRIVACY_AUDIT_FAILED",
      },
      PROCESSOR_TOKEN,
    );
    expect(privacyFailure.status, await privacyFailure.clone().text()).toBe(
      200,
    );
    expect(await privacyFailure.json()).toMatchObject({
      status: "failed",
      input_status: "purged",
    });
    expect(await env.ARCHIVE.head(privacyKey)).toBeNull();

    const compatibilityClaimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("converter"),
      PROCESSOR_TOKEN,
    );
    expect(compatibilityClaimResponse.status).toBe(200);
    const compatibilityClaim = await compatibilityClaimResponse.json<{
      job_id: string;
      bundle_id: string;
      lease_token: string;
    }>();
    const compatibilityKey = received.objectKeys.get(
      compatibilityClaim.bundle_id,
    )!;
    const compatibilityFailure = await call(
      "POST",
      `/v1/processor/jobs/${compatibilityClaim.job_id}/fail`,
      {
        lease_token: compatibilityClaim.lease_token,
        retryable: false,
        error_code: "DCM2NIIX_FAILED",
        error_message: "DCM2NIIX_FAILED",
      },
      PROCESSOR_TOKEN,
    );
    expect(
      compatibilityFailure.status,
      await compatibilityFailure.clone().text(),
    ).toBe(200);
    expect(await compatibilityFailure.json()).toEqual({
      job_id: compatibilityClaim.job_id,
      status: "failed",
    });
    expect(await env.ARCHIVE.head(compatibilityKey)).not.toBeNull();

    const status = await call(
      "GET",
      `/v1/dicom-uploads/${received.uploadId}`,
      undefined,
      token,
    );
    expect(await status.json()).toMatchObject({
      processing: {
        status: "failed",
        failed_series: 2,
        purged_series: 1,
      },
    });
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM received_series_reservations
         WHERE upload_id = ?1 AND withdrawn_at IS NOT NULL`,
      )
        .bind(received.uploadId)
        .first<number>("count"),
    ).toBe(1);
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM audit_events
         WHERE upload_id = ?1 AND event_type = 'processing.input_purged'`,
      )
        .bind(received.uploadId)
        .first<number>("count"),
    ).toBe(1);
  });

  it("finishes a terminal privacy purge after a transient delete failure", async () => {
    const { token } = await enrolledDevice();
    const received = await receiveSmallDicomUpload(token, ["4".repeat(24)]);
    const claimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("privacy-auditor"),
      PROCESSOR_TOKEN,
    );
    expect(claimResponse.status).toBe(200);
    const claim = await claimResponse.json<{
      job_id: string;
      bundle_id: string;
      lease_token: string;
    }>();
    const sourceKey = received.objectKeys.get(claim.bundle_id)!;
    const deleteSpy = vi
      .spyOn(env.ARCHIVE, "delete")
      .mockRejectedValueOnce(new Error("temporary R2 failure"));
    const failure = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/fail`,
      {
        lease_token: claim.lease_token,
        retryable: false,
        error_code: "DICOM_PRIVACY_AUDIT_FAILED",
        error_message: "DICOM_PRIVACY_AUDIT_FAILED",
      },
      PROCESSOR_TOKEN,
    );
    expect(failure.status).toBe(502);
    expect(await failure.json()).toMatchObject({
      error: { code: "STORAGE_UNAVAILABLE" },
    });
    deleteSpy.mockRestore();

    // The lease was made terminal before deletion, so no converter can publish
    // the rejected input while cleanup is pending.
    expect(
      await env.DB.prepare(
        "SELECT status, input_purged_at FROM processing_jobs WHERE id = ?1",
      )
        .bind(claim.job_id)
        .first<{ status: string; input_purged_at: number | null }>(),
    ).toEqual({ status: "failed", input_purged_at: null });
    expect(await env.ARCHIVE.head(sourceKey)).not.toBeNull();

    // The next authenticated claim finishes pending purges before taking work.
    const nextClaim = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("cleanup-pass"),
      PROCESSOR_TOKEN,
    );
    expect(nextClaim.status).toBe(204);
    expect(await env.ARCHIVE.head(sourceKey)).toBeNull();
    const purgedAt = await env.DB.prepare(
      "SELECT input_purged_at FROM processing_jobs WHERE id = ?1",
    )
      .bind(claim.job_id)
      .first<number>("input_purged_at");
    expect(Number(purgedAt)).toBeGreaterThan(0);
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM audit_events
         WHERE event_type = 'processing.input_purged'
           AND subject_id = ?1`,
      )
        .bind(claim.job_id)
        .first<number>("count"),
    ).toBe(1);
  });

  it("does not let one rejected-input cleanup failure starve other jobs", async () => {
    const { token } = await enrolledDevice();
    const received = await receiveSmallDicomUpload(token, [
      "7".repeat(24),
      "8".repeat(24),
    ]);
    const claimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("failing-purge"),
      PROCESSOR_TOKEN,
    );
    const claim = await claimResponse.json<{
      job_id: string;
      bundle_id: string;
      lease_token: string;
    }>();
    const rejectedKey = received.objectKeys.get(claim.bundle_id)!;
    const deleteSpy = vi
      .spyOn(env.ARCHIVE, "delete")
      .mockRejectedValue(new Error("persistent R2 delete outage"));
    const failure = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/fail`,
      {
        lease_token: claim.lease_token,
        retryable: false,
        error_code: "DICOM_PRIVACY_AUDIT_FAILED",
        error_message: "DICOM_PRIVACY_AUDIT_FAILED",
      },
      PROCESSOR_TOKEN,
    );
    expect(failure.status).toBe(502);

    const unrelated = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("unrelated-converter"),
      PROCESSOR_TOKEN,
    );
    expect(unrelated.status, await unrelated.clone().text()).toBe(200);
    expect(await unrelated.json()).not.toMatchObject({ job_id: claim.job_id });
    expect(await env.ARCHIVE.head(rejectedKey)).not.toBeNull();
    deleteSpy.mockRestore();

    const cleanup = await call(
      "POST",
      "/v1/admin/cleanup",
      undefined,
      ADMIN_TOKEN,
    );
    expect(cleanup.status).toBe(200);
    expect(await env.ARCHIVE.head(rejectedKey)).toBeNull();
  });

  it("receives a production-shaped session without opening scientific bytes", async () => {
    const { token } = await enrolledDevice();
    const seriesCount = 8;
    const payloads = new Map<string, Uint8Array<ArrayBuffer>>();
    const series = [];
    for (let index = 0; index < seriesCount; index += 1) {
      const seriesArchiveId = (0xa0 + index).toString(16).padStart(24, "0");
      const payload = new TextEncoder().encode(
        `deidentified-dicom-series-${index}`.padEnd(128, "."),
      );
      const relativeKey = `${seriesArchiveId}/dicom.tar.zst`;
      payloads.set(relativeKey, payload);
      series.push({
        series_archive_id: seriesArchiveId,
        series_id: (0xb0 + index).toString(16).padStart(24, "0"),
        subject_id: "c".repeat(24),
        session_id: "d".repeat(24),
        protocol_group_id: (0xe0 + index).toString(16).padStart(24, "0"),
        dicom_count: 300 + index,
        archive: {
          format: "dicom-tar-zstd",
          relative_key: relativeKey,
          size: payload.byteLength,
          sha256: await sha256Hex(payload),
        },
      });
    }
    const requestBody = {
      format: "dicom-series-v1",
      client_version: "0.3.0",
      deidentification: {
        policy_id: "scaling-neuro.dicom-deidentification",
        policy_version: "1.0.0",
      },
      series,
    };
    const staleClient = await call(
      "POST",
      "/v1/dicom-uploads",
      { ...requestBody, client_version: "0.2.9" },
      token,
    );
    expect(staleClient.status).toBe(426);
    expect(await staleClient.json()).toMatchObject({
      error: {
        code: "CLIENT_UPDATE_REQUIRED",
        details: { minimum_client_version: "0.3.0" },
      },
    });
    const allocationResponse = await call(
      "POST",
      "/v1/dicom-uploads",
      requestBody,
      token,
    );
    expect(
      allocationResponse.status,
      await allocationResponse.clone().text(),
    ).toBe(201);
    const allocation = await allocationResponse.json<{
      upload_id: string;
      object_prefix: string;
      multipart_objects: Array<{
        key: string;
        upload_id: string;
        part_size: number;
      }>;
    }>();
    expect(allocation.multipart_objects).toHaveLength(seriesCount);

    // Exact request replay recovers the same remote session even if local
    // state was lost before the allocation response was saved.
    const replay = await call("POST", "/v1/dicom-uploads", requestBody, token);
    expect(replay.status).toBe(200);
    expect(await replay.json()).toMatchObject({
      upload_id: allocation.upload_id,
    });

    const completionObjects = [];
    for (const multipart of allocation.multipart_objects) {
      const relativeKey = multipart.key.slice(allocation.object_prefix.length);
      const payload = payloads.get(relativeKey);
      expect(payload).toBeDefined();
      const part = await env.ARCHIVE.resumeMultipartUpload(
        multipart.key,
        multipart.upload_id,
      ).uploadPart(1, payload!);
      const descriptor = series.find(
        (item) => item.archive.relative_key === relativeKey,
      );
      completionObjects.push({
        key: multipart.key,
        size: descriptor!.archive.size,
        sha256: descriptor!.archive.sha256,
        parts: [{ part_number: 1, etag: part.etag }],
      });
    }

    const getSpy = vi.spyOn(env.ARCHIVE, "get");
    const completedResponse = await call(
      "POST",
      `/v1/dicom-uploads/${allocation.upload_id}/complete`,
      { objects: completionObjects },
      token,
    );
    expect(
      completedResponse.status,
      await completedResponse.clone().text(),
    ).toBe(200);
    expect(await completedResponse.json()).toMatchObject({
      upload_id: allocation.upload_id,
      status: "committed",
      format: "dicom-series-v1",
      receipt: {
        received_series: seriesCount,
        total_series: seriesCount,
      },
      processing: {
        status: "queued",
        queued_series: seriesCount,
        total_series: seriesCount,
      },
    });
    expect(getSpy).not.toHaveBeenCalled();
    getSpy.mockRestore();
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM processing_jobs WHERE upload_id = ?1",
      )
        .bind(allocation.upload_id)
        .first<number>("count"),
    ).toBe(seriesCount);

    // POST allocation replay always keeps the session-response shape, even
    // after commit; it never swaps in the richer GET status document.
    const committedReplay = await call(
      "POST",
      "/v1/dicom-uploads",
      requestBody,
      token,
    );
    expect(committedReplay.status).toBe(200);
    expect(await committedReplay.json()).toEqual({
      upload_id: allocation.upload_id,
      status: "committed",
      format: "dicom-series-v1",
      object_prefix: allocation.object_prefix,
      multipart_objects: [],
    });

    const unauthorized = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("cluster-a"),
      "not-the-processor-token-but-long-enough",
    );
    expect(unauthorized.status).toBe(401);
    const claimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("cluster-a"),
      PROCESSOR_TOKEN,
    );
    expect(claimResponse.status).toBe(200);
    const claim = await claimResponse.json<{
      job_id: string;
      lease_token: string;
      input_format: string;
      input: { format: string; dicom_count: number; url: string };
    }>();
    expect(claim).toMatchObject({
      input_format: "dicom-series-v1",
      input: { format: "dicom-tar-zstd" },
    });
    expect(claim.input.url).toContain("X-Amz-Signature=");

    const heartbeat = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/heartbeat`,
      { lease_token: claim.lease_token, lease_seconds: 900 },
      PROCESSOR_TOKEN,
    );
    expect(heartbeat.status).toBe(200);

    const niftiBytes = new TextEncoder().encode("gzip-placeholder".padEnd(64));
    const sidecarBytes = new TextEncoder().encode('{"RepetitionTime":1.5}');
    const manifestBytes = new TextEncoder().encode('{"processor":"test"}');
    const outputRequest = {
      lease_token: claim.lease_token,
      outputs: [
        {
          kind: "nifti",
          size_bytes: niftiBytes.byteLength,
          sha256: await sha256Hex(niftiBytes),
          content_type: "application/gzip",
          uncompressed_sha256: "a".repeat(64),
        },
        {
          kind: "sidecar",
          size_bytes: sidecarBytes.byteLength,
          sha256: await sha256Hex(sidecarBytes),
          content_type: "application/json",
        },
        {
          kind: "processing_manifest",
          size_bytes: manifestBytes.byteLength,
          sha256: await sha256Hex(manifestBytes),
          content_type: "application/json",
        },
      ],
    };
    const grantsResponse = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/outputs`,
      outputRequest,
      PROCESSOR_TOKEN,
    );
    expect(grantsResponse.status, await grantsResponse.clone().text()).toBe(
      200,
    );
    const grants = await grantsResponse.json<{
      outputs: Array<{ kind: string; headers: Record<string, string> }>;
    }>();
    expect(grants.outputs).toHaveLength(3);
    expect(grants.outputs[0]?.headers).toHaveProperty("x-amz-content-sha256");

    const outputRows = await env.DB.prepare(
      "SELECT kind, object_key, expected_sha256, content_type FROM processing_job_outputs WHERE job_id = ?1",
    )
      .bind(claim.job_id)
      .all<{
        kind: string;
        object_key: string;
        expected_sha256: string;
        content_type: string;
      }>();
    const bytesByKind: Record<string, Uint8Array<ArrayBuffer>> = {
      nifti: niftiBytes,
      sidecar: sidecarBytes,
      processing_manifest: manifestBytes,
    };
    for (const output of outputRows.results) {
      await env.ARCHIVE.put(output.object_key, bytesByKind[output.kind]!, {
        httpMetadata: { contentType: output.content_type },
        customMetadata: {
          job_id: claim.job_id,
          kind: output.kind,
          sha256: output.expected_sha256,
        },
      });
    }
    // Expire the lease after the endpoint's initial authentication but before
    // its D1 publication batch. No output or catalog row may be published by
    // this now-stale processor.
    await env.DB.prepare(
      "UPDATE processing_jobs SET next_attempt_at = ?1 WHERE upload_id = ?2 AND id != ?3",
    )
      .bind(
        Math.floor(Date.now() / 1000) + 3600,
        allocation.upload_id,
        claim.job_id,
      )
      .run();
    const originalHead = env.ARCHIVE.head.bind(env.ARCHIVE);
    let expiredDuringHead = false;
    const expirySpy = vi
      .spyOn(env.ARCHIVE, "head")
      .mockImplementation(async (...args: Parameters<R2Bucket["head"]>) => {
        if (!expiredDuringHead) {
          expiredDuringHead = true;
          await env.DB.prepare(
            "UPDATE processing_jobs SET lease_expires_at = ?1 WHERE id = ?2",
          )
            .bind(Math.floor(Date.now() / 1000) - 1, claim.job_id)
            .run();
        }
        return originalHead(...args);
      });
    const staleCompletion = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/complete`,
      {
        lease_token: claim.lease_token,
        processor_version: "1.0.0",
        dcm2niix_version: "1.0.20250506",
        outputs: outputRequest.outputs,
        validation: {
          archive_sha256_verified: true,
          dicom_count: claim.input.dicom_count,
          dicom_parse_succeeded: true,
          functional_epi_confirmed: true,
        },
      },
      PROCESSOR_TOKEN,
    );
    expirySpy.mockRestore();
    expect(staleCompletion.status).toBe(409);
    expect(await staleCompletion.json()).toMatchObject({
      error: { code: "LEASE_LOST" },
    });
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM processing_job_outputs WHERE job_id = ?1 AND completed_at IS NOT NULL",
      )
        .bind(claim.job_id)
        .first<number>("count"),
    ).toBe(0);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM catalog_series WHERE upload_id = ?1",
      )
        .bind(allocation.upload_id)
        .first<number>("count"),
    ).toBe(0);

    const reclaimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      processorClaim("cluster-b"),
      PROCESSOR_TOKEN,
    );
    expect(reclaimResponse.status).toBe(200);
    const reclaimed = await reclaimResponse.json<{
      job_id: string;
      lease_token: string;
    }>();
    expect(reclaimed.job_id).toBe(claim.job_id);
    const completionRequest = {
      lease_token: reclaimed.lease_token,
      processor_version: "1.0.0",
      dcm2niix_version: "1.0.20250506",
      outputs: outputRequest.outputs,
      validation: {
        archive_sha256_verified: true,
        dicom_count: claim.input.dicom_count,
        dicom_parse_succeeded: true,
        functional_epi_confirmed: true,
      },
    };
    const processed = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/complete`,
      completionRequest,
      PROCESSOR_TOKEN,
    );
    expect(processed.status, await processed.clone().text()).toBe(200);
    expect(await processed.json()).toMatchObject({ status: "processed" });
    const lostResponseReplay = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/complete`,
      completionRequest,
      PROCESSOR_TOKEN,
    );
    expect(lostResponseReplay.status).toBe(200);
    expect(await lostResponseReplay.json()).toEqual({
      job_id: claim.job_id,
      upload_id: allocation.upload_id,
      status: "processed",
    });
    const divergentReplay = await call(
      "POST",
      `/v1/processor/jobs/${claim.job_id}/complete`,
      { ...completionRequest, processor_version: "different-result" },
      PROCESSOR_TOKEN,
    );
    expect(divergentReplay.status).toBe(409);
    expect(await divergentReplay.json()).toMatchObject({
      error: { code: "CONFLICT" },
    });
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM catalog_series WHERE upload_id = ?1",
      )
        .bind(allocation.upload_id)
        .first<number>("count"),
    ).toBe(1);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM audit_events WHERE upload_id = ?1 AND event_type = 'processing.processed'",
      )
        .bind(allocation.upload_id)
        .first<number>("count"),
    ).toBe(1);
  }, 15_000);

  it("converges two workstations racing the same series without a conflict loop", async () => {
    const inviteResponse = await call(
      "POST",
      "/v1/admin/invites",
      {
        site_slug: `race-${crypto.randomUUID().slice(0, 8)}`,
        site_name: "Shared lab",
        project_slug: "epi",
        project_name: "EPI",
        consent_policy_version: "pilot-1",
        expires_in_seconds: 3600,
        max_uses: 2,
      },
      ADMIN_TOKEN,
    );
    const invite = await inviteResponse.json<{ invite_code: string }>();
    const tokens = [deviceToken(), deviceToken()];
    for (const [index, token] of tokens.entries()) {
      const enrollment = await call("POST", "/v1/enroll", {
        invite_code: invite.invite_code,
        enrollment_id: crypto.randomUUID(),
        device_token: token,
        device_name: `scanner-${index + 1}`,
        client_version: "0.3.0",
        platform: "linux-x64",
      });
      expect(enrollment.status).toBe(201);
    }
    const archiveId = "7".repeat(24);
    const payload = new TextEncoder().encode(
      "same-deidentified-series".padEnd(128, "."),
    );
    const body = {
      format: "dicom-series-v1",
      client_version: "0.3.0",
      deidentification: {
        policy_id: "scaling-neuro.dicom-deidentification",
        policy_version: "1.0.0",
      },
      series: [
        {
          series_archive_id: archiveId,
          series_id: "8".repeat(24),
          subject_id: "9".repeat(24),
          session_id: "a".repeat(24),
          protocol_group_id: "b".repeat(24),
          dicom_count: 240,
          archive: {
            format: "dicom-tar-zstd",
            relative_key: `${archiveId}/dicom.tar.zst`,
            size: payload.byteLength,
            sha256: await sha256Hex(payload),
          },
        },
      ],
    };
    const allocations = [];
    for (const token of tokens) {
      const response = await call("POST", "/v1/dicom-uploads", body, token);
      expect(response.status).toBe(201);
      allocations.push(
        await response.json<{
          upload_id: string;
          multipart_objects: Array<{ key: string; upload_id: string }>;
        }>(),
      );
    }
    const completionBodies = [];
    for (const allocation of allocations) {
      const object = allocation.multipart_objects[0]!;
      const part = await env.ARCHIVE.resumeMultipartUpload(
        object.key,
        object.upload_id,
      ).uploadPart(1, payload);
      completionBodies.push({
        objects: [
          {
            key: object.key,
            size: payload.byteLength,
            sha256: body.series[0]!.archive.sha256,
            parts: [{ part_number: 1, etag: part.etag }],
          },
        ],
      });
    }
    const winner = await call(
      "POST",
      `/v1/dicom-uploads/${allocations[0]!.upload_id}/complete`,
      completionBodies[0],
      tokens[0],
    );
    expect(winner.status).toBe(200);
    expect(await winner.json()).toMatchObject({ status: "committed" });
    const loser = await call(
      "POST",
      `/v1/dicom-uploads/${allocations[1]!.upload_id}/complete`,
      completionBodies[1],
      tokens[1],
    );
    expect(loser.status, await loser.clone().text()).toBe(200);
    const loserBody = await loser.json();
    expect(
      validateDicomUploadStatus(loserBody),
      responseAjv.errorsText(validateDicomUploadStatus.errors),
    ).toBe(true);
    expect(loserBody).toMatchObject({
      upload_id: allocations[1]!.upload_id,
      status: "already_received",
      already_received_series: [
        {
          series_archive_id: archiveId,
          receipt_upload_id: allocations[0]!.upload_id,
        },
      ],
    });
    expect(
      await env.DB.prepare(
        "SELECT status, receipt_reconciled_at, purged_at FROM uploads WHERE id = ?1",
      )
        .bind(allocations[1]!.upload_id)
        .first<{
          status: string;
          receipt_reconciled_at: number | null;
          purged_at: number | null;
        }>(),
    ).toMatchObject({
      status: "expired",
      purged_at: null,
    });
    expect(
      await env.DB.prepare(
        "SELECT receipt_reconciled_at FROM uploads WHERE id = ?1",
      )
        .bind(allocations[1]!.upload_id)
        .first<number>("receipt_reconciled_at"),
    ).toBeGreaterThan(0);
    expect(
      await env.ARCHIVE.head(allocations[1]!.multipart_objects[0]!.key),
    ).not.toBeNull();

    // The success response may have been lost. The identical completion is a
    // durable replay and also retries non-blocking R2 cleanup.
    const deleteSpy = vi
      .spyOn(env.ARCHIVE, "delete")
      .mockRejectedValueOnce(new Error("temporary duplicate cleanup failure"));
    const cleanupInterrupted = await call(
      "POST",
      `/v1/dicom-uploads/${allocations[1]!.upload_id}/complete`,
      completionBodies[1],
      tokens[1],
    );
    expect(cleanupInterrupted.status).toBe(200);
    expect(await cleanupInterrupted.json()).toMatchObject({
      status: "already_received",
      already_received_series: [
        {
          series_archive_id: archiveId,
          receipt_upload_id: allocations[0]!.upload_id,
        },
      ],
    });
    expect(
      await env.ARCHIVE.head(allocations[1]!.multipart_objects[0]!.key),
    ).not.toBeNull();
    deleteSpy.mockRestore();

    const completionReplay = await call(
      "POST",
      `/v1/dicom-uploads/${allocations[1]!.upload_id}/complete`,
      completionBodies[1],
      tokens[1],
    );
    expect(completionReplay.status).toBe(200);
    expect(
      await env.ARCHIVE.head(allocations[1]!.multipart_objects[0]!.key),
    ).toBeNull();

    // Once the winner is visible at create time, the second workstation gets
    // an immediate success and never allocates or retransmits another object.
    const replay = await call("POST", "/v1/dicom-uploads", body, tokens[1]);
    expect(replay.status).toBe(200);
    const replayBody = await replay.json();
    expect(
      validateDicomUploadSession(replayBody),
      responseAjv.errorsText(validateDicomUploadSession.errors),
    ).toBe(true);
    expect(replayBody).toMatchObject({
      status: "already_received",
      upload_id: allocations[1]!.upload_id,
      already_received_series: [
        {
          series_archive_id: archiveId,
          receipt_upload_id: allocations[0]!.upload_id,
        },
      ],
    });
  });

  it("reconciles a partial cross-workstation overlap without retransmitting unique series", async () => {
    const inviteResponse = await call(
      "POST",
      "/v1/admin/invites",
      {
        site_slug: `partial-${crypto.randomUUID().slice(0, 8)}`,
        site_name: "Partial overlap lab",
        project_slug: "epi",
        project_name: "EPI",
        consent_policy_version: "pilot-1",
        expires_in_seconds: 3600,
        max_uses: 2,
      },
      ADMIN_TOKEN,
    );
    const invite = await inviteResponse.json<{ invite_code: string }>();
    const tokens = [deviceToken(), deviceToken()];
    for (const [index, token] of tokens.entries()) {
      const response = await call("POST", "/v1/enroll", {
        invite_code: invite.invite_code,
        enrollment_id: crypto.randomUUID(),
        device_token: token,
        device_name: `partial-${index}`,
        client_version: "0.3.0",
        platform: "linux-x64",
      });
      expect(response.status).toBe(201);
    }
    const payloads = new Map<string, Uint8Array<ArrayBuffer>>();
    const makeSeries = async (digit: string, label: string) => {
      const archiveId = digit.repeat(24);
      const payload = new TextEncoder().encode(label.padEnd(128, "."));
      payloads.set(archiveId, payload);
      return {
        series_archive_id: archiveId,
        series_id: String(Number(digit) + 3).repeat(24),
        subject_id: "a".repeat(24),
        session_id: "b".repeat(24),
        protocol_group_id: String(Number(digit) + 6).repeat(24),
        dicom_count: 200,
        archive: {
          format: "dicom-tar-zstd",
          relative_key: `${archiveId}/dicom.tar.zst`,
          size: payload.byteLength,
          sha256: await sha256Hex(payload),
        },
      };
    };
    const shared = await makeSeries("1", "shared");
    const winnerOnly = await makeSeries("2", "winner-only");
    const loserOnly = await makeSeries("3", "loser-only");
    const common = {
      format: "dicom-series-v1",
      client_version: "0.3.0",
      deidentification: {
        policy_id: "scaling-neuro.dicom-deidentification",
        policy_version: "1.0.0",
      },
    };
    const bodies = [
      { ...common, series: [shared, winnerOnly] },
      { ...common, series: [shared, loserOnly] },
    ];
    const allocations = [];
    const completions = [];
    for (let index = 0; index < 2; index += 1) {
      const response = await call(
        "POST",
        "/v1/dicom-uploads",
        bodies[index],
        tokens[index],
      );
      expect(response.status).toBe(201);
      const allocation = await response.json<{
        upload_id: string;
        multipart_objects: Array<{
          key: string;
          upload_id: string;
          series_archive_id: string;
        }>;
      }>();
      allocations.push(allocation);
      const objects = [];
      for (const object of allocation.multipart_objects) {
        const payload = payloads.get(object.series_archive_id)!;
        const part = await env.ARCHIVE.resumeMultipartUpload(
          object.key,
          object.upload_id,
        ).uploadPart(1, payload);
        const descriptor = bodies[index]!.series.find(
          (series) => series.series_archive_id === object.series_archive_id,
        )!;
        objects.push({
          key: object.key,
          size: descriptor.archive.size,
          sha256: descriptor.archive.sha256,
          parts: [{ part_number: 1, etag: part.etag }],
        });
      }
      completions.push({ objects });
    }
    const first = await call(
      "POST",
      `/v1/dicom-uploads/${allocations[0]!.upload_id}/complete`,
      completions[0],
      tokens[0],
    );
    expect(first.status).toBe(200);
    const deleteSpy = vi
      .spyOn(env.ARCHIVE, "delete")
      .mockRejectedValueOnce(new Error("temporary partial cleanup failure"));
    const interrupted = await call(
      "POST",
      `/v1/dicom-uploads/${allocations[1]!.upload_id}/complete`,
      completions[1],
      tokens[1],
    );
    expect(interrupted.status).toBe(502);
    expect(await interrupted.json()).toMatchObject({
      error: { code: "STORAGE_UNAVAILABLE" },
    });
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM dicom_upload_reconciled_series
         WHERE upload_id = ?1`,
      )
        .bind(allocations[1]!.upload_id)
        .first<number>("count"),
    ).toBe(1);
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM dicom_upload_series WHERE upload_id = ?1",
      )
        .bind(allocations[1]!.upload_id)
        .first<number>("count"),
    ).toBe(1);
    expect(deleteSpy).not.toHaveBeenCalled();

    const cleanupInterrupted = await call(
      "POST",
      `/v1/dicom-uploads/${allocations[1]!.upload_id}/complete`,
      completions[1],
      tokens[1],
    );
    expect(cleanupInterrupted.status).toBe(502);
    expect(await cleanupInterrupted.json()).toMatchObject({
      error: { code: "STORAGE_UNAVAILABLE" },
    });
    deleteSpy.mockRestore();

    const resumed = await call(
      "POST",
      `/v1/dicom-uploads/${allocations[1]!.upload_id}/complete`,
      completions[1],
      tokens[1],
    );
    expect(resumed.status, await resumed.clone().text()).toBe(200);
    expect(await resumed.json()).toMatchObject({
      upload_id: allocations[1]!.upload_id,
      status: "committed",
      series_count: 1,
      already_received_series: [
        {
          series_archive_id: shared.series_archive_id,
          receipt_upload_id: allocations[0]!.upload_id,
        },
      ],
      processing: { queued_series: 1, total_series: 1 },
    });
    expect(
      await env.DB.prepare(
        "SELECT COUNT(*) AS count FROM processing_jobs WHERE upload_id = ?1",
      )
        .bind(allocations[1]!.upload_id)
        .first<number>("count"),
    ).toBe(1);
    const losingSharedKey = allocations[1]!.multipart_objects.find(
      (object) => object.series_archive_id === shared.series_archive_id,
    )!.key;
    const losingUniqueKey = allocations[1]!.multipart_objects.find(
      (object) => object.series_archive_id === loserOnly.series_archive_id,
    )!.key;
    expect(await env.ARCHIVE.head(losingSharedKey)).toBeNull();
    expect(await env.ARCHIVE.head(losingUniqueKey)).not.toBeNull();

    // A client may have checkpointed the original completion body before the
    // partial race was reconciled. Replaying that exact body must converge to
    // the committed receipt instead of validating now-retired duplicate rows.
    const replay = await call(
      "POST",
      `/v1/dicom-uploads/${allocations[1]!.upload_id}/complete`,
      completions[1],
      tokens[1],
    );
    expect(replay.status, await replay.clone().text()).toBe(200);
    expect(await replay.json()).toMatchObject({
      upload_id: allocations[1]!.upload_id,
      status: "committed",
      series_count: 1,
      already_received_series: [
        {
          series_archive_id: shared.series_archive_id,
          receipt_upload_id: allocations[0]!.upload_id,
        },
      ],
    });
  });
});
