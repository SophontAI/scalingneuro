import { env } from "cloudflare:workers";
import {
  createExecutionContext,
  waitOnExecutionContext,
} from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { fetchHandler } from "../src/index";

const ADMIN_TOKEN = "test-admin-token-with-sufficient-entropy";

async function call(
  method: string,
  path: string,
  body?: unknown,
  token?: string,
  clientIp?: string,
): Promise<Response> {
  const headers = new Headers();
  if (body !== undefined) headers.set("content-type", "application/json");
  if (token) headers.set("authorization", `Bearer ${token}`);
  if (clientIp) headers.set("cf-connecting-ip", clientIp);
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
  const entropy =
    crypto.randomUUID().replaceAll("-", "") +
    crypto.randomUUID().replaceAll("-", "").slice(0, 11);
  return `sn_device_${entropy}`;
}

function registration(): Record<string, unknown> {
  const suffix = crypto.randomUUID().replaceAll("-", "");
  return {
    registration_id: crypto.randomUUID(),
    device_token: deviceToken(),
    device_name: "scanner-transfer-workstation",
    client_version: "0.2.0",
    platform: "linux-x64",
    contact_email: `Researcher+${suffix}@Example.edu`,
    contact_name: "Example Researcher",
    institution_name: "Example University",
    institution_ror_id: "https://ror.org/03yrm5c26",
    lab_name: "Example Neuroimaging Lab",
    contact_opt_in: true,
    accepted_consent_policy_version: "open-epi-1.0.0",
  };
}

