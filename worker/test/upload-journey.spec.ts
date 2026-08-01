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
  const context = createExecutionContext();
  const response = await fetchHandler(
    new Request(`https://scalingneuro.com${path}`, {
      method,
      headers,
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    }),
    env,
    context,
  );
  await waitOnExecutionContext(context);
  return response;
}

async function sha256(bytes: Uint8Array<ArrayBuffer>): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
}

describe("new PI EPI archive journey", () => {
  it("registers, stages one EPI archive, and permits cancellation before publication", async () => {
    const deviceToken =
      `sn_device_${crypto.randomUUID().replaceAll("-", "")}` +
      crypto.randomUUID().replaceAll("-", "").slice(0, 11);
    const registration = await call("POST", "/v1/register", {
      registration_id: crypto.randomUUID(),
      device_token: deviceToken,
      device_name: "scanner-transfer-workstation",
      client_version: "0.6.2",
      platform: "test",
      contact_email: `pi+${crypto.randomUUID()}@example.edu`,
      contact_name: "Example PI",
      institution_name: "Example University",
      lab_name: "Example Lab",
      contact_opt_in: false,
      accepted_consent_policy_version: "open-epi-4.0.0",
    });
    expect(registration.status).toBe(201);
    const registered = await registration.json<{
      site_id: string;
      project_id: string;
      device_id: string;
    }>();

    const migrationWindowUploadId = crypto.randomUUID();
    const migrationWindowTimestamp = Math.floor(Date.now() / 1000);
    // Simulate the previous Worker receiving an archive after migration 0029
    // is applied but before the new Worker deployment reaches production.
    await env.DB.prepare(
      `INSERT INTO uploads
         (id, site_id, project_id, device_id, status, archive_prefix,
          request_hash, client_version, consent_policy_version,
          data_license_id, series_count, total_bytes,
          created_at, updated_at, expires_at)
       VALUES (?1, ?2, ?3, ?4, 'uploading', ?5, ?6, '0.6.1',
               'open-epi-3.0.0', 'CC0-1.0', 1, 32, ?7, ?7, ?8)`,
    )
      .bind(
        migrationWindowUploadId,
        registered.site_id,
        registered.project_id,
        registered.device_id,
        `dicom/v1/${registered.site_id}/${registered.project_id}/${migrationWindowUploadId}/`,
        `migration-window-${migrationWindowUploadId}`,
        migrationWindowTimestamp,
        migrationWindowTimestamp + 3600,
      )
      .run();
    await env.DB.prepare(
      `UPDATE uploads
       SET status = 'committed', received_at = ?1,
           data_license_granted_at = ?1
       WHERE id = ?2`,
    )
      .bind(migrationWindowTimestamp, migrationWindowUploadId)
      .run();
    await env.DB.prepare(
      `INSERT INTO audit_events
         (id, event_type, upload_id, subject_type, subject_id,
          detail_code, created_at)
       VALUES (?1, 'upload.licensed', ?2, 'upload', ?2,
               'CC0-1.0', ?3)`,
    )
      .bind(
        crypto.randomUUID(),
        migrationWindowUploadId,
        migrationWindowTimestamp,
      )
      .run();
    expect(
      await env.DB.prepare(
        `SELECT data_license_granted_at, publication_scheduled_at
         FROM uploads WHERE id = ?1`,
      )
        .bind(migrationWindowUploadId)
        .first(),
    ).toEqual({
      data_license_granted_at: null,
      publication_scheduled_at:
        migrationWindowTimestamp + 7 * 24 * 60 * 60,
    });
    expect(
      await env.DB.prepare(
        `SELECT COUNT(*) AS count FROM audit_events
         WHERE upload_id = ?1 AND event_type = 'upload.licensed'`,
      )
        .bind(migrationWindowUploadId)
        .first(),
    ).toEqual({ count: 0 });

    const bytes = new Uint8Array(4096);
    crypto.getRandomValues(bytes);
    const archiveSha256 = await sha256(bytes);
    const seriesArchiveId = "7c2a5f77f3ab6c6d9e011234";
    const relativeKey = `${seriesArchiveId}/dicom.tar.zst`;
    const created = await call(
      "POST",
      "/v1/dicom-uploads",
      {
        format: "dicom-series-v1",
        client_version: "0.6.2",
        deidentification: {
          policy_id: "scaling-neuro.dicom-deidentification",
          policy_version: "2.0.0",
        },
        series: [
          {
            series_archive_id: seriesArchiveId,
            series_id: "3d5a987c014de62f9a011234",
            subject_id: "9c48102a77e3500f3a011234",
            session_id: "6a138b712e4d11af9a011234",
            protocol_group_id: "bb1c4ef23b97d8029a011234",
            dicom_count: 4,
            series_kind: "functional_epi",
            archive_route: "functional-epi-v1",
            pixel_data_policy: "scanner-native-not-defaced",
            archive: {
              format: "dicom-tar-zstd",
              relative_key: relativeKey,
              size: bytes.byteLength,
              sha256: archiveSha256,
            },
          },
        ],
      },
      deviceToken,
    );
    expect(created.status).toBe(201);
    const upload = await created.json<{
      upload_id: string;
      multipart_objects: Array<{
        key: string;
        upload_id: string;
        part_size: number;
      }>;
    }>();
    expect(upload.multipart_objects).toHaveLength(1);
    const object = upload.multipart_objects[0]!;
    expect(object.key).toContain(relativeKey);

    const partGrant = await call(
      "POST",
      `/v1/dicom-uploads/${upload.upload_id}/parts`,
      {
        key: object.key,
        part_number: 1,
        size: bytes.byteLength,
        sha256: archiveSha256,
      },
      deviceToken,
    );
    expect(partGrant.status).toBe(200);
    expect(await partGrant.json()).toMatchObject({
      headers: {
        "content-length": String(bytes.byteLength),
        "x-amz-content-sha256": archiveSha256,
      },
    });

    const multipart = env.ARCHIVE.resumeMultipartUpload(
      object.key,
      object.upload_id,
    );
    const part = await multipart.uploadPart(1, bytes);
    const completed = await call(
      "POST",
      `/v1/dicom-uploads/${upload.upload_id}/complete`,
      {
        objects: [
          {
            key: object.key,
            size: bytes.byteLength,
            sha256: archiveSha256,
            parts: [{ part_number: 1, etag: part.etag }],
          },
        ],
      },
      deviceToken,
    );
    expect(completed.status).toBe(200);
    const completedBody = await completed.json<Record<string, unknown>>();
    expect(completedBody).toMatchObject({
      upload_id: upload.upload_id,
      status: "committed",
      publication: {
        status: "staged",
        cancellation_email: "admin@sophont.med",
      },
      receipt: { received_series: 1, received_bytes: bytes.byteLength },
    });
    expect(completedBody).not.toHaveProperty("data_license");

    const storedLicense = await env.DB.prepare(
      `SELECT data_license_id, data_license_granted_at,
              publication_scheduled_at
       FROM uploads WHERE id = ?1`,
    )
      .bind(upload.upload_id)
      .first<{
        data_license_id: string;
        data_license_granted_at: number | null;
        publication_scheduled_at: number;
      }>();
    expect(storedLicense?.data_license_id).toBe("CC0-1.0");
    expect(storedLicense?.data_license_granted_at).toBeNull();
    expect(storedLicense!.publication_scheduled_at).toBeGreaterThan(
      Math.floor(Date.now() / 1000) + 6 * 24 * 60 * 60,
    );
    const storedObject = await env.ARCHIVE.head(object.key);
    expect(storedObject?.customMetadata?.data_license_id).toBeUndefined();

    const access = await call("POST", "/v1/archive-access", {
      contact_name: "Archive Researcher",
      contact_email: `archive+${crypto.randomUUID()}@example.edu`,
      institution_name: "Example University",
      lab_name: "Example Lab",
      plans_to_contribute: true,
      contributor_attestation: true,
      accepted_contribution_policy_version: "open-epi-4.0.0",
      data_use_agreement: true,
      accepted_data_use_policy_version: "archive-access-2.0.0",
    });
    expect(access.status).toBe(202);
    const { request_id: accessRequestId } = await access.json<{
      request_id: string;
    }>();
    const approval = await call(
      "POST",
      `/v1/admin/archive-access-requests/${accessRequestId}/approve`,
      undefined,
      "test-archive-access-admin-token-0000000000000000",
    );
    expect(approval.status).toBe(200);
    const { access_token: accessToken } = await approval.json<{
      access_token: string;
    }>();
    const archive = await call(
      "GET",
      "/v1/archive",
      undefined,
      accessToken,
    );
    expect(archive.status).toBe(200);
    const listing = await archive.json<{
      series: Array<{
        upload_id: string;
        sha256: string;
        data_license: { id: string; url: string; granted_at: string };
        download_url: string;
      }>;
    }>();
    expect(listing.series.find(
      (series) => series.upload_id === upload.upload_id,
    )).toBeUndefined();

    const download = await call(
      "GET",
      `/v1/archive/${upload.upload_id}/${seriesArchiveId}/download`,
      undefined,
      accessToken,
    );
    expect(download.status).toBe(404);

    const cancelled = await call(
      "POST",
      `/v1/admin/dicom-uploads/${upload.upload_id}/cancel`,
      undefined,
      "test-archive-access-admin-token-0000000000000000",
    );
    expect(cancelled.status).toBe(200);
    expect(await cancelled.json()).toMatchObject({
      upload_id: upload.upload_id,
      publication_status: "cancelled",
    });
    expect(await env.ARCHIVE.head(object.key)).toBeNull();

    const repeatedCancellation = await call(
      "POST",
      `/v1/admin/dicom-uploads/${upload.upload_id}/cancel`,
      undefined,
      "test-archive-access-admin-token-0000000000000000",
    );
    expect(repeatedCancellation.status).toBe(200);
    expect(await repeatedCancellation.json()).toMatchObject({
      upload_id: upload.upload_id,
      publication_status: "cancelled",
    });

    const status = await call(
      "GET",
      `/v1/dicom-uploads/${upload.upload_id}`,
      undefined,
      deviceToken,
    );
    expect(status.status).toBe(200);
    expect(await status.json()).toMatchObject({ status: "withdrawn" });
  });
});
