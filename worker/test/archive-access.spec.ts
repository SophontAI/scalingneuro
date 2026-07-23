import { env } from "cloudflare:workers";
import {
  createExecutionContext,
  waitOnExecutionContext,
} from "cloudflare:test";
import { describe, expect, it } from "vitest";
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

function accessRequest(email: string): Record<string, unknown> {
  return {
    contact_name: "Example Researcher",
    contact_email: email,
    institution_name: "Example University",
    lab_name: "Example Neuroimaging Lab",
    participation_commitment: true,
  };
}

describe("shared EPI archive access", () => {
  it("grants archive access after the participation form", async () => {
    const email = `archive+${crypto.randomUUID()}@example.edu`;
    const response = await call(
      "POST",
      "/v1/archive-access",
      accessRequest(email),
    );
    expect(response.status).toBe(201);
    const grant = await response.json<{
      access_token: string;
      token_type: string;
      archive_url: string;
    }>();
    expect(grant).toMatchObject({
      token_type: "Bearer",
      archive_url: "https://scalingneuro.com/v1/archive",
    });
    expect(grant.access_token).toMatch(/^sn_access_[A-Za-z0-9_-]{43}$/u);

    const stored = await env.DB.prepare(
      `SELECT token_hash, email_hash, email_ciphertext
       FROM archive_access_registrations
       WHERE lab_name = ?1 LIMIT 1`,
    )
      .bind("Example Neuroimaging Lab")
      .first<{
        token_hash: string;
        email_hash: string;
        email_ciphertext: string;
      }>();
    expect(stored?.token_hash).not.toContain(grant.access_token);
    expect(stored?.email_hash).not.toContain(email.toLowerCase());
    expect(stored?.email_ciphertext).not.toContain(email.toLowerCase());

    const archive = await call("GET", "/v1/archive", undefined, grant.access_token);
    expect(archive.status).toBe(200);
    expect(await archive.json()).toEqual({
      format: "dicom-tar-zstd",
      series: [],
    });
  });

  it("requires an explicit lab participation commitment", async () => {
    const body = accessRequest(`declined+${crypto.randomUUID()}@example.edu`);
    body.participation_commitment = false;
    const response = await call("POST", "/v1/archive-access", body);
    expect(response.status).toBe(400);
    expect(await response.json()).toMatchObject({
      error: { code: "INVALID_REQUEST" },
    });
  });

  it("rotates the token when the same work email submits again", async () => {
    const email = `rotate+${crypto.randomUUID()}@example.edu`;
    const first = await (
      await call("POST", "/v1/archive-access", accessRequest(email))
    ).json<{ access_token: string }>();
    const second = await (
      await call("POST", "/v1/archive-access", accessRequest(email))
    ).json<{ access_token: string }>();
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

  it("lists and redirects to a signed download for a committed EPI archive", async () => {
    const email = `download+${crypto.randomUUID()}@example.edu`;
    const grant = await (
      await call("POST", "/v1/archive-access", accessRequest(email))
    ).json<{ access_token: string }>();
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
         VALUES (?1, ?2, ?3, 'Download Test', 'open-epi-2.0.0', 1, ?4)`,
      ).bind(projectId, siteId, `project-${projectId}`, timestamp),
      env.DB.prepare(
        `INSERT INTO devices
           (id, site_id, project_id, token_hash, device_name, platform,
            client_version, accepted_consent_policy_version,
            created_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, 'Download Test', 'test', '0.5.0',
                 'open-epi-2.0.0', ?5, ?5)`,
      ).bind(deviceId, siteId, projectId, `token-${deviceId}`, timestamp),
      env.DB.prepare(
        `INSERT INTO uploads
           (id, site_id, project_id, device_id, status, archive_prefix,
            request_hash, client_version, consent_policy_version, series_count,
            total_bytes, created_at, updated_at, expires_at, received_at)
         VALUES (?1, ?2, ?3, ?4, 'committed', ?5, ?6, '0.5.0',
                 'open-epi-2.0.0', 1, 1024, ?7, ?7, ?8, ?7)`,
      ).bind(
        uploadId,
        siteId,
        projectId,
        deviceId,
        `dicom/v1/${siteId}/${projectId}/${uploadId}/`,
        `request-${uploadId}`,
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
    const listing = await archive.json<{
      series: Array<{ download_url: string; sha256: string }>;
    }>();
    expect(listing.series).toHaveLength(1);
    expect(listing.series[0]?.sha256).toBe("a".repeat(64));

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
  });
});
