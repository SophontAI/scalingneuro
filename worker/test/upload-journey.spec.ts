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
  it("registers, stores one EPI archive, lists it, and grants a download", async () => {
    const deviceToken =
      `sn_device_${crypto.randomUUID().replaceAll("-", "")}` +
      crypto.randomUUID().replaceAll("-", "").slice(0, 11);
    const registration = await call("POST", "/v1/register", {
      registration_id: crypto.randomUUID(),
      device_token: deviceToken,
      device_name: "scanner-transfer-workstation",
      client_version: "0.5.0",
      platform: "test",
      contact_email: `pi+${crypto.randomUUID()}@example.edu`,
      contact_name: "Example PI",
      institution_name: "Example University",
      lab_name: "Example Lab",
      contact_opt_in: false,
      accepted_consent_policy_version: "open-epi-2.0.0",
    });
    expect(registration.status).toBe(201);

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
        client_version: "0.5.0",
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
    expect(await completed.json()).toMatchObject({
      upload_id: upload.upload_id,
      status: "committed",
      receipt: { received_series: 1, received_bytes: bytes.byteLength },
    });

    const access = await call("POST", "/v1/archive-access", {
      contact_name: "Archive Researcher",
      contact_email: `archive+${crypto.randomUUID()}@example.edu`,
      institution_name: "Example University",
      lab_name: "Example Lab",
      participation_commitment: true,
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
        download_url: string;
      }>;
    }>();
    const listed = listing.series.find(
      (series) => series.upload_id === upload.upload_id,
    );
    expect(listed).toMatchObject({ sha256: archiveSha256 });

    const download = await call(
      "GET",
      new URL(listed!.download_url).pathname,
      undefined,
      accessToken,
    );
    expect(download.status).toBe(302);
    expect(download.headers.get("location")).toContain(
      "X-Amz-Signature=",
    );
  });
});
