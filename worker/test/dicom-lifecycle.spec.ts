import { env } from "cloudflare:workers";
import {
  createExecutionContext,
  waitOnExecutionContext,
} from "cloudflare:test";
import { describe, expect, it, vi } from "vitest";
import { sha256Hex } from "../src/crypto";
import { fetchHandler } from "../src/index";

const ADMIN_TOKEN = "test-admin-token-with-sufficient-entropy";
const PROCESSOR_TOKEN = "test-processor-token-with-sufficient-entropy";

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

async function receiveSmallDicomUpload(
  token: string,
  seriesArchiveIds: string[],
): Promise<{
  uploadId: string;
  objectKeys: Map<string, string>;
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
  const completed = await call(
    "POST",
    `/v1/dicom-uploads/${allocation.upload_id}/complete`,
    { objects },
    token,
  );
  expect(completed.status, await completed.clone().text()).toBe(200);
  return { uploadId: allocation.upload_id, objectKeys };
}

describe("DICOM receipt and processing queue", () => {
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

  it("replays a lost claim response without consuming another job or attempt", async () => {
    const { token } = await enrolledDevice();
    await receiveSmallDicomUpload(token, ["5".repeat(24), "6".repeat(24)]);

    const firstResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      {
        processor_id: "claim-replay-consumer",
        lease_seconds: 900,
        claim_input_format: "dicom-series-v1",
      },
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
      {
        processor_id: "claim-replay-consumer",
        lease_seconds: 900,
        claim_input_format: "nifti-v1",
      },
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
      {
        processor_id: "claim-replay-consumer",
        lease_seconds: 900,
        claim_input_format: "dicom-series-v1",
      },
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
      { processor_id: "independent-consumer", lease_seconds: 900 },
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
          {
            processor_id: "concurrent-same-consumer",
            lease_seconds: 900,
            claim_input_format: "dicom-series-v1",
          },
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
      { processor_id: "raw-priority-consumer", lease_seconds: 900 },
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
      {
        processor_id: "raw-only-consumer",
        lease_seconds: 900,
        claim_input_format: "dicom-series-v1",
      },
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

  it("purges a source whose downloaded archive fails whole-object integrity", async () => {
    const { token } = await enrolledDevice();
    const received = await receiveSmallDicomUpload(token, ["0".repeat(24)]);
    const claimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      { processor_id: "archive-integrity-auditor", lease_seconds: 900 },
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
    expect(await failed.json()).toMatchObject({
      status: "failed",
      input_status: "purged",
    });
    expect(await env.ARCHIVE.head(sourceKey)).toBeNull();
  });

  it("purges terminal privacy rejects but retains converter failures", async () => {
    const { token } = await enrolledDevice();
    const identifiers = ["1".repeat(24), "2".repeat(24)];
    const received = await receiveSmallDicomUpload(token, identifiers);

    const privacyClaimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      { processor_id: "privacy-auditor", lease_seconds: 900 },
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
      { processor_id: "converter", lease_seconds: 900 },
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
      { processor_id: "privacy-auditor", lease_seconds: 900 },
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
      { processor_id: "cleanup-pass", lease_seconds: 900 },
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
      { processor_id: "failing-purge", lease_seconds: 900 },
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
      { processor_id: "unrelated-converter", lease_seconds: 900 },
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
      { processor_id: "cluster-a", lease_seconds: 900 },
      "not-the-processor-token-but-long-enough",
    );
    expect(unauthorized.status).toBe(401);
    const claimResponse = await call(
      "POST",
      "/v1/processor/jobs/claim",
      { processor_id: "cluster-a", lease_seconds: 900 },
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
      { processor_id: "cluster-b", lease_seconds: 900 },
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
  });

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
    expect(await loser.json()).toMatchObject({
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
    expect(await replay.json()).toMatchObject({
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
