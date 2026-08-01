import { env } from "cloudflare:workers";
import {
  createExecutionContext,
  waitOnExecutionContext,
} from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { sha256Hex } from "../src/crypto";
import { fetchHandler } from "../src/index";

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
  const context = createExecutionContext();
  const response = await fetchHandler(request, env, context);
  await waitOnExecutionContext(context);
  return response;
}

function accessRequest(
  email: string,
  plansToContribute = true,
): Record<string, unknown> {
  return {
    contact_name: "Example Researcher",
    contact_email: email,
    institution_name: "Example University",
    lab_name: "Example Neuroimaging Lab",
    plans_to_contribute: plansToContribute,
    contributor_attestation: plansToContribute,
    accepted_contribution_policy_version: plansToContribute
      ? "open-epi-4.0.0"
      : null,
    data_use_agreement: true,
    accepted_data_use_policy_version: "archive-access-2.0.0",
  };
}

const ADMIN_TOKEN = "test-archive-access-admin-token-0000000000000000";

async function submitAndApprove(email: string): Promise<{
  access_token: string;
  token_type: string;
  archive_url: string;
  accepted_data_use_policy_version: string;
}> {
  const submitted = await call(
    "POST",
    "/v1/archive-access",
    accessRequest(email),
  );
  expect(submitted.status).toBe(202);
  const pending = await submitted.json<{
    request_id: string;
    status: string;
  }>();
  expect(pending.status).toBe("pending_review");
  const approved = await call(
    "POST",
    `/v1/admin/archive-access-requests/${pending.request_id}/approve`,
    undefined,
    ADMIN_TOKEN,
  );
  expect(approved.status).toBe(200);
  return approved.json<{
    access_token: string;
    token_type: string;
    archive_url: string;
    accepted_data_use_policy_version: string;
  }>();
}