describe("open contributor registration", () => {
  it("registers without an invite, encrypts contact data, and replays safely", async () => {
    const info = await call("GET", "/v1/contribution");
    expect(info.status).toBe(200);
    expect(await info.json()).toEqual({
      registration_open: true,
      project_name: "Scaling Neuro public EPI contribution",
      consent_policy_version: "open-epi-1.0.0",
      policy_url: "https://scalingneuro.com/docs/contribution-policy",
      self_service_quota_bytes: 268_435_456_000,
      minimum_client_version: "0.2.0",
    });

    const request = registration();
    const normalizedEmail = (request.contact_email as string).toLowerCase();
    const created = await call("POST", "/v1/register", request);
    expect(created.status).toBe(201);
    const enrollment = await created.json<Record<string, unknown>>();
    expect(enrollment).toMatchObject({
      enrollment_id: request.registration_id,
      device_token: request.device_token,
      project_name: "Scaling Neuro public EPI contribution",
      consent_policy_version: "open-epi-1.0.0",
    });
    expect(enrollment.pseudonym_key_b64).toMatch(/^[A-Za-z0-9+/]{43}=$/u);

    const stored = await env.DB.prepare(
      `SELECT r.email_hash, r.email_ciphertext, d.invite_id,
              p.upload_quota_bytes
       FROM contributor_registrations r
       JOIN devices d ON d.id = r.device_id
       JOIN projects p ON p.id = r.project_id
       WHERE r.id = ?1`,
    )
      .bind(request.registration_id)
      .first<{
        email_hash: string;
        email_ciphertext: string;
        invite_id: string | null;
        upload_quota_bytes: number;
      }>();
    expect(stored?.email_hash).toMatch(/^[a-f0-9]{64}$/u);
    expect(stored?.email_ciphertext).not.toContain(normalizedEmail);
    expect(stored?.invite_id).toBeNull();
    expect(stored?.upload_quota_bytes).toBe(268_435_456_000);

    const replay = await call("POST", "/v1/register", request);
    expect(replay.status).toBe(201);
    expect(await replay.json()).toEqual(enrollment);
    const upgradedReplay = await call("POST", "/v1/register", {
      ...request,
      client_version: "0.2.1",
      platform: "macos-aarch64",
    });
    expect(upgradedReplay.status).toBe(201);
    expect(await upgradedReplay.json()).toEqual(enrollment);
    const count = await env.DB.prepare(
      "SELECT COUNT(*) AS count FROM contributor_registrations WHERE id = ?1",
    )
      .bind(request.registration_id)
      .first<{ count: number }>();
    expect(count?.count).toBe(1);

    const registrations = await call(
      "GET",
      "/v1/admin/registrations",
      undefined,
      ADMIN_TOKEN,
    );
    expect(registrations.status).toBe(200);
    expect(await registrations.json()).toMatchObject({
      registrations: [
        {
          registration_id: request.registration_id,
          contact_email: normalizedEmail,
          contact_name: "Example Researcher",
          institution_name: "Example University",
          institution_ror_id: "https://ror.org/03yrm5c26",
          lab_name: "Example Neuroimaging Lab",
          contact_opt_in: true,
          platform: "macos-aarch64",
          client_version: "0.2.1",
          committed_uploads: 0,
          committed_series: 0,
          committed_bytes: 0,
        },
      ],
    });
  });

  it("rejects stale clients, policy drift, changed replays, and over-quota uploads", async () => {
    const stale = registration();
    stale.client_version = "0.1.1";
    const staleResponse = await call("POST", "/v1/register", stale);
    expect(staleResponse.status).toBe(426);
    expect(await staleResponse.json()).toMatchObject({
      error: {
        code: "CLIENT_UPDATE_REQUIRED",
        details: { minimum_client_version: "0.2.0" },
      },
    });

    const policy = registration();
    policy.accepted_consent_policy_version = "old-policy";
    const policyResponse = await call("POST", "/v1/register", policy);
    expect(policyResponse.status).toBe(409);
    expect(await policyResponse.json()).toMatchObject({
      error: {
        code: "CONSENT_POLICY_UPDATE_REQUIRED",
        details: { consent_policy_version: "open-epi-1.0.0" },
      },
    });

    const request = registration();
    const created = await call("POST", "/v1/register", request);
    const enrollment = await created.json<Record<string, unknown>>();
    const changed = { ...request, lab_name: "Different Lab" };
    const conflict = await call("POST", "/v1/register", changed);
    expect(conflict.status).toBe(409);
    expect(await conflict.json()).toMatchObject({ error: { code: "CONFLICT" } });

    await env.DB.prepare(
      "UPDATE projects SET upload_quota_bytes = 33 WHERE id = ?1",
    )
      .bind(enrollment.project_id)
      .run();
    const id = (value: string) => value.repeat(24);
    const upload = await call(
      "POST",
      "/v1/uploads",
      {
        client_version: "0.2.0",
        bundles: [
          {
            bundle_id: id("1"),
            series_id: id("2"),
            subject_id: id("3"),
            session_id: id("4"),
            protocol_group_id: id("5"),
            nii: {
              relative_key: `${id("1")}/scan_bold.nii.gz`,
              size: 32,
              sha256: "a".repeat(64),
              uncompressed_sha256: "b".repeat(64),
            },
            metadata: {
              relative_key: `${id("1")}/scan_bold.json`,
              size: 2,
              sha256: "c".repeat(64),
            },
          },
        ],
      },
      enrollment.device_token as string,
    );
    expect(upload.status).toBe(413);
    expect(await upload.json()).toMatchObject({
      error: {
        code: "QUOTA_EXCEEDED",
        details: { quota_bytes: 33, used_bytes: 0, requested_bytes: 34 },
      },
    });
  });

  it("bounds unauthenticated registration creation by Cloudflare network identity", async () => {
    for (let attempt = 0; attempt < 5; attempt += 1) {
      const response = await call(
        "POST",
        "/v1/register",
        registration(),
        undefined,
        "192.0.2.10",
      );
      expect(response.status).toBe(201);
    }
    const limited = await call(
      "POST",
      "/v1/register",
      registration(),
      undefined,
      "192.0.2.10",
    );
    expect(limited.status).toBe(429);
    expect(await limited.json()).toMatchObject({
      error: { code: "RATE_LIMITED" },
    });
  });
});