describe("shared EPI archive access", () => {
  it("holds the public form for review and grants access only after approval", async () => {
    const email = `archive+${crypto.randomUUID()}@example.edu`;
    const response = await call(
      "POST",
      "/v1/archive-access",
      accessRequest(email),
    );
    expect(response.status).toBe(202);
    const pending = await response.json<{
      request_id: string;
      status: string;
      message: string;
    }>();
    expect(pending).toMatchObject({
      status: "pending_review",
      message:
        "Your request is pending review. We will email next steps to your work address.",
    });
    expect(pending).not.toHaveProperty("access_token");

    const stored = await env.DB.prepare(
      `SELECT email_hash, email_ciphertext, status, plans_to_contribute,
              contributor_attestation, accepted_contribution_policy_version,
              contributor_attestation_accepted_at, data_use_agreement,
              accepted_data_use_policy_version,
              data_use_agreement_accepted_at
       FROM archive_access_requests
       WHERE lab_name = ?1 LIMIT 1`,
    )
      .bind("Example Neuroimaging Lab")
      .first<{
        email_hash: string;
        email_ciphertext: string;
        status: string;
        plans_to_contribute: number;
        contributor_attestation: number;
        accepted_contribution_policy_version: string;
        contributor_attestation_accepted_at: number;
        data_use_agreement: number;
        accepted_data_use_policy_version: string;
        data_use_agreement_accepted_at: number;
      }>();
    expect(stored?.email_hash).not.toContain(email.toLowerCase());
    expect(stored?.email_ciphertext).not.toContain(email.toLowerCase());
    expect(stored?.status).toBe("pending");
    expect(stored?.plans_to_contribute).toBe(1);
    expect(stored?.contributor_attestation).toBe(1);
    expect(stored?.accepted_contribution_policy_version).toBe(
      "open-epi-4.0.0",
    );
    expect(stored?.contributor_attestation_accepted_at).toBeGreaterThan(0);
    expect(stored?.data_use_agreement).toBe(1);
    expect(stored?.accepted_data_use_policy_version).toBe(
      "archive-access-2.0.0",
    );
    expect(stored?.data_use_agreement_accepted_at).toBeGreaterThan(0);
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM archive_access_registrations
         WHERE email_hash = ?1`,
      )
        .bind(stored?.email_hash)
        .first<{ count: number }>(),
    ).toEqual({ count: 0 });

    const unauthenticatedList = await call(
      "GET",
      "/v1/admin/archive-access-requests",
    );
    expect(unauthenticatedList.status).toBe(401);
    const review = await call(
      "GET",
      "/v1/admin/archive-access-requests",
      undefined,
      ADMIN_TOKEN,
    );
    expect(review.status).toBe(200);
    expect(await review.json()).toMatchObject({
      requests: [
        {
          request_id: pending.request_id,
          status: "pending",
          contact_email: email.toLowerCase(),
          institution_name: "Example University",
          lab_name: "Example Neuroimaging Lab",
          plans_to_contribute: true,
          contributor_attestation: true,
          accepted_contribution_policy_version: "open-epi-4.0.0",
          contributor_attestation_accepted_at: expect.any(String),
          data_use_agreement: true,
          accepted_data_use_policy_version: "archive-access-2.0.0",
          data_use_agreement_accepted_at: expect.any(String),
        },
      ],
    });

    const approval = await call(
      "POST",
      `/v1/admin/archive-access-requests/${pending.request_id}/approve`,
      undefined,
      ADMIN_TOKEN,
    );
    expect(approval.status).toBe(200);
    const grant = await approval.json<{
      access_token: string;
      token_type: string;
      archive_url: string;
      accepted_data_use_policy_version: string;
      data_use_agreement_accepted_at: string;
    }>();
    expect(grant).toMatchObject({
      token_type: "Bearer",
      archive_url: "https://scalingneuro.org/v1/archive",
      accepted_data_use_policy_version: "archive-access-2.0.0",
      data_use_agreement_accepted_at: expect.any(String),
    });
    expect(grant.access_token).toMatch(/^sn_access_[A-Za-z0-9_-]{43}$/u);

    const registration = await env.DB.prepare(
      `SELECT token_hash, email_hash, email_ciphertext, plans_to_contribute,
              contributor_attestation, accepted_contribution_policy_version,
              contributor_attestation_accepted_at, data_use_agreement,
              accepted_data_use_policy_version,
              data_use_agreement_accepted_at
       FROM archive_access_registrations
       WHERE lab_name = ?1 LIMIT 1`,
    )
      .bind("Example Neuroimaging Lab")
      .first<{
        token_hash: string;
        email_hash: string;
        email_ciphertext: string;
        plans_to_contribute: number;
        contributor_attestation: number;
        accepted_contribution_policy_version: string;
        contributor_attestation_accepted_at: number;
        data_use_agreement: number;
        accepted_data_use_policy_version: string;
        data_use_agreement_accepted_at: number;
      }>();
    expect(registration?.token_hash).not.toContain(grant.access_token);
    expect(registration?.email_hash).not.toContain(email.toLowerCase());
    expect(registration?.email_ciphertext).not.toContain(email.toLowerCase());
    expect(registration?.plans_to_contribute).toBe(1);
    expect(registration?.contributor_attestation).toBe(1);
    expect(registration?.accepted_contribution_policy_version).toBe(
      "open-epi-4.0.0",
    );
    expect(registration?.contributor_attestation_accepted_at).toBe(
      stored?.contributor_attestation_accepted_at,
    );
    expect(registration?.data_use_agreement).toBe(1);
    expect(registration?.accepted_data_use_policy_version).toBe(
      "archive-access-2.0.0",
    );
    expect(registration?.data_use_agreement_accepted_at).toBe(
      stored?.data_use_agreement_accepted_at,
    );

    const archive = await call(
      "GET",
      "/v1/archive",
      undefined,
      grant.access_token,
    );
    expect(archive.status).toBe(200);
    expect(await archive.json()).toEqual({
      format: "dicom-tar-zstd",
      series: [],
    });

    const repeatedApproval = await call(
      "POST",
      `/v1/admin/archive-access-requests/${pending.request_id}/approve`,
      undefined,
      ADMIN_TOKEN,
    );
    expect(repeatedApproval.status).toBe(409);
  });

  it("requires an explicit contribution plan", async () => {
    const body = accessRequest(`missing-plan+${crypto.randomUUID()}@example.edu`);
    delete body.plans_to_contribute;
    const response = await call("POST", "/v1/archive-access", body);
    expect(response.status).toBe(400);
    expect(await response.json()).toMatchObject({
      error: { code: "INVALID_REQUEST" },
    });
  });

  it("records a requester who does not plan to contribute", async () => {
    const email = `noncontributor+${crypto.randomUUID()}@example.edu`;
    const response = await call(
      "POST",
      "/v1/archive-access",
      accessRequest(email, false),
    );
    expect(response.status).toBe(202);
    const pending = await response.json<{ request_id: string }>();
    const review = await call(
      "GET",
      "/v1/admin/archive-access-requests",
      undefined,
      ADMIN_TOKEN,
    );
    expect(review.status).toBe(200);
    const body = await review.json<{ requests: unknown[] }>();
    expect(body.requests).toContainEqual(
      expect.objectContaining({
        request_id: pending.request_id,
        plans_to_contribute: false,
        contributor_attestation: false,
        accepted_contribution_policy_version: null,
        contributor_attestation_accepted_at: null,
      }),
    );
  });

  it("requires the current contributor attestation for a yes answer", async () => {
    const missing = accessRequest(
      `contributor-attestation+${crypto.randomUUID()}@example.edu`,
    );
    missing.contributor_attestation = false;
    const missingResponse = await call(
      "POST",
      "/v1/archive-access",
      missing,
    );
    expect(missingResponse.status).toBe(400);

    const stale = accessRequest(
      `contributor-policy+${crypto.randomUUID()}@example.edu`,
    );
    stale.accepted_contribution_policy_version = "open-epi-2.0.0";
    const staleResponse = await call("POST", "/v1/archive-access", stale);
    expect(staleResponse.status).toBe(409);
    expect(await staleResponse.json()).toMatchObject({
      error: {
        code: "CONSENT_POLICY_UPDATE_REQUIRED",
        details: { consent_policy_version: "open-epi-4.0.0" },
      },
    });
  });

  it("requires the current archive access and privacy agreement", async () => {
    const declined = accessRequest(
      `data-use-declined+${crypto.randomUUID()}@example.edu`,
    );
    declined.data_use_agreement = false;
    const declinedResponse = await call(
      "POST",
      "/v1/archive-access",
      declined,
    );
    expect(declinedResponse.status).toBe(400);
    expect(await declinedResponse.json()).toMatchObject({
      error: { code: "INVALID_REQUEST" },
    });

    const stale = accessRequest(
      `data-use-stale+${crypto.randomUUID()}@example.edu`,
    );
    stale.accepted_data_use_policy_version = "archive-access-0.9.0";
    const staleResponse = await call("POST", "/v1/archive-access", stale);
    expect(staleResponse.status).toBe(409);
    expect(await staleResponse.json()).toMatchObject({
      error: {
        code: "ARCHIVE_ACCESS_POLICY_UPDATE_REQUIRED",
        details: { data_use_policy_version: "archive-access-2.0.0" },
      },
    });
  });

  it("refuses to approve a request whose policy acceptance is no longer current", async () => {
    const email = `approval-policy-drift+${crypto.randomUUID()}@example.edu`;
    const submitted = await call(
      "POST",
      "/v1/archive-access",
      accessRequest(email),
    );
    const pending = await submitted.json<{ request_id: string }>();
    await env.DB.prepare(
      `UPDATE archive_access_requests
       SET data_use_agreement_accepted_at = NULL
       WHERE id = ?1`,
    )
      .bind(pending.request_id)
      .run();

    const approval = await call(
      "POST",
      `/v1/admin/archive-access-requests/${pending.request_id}/approve`,
      undefined,
      ADMIN_TOKEN,
    );
    expect(approval.status).toBe(409);
    expect(await approval.json()).toMatchObject({
      error: {
        code: "ARCHIVE_ACCESS_POLICY_UPDATE_REQUIRED",
        details: { data_use_policy_version: "archive-access-2.0.0" },
      },
    });
  });

  it("blocks a grant whose accepted access agreement is no longer current", async () => {
    const email = `policy-drift+${crypto.randomUUID()}@example.edu`;
    const grant = await submitAndApprove(email);
    await env.DB.prepare(
      `UPDATE archive_access_registrations
       SET accepted_data_use_policy_version = 'archive-access-0.9.0'
       WHERE token_hash = ?1`,
    )
      .bind(await sha256Hex(grant.access_token))
      .run();

    const response = await call(
      "GET",
      "/v1/archive",
      undefined,
      grant.access_token,
    );
    expect(response.status).toBe(403);
    expect(await response.json()).toMatchObject({
      error: {
        code: "ARCHIVE_ACCESS_POLICY_UPDATE_REQUIRED",
        details: { data_use_policy_version: "archive-access-2.0.0" },
      },
    });
  });

  it("does not rotate an active token until a resubmission is approved", async () => {
    const email = `rotate+${crypto.randomUUID()}@example.edu`;
    const first = await submitAndApprove(email);
    const resubmitted = await call(
      "POST",
      "/v1/archive-access",
      accessRequest(email),
    );
    expect(resubmitted.status).toBe(202);
    const pending = await resubmitted.json<{ request_id: string }>();

    const stillActive = await call(
      "GET",
      "/v1/archive",
      undefined,
      first.access_token,
    );
    expect(stillActive.status).toBe(200);

    const secondResponse = await call(
      "POST",
      `/v1/admin/archive-access-requests/${pending.request_id}/approve`,
      undefined,
      ADMIN_TOKEN,
    );
    expect(secondResponse.status).toBe(200);
    const second = await secondResponse.json<{ access_token: string }>();
    expect(second.access_token).not.toBe(first.access_token);

    const oldAccess = await call(
      "GET",
      "/v1/archive",
      undefined,
      first.access_token,
    );
    expect(oldAccess.status).toBe(401);
    const newAccess = await call(
      "GET",
      "/v1/archive",
      undefined,
      second.access_token,
    );
    expect(newAccess.status).toBe(200);
  });

  it("rejects a pending request without issuing credentials", async () => {
    const email = `reject+${crypto.randomUUID()}@example.edu`;
    const submitted = await call(
      "POST",
      "/v1/archive-access",
      accessRequest(email),
    );
    const pending = await submitted.json<{ request_id: string }>();
    const rejection = await call(
      "POST",
      `/v1/admin/archive-access-requests/${pending.request_id}/reject`,
      undefined,
      ADMIN_TOKEN,
    );
    expect(rejection.status).toBe(200);
    expect(await rejection.json()).toMatchObject({ status: "rejected" });
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count
         FROM archive_access_registrations r
         JOIN archive_access_requests q ON q.email_hash = r.email_hash
         WHERE q.id = ?1`,
      )
        .bind(pending.request_id)
        .first<{ count: number }>(),
    ).toEqual({ count: 0 });
  });

  it("hides a staged archive and publishes it after its effective time", async () => {
    const email = `download+${crypto.randomUUID()}@example.edu`;
    const grant = await submitAndApprove(email);
    const siteId = crypto.randomUUID();
    const projectId = crypto.randomUUID();
    const deviceId = crypto.randomUUID();
    const uploadId = crypto.randomUUID();
    const seriesArchiveId = "7c2a5f77f3ab6c6d9e011234";
    const seriesId = "8d3b6e88f4bc7d7e0f122345";
    const timestamp = Math.floor(Date.now() / 1000);
    const key = `${seriesArchiveId}/series.dicom.tar.zst`;
    await env.DB.batch([
      env.DB.prepare(
        `INSERT INTO sites
           (id, slug, name, pseudonym_key_ciphertext,
            pseudonym_key_version, created_at)
         VALUES (?1, ?2, 'Download Test', 'encrypted', 1, ?3)`,
      ).bind(siteId, `site-${siteId}`, timestamp),
      env.DB.prepare(
        `INSERT INTO projects
           (id, site_id, slug, name, consent_policy_version, active, created_at)
         VALUES (?1, ?2, ?3, 'Download Test', 'open-epi-4.0.0', 1, ?4)`,
      ).bind(projectId, siteId, `project-${projectId}`, timestamp),
      env.DB.prepare(
        `INSERT INTO devices
           (id, site_id, project_id, token_hash, device_name, platform,
            client_version, accepted_consent_policy_version,
            created_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, 'Download Test', 'test', '0.6.2',
                 'open-epi-4.0.0', ?5, ?5)`,
      ).bind(deviceId, siteId, projectId, `token-${deviceId}`, timestamp),
      env.DB.prepare(
        `INSERT INTO uploads
           (id, site_id, project_id, device_id, status, archive_prefix,
            request_hash, client_version, consent_policy_version,
            data_license_id, publication_scheduled_at, series_count,
            total_bytes, created_at, updated_at, expires_at, received_at)
         VALUES (?1, ?2, ?3, ?4, 'committed', ?5, ?6, '0.6.2',
                 'open-epi-4.0.0', 'CC0-1.0', ?7, 1, 1024,
                 ?8, ?8, ?9, ?8)`,
      ).bind(
        uploadId,
        siteId,
        projectId,
        deviceId,
        `dicom/v1/${siteId}/${projectId}/${uploadId}/`,
        `request-${uploadId}`,
        timestamp + 7 * 24 * 60 * 60,
        timestamp,
        timestamp + 3600,
      ),
      env.DB.prepare(
        `INSERT INTO dicom_upload_series
           (upload_id, series_archive_id, series_id, subject_id, session_id,
            protocol_group_id, bundle_hash, dicom_count, archive_relative_key,
            expected_size, expected_sha256, completed_at, etag, series_kind,
            archive_route, pixel_data_policy)
         VALUES (?1, ?2, ?3, 'subject', 'session', 'protocol', ?4, 4, ?5,
                 1024, ?6, ?7, 'etag', 'functional_epi',
                 'functional-epi-v1', 'scanner-native-not-defaced')`,
      ).bind(
        uploadId,
        seriesArchiveId,
        seriesId,
        `bundle-${uploadId}`,
        key,
        "a".repeat(64),
        timestamp,
      ),
      env.DB.prepare(
        `INSERT INTO received_series_reservations
           (upload_id, bundle_id, site_id, project_id, series_id, bundle_hash,
            received_at, series_kind, archive_route,
            pixel_data_policy)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                 'functional_epi', 'functional-epi-v1',
                 'scanner-native-not-defaced')`,
      ).bind(
        uploadId,
        seriesArchiveId,
        siteId,
        projectId,
        seriesId,
        `bundle-${uploadId}`,
        timestamp,
      ),
    ]);

    const archive = await call(
      "GET",
      "/v1/archive",
      undefined,
      grant.access_token,
    );
    expect(archive.status).toBe(200);
    expect((await archive.json<{ series: unknown[] }>()).series).toHaveLength(0);

    await env.DB.prepare(
      `UPDATE uploads
       SET publication_scheduled_at = ?1
       WHERE id = ?2`,
    )
      .bind(timestamp - 1, uploadId)
      .run();

    const publishedArchive = await call(
      "GET",
      "/v1/archive",
      undefined,
      grant.access_token,
    );
    expect(publishedArchive.status).toBe(200);
    const listing = await publishedArchive.json<{
      series: Array<{
        download_url: string;
        sha256: string;
        data_license: { id: string };
      }>;
    }>();
    expect(listing.series).toHaveLength(1);
    expect(listing.series[0]?.sha256).toBe("a".repeat(64));
    expect(listing.series[0]?.data_license.id).toBe("CC0-1.0");

    const downloadPath = new URL(
      listing.series[0]?.download_url ?? "",
    ).pathname;
    const download = await call(
      "GET",
      downloadPath,
      undefined,
      grant.access_token,
    );
    expect(download.status).toBe(302);
    expect(download.headers.get("cache-control")).toBe("no-store");
    expect(download.headers.get("location")).toMatch(
      new RegExp(
        `^https://.+\\.r2\\.cloudflarestorage\\.com/[^/]+/.+/${key}` +
          "\\?.*X-Amz-Signature=",
        "u",
      ),
    );

    const lateCancellation = await call(
      "POST",
      `/v1/admin/dicom-uploads/${uploadId}/cancel`,
      undefined,
      ADMIN_TOKEN,
    );
    expect(lateCancellation.status).toBe(409);
  });
});
