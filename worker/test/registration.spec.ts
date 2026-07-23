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

function deviceToken(): string {
  const entropy =
    crypto.randomUUID().replaceAll("-", "") +
    crypto.randomUUID().replaceAll("-", "").slice(0, 11);
  return `sn_device_${entropy}`;
}

function registration(
  clientVersion = "0.5.0",
  policyVersion = "open-epi-2.0.0",
): Record<string, unknown> {
  return {
    registration_id: crypto.randomUUID(),
    device_token: deviceToken(),
    device_name: "scanner-transfer-workstation",
    client_version: clientVersion,
    platform: "linux-x64",
    contact_email: `researcher+${crypto.randomUUID()}@example.edu`,
    contact_name: "Example Researcher",
    institution_name: "Example University",
    institution_ror_id: "https://ror.org/03yrm5c26",
    lab_name: "Example Neuroimaging Lab",
    contact_opt_in: true,
    accepted_consent_policy_version: policyVersion,
  };
}

describe("functional EPI contributor registration", () => {
  it("publishes the minimal contribution contract", async () => {
    const response = await call("GET", "/v1/contribution");
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      registration_open: true,
      project_name: "Scaling Neuro shared EPI archive",
      consent_policy_version: "open-epi-2.0.0",
      policy_url: "https://scalingneuro.com/docs/contribution-policy",
      self_service_quota_bytes: null,
      minimum_client_version: "0.5.0",
    });
  });

  it("registers a workstation, encrypts the email, and replays safely", async () => {
    const input = registration();
    const created = await call("POST", "/v1/register", input);
    expect(created.status).toBe(201);
    const registrationResponse =
      await created.json<Record<string, unknown>>();
    expect(registrationResponse).toMatchObject({
      registration_id: input.registration_id,
      device_token: input.device_token,
      project_name: "Scaling Neuro shared EPI archive",
      consent_policy_version: "open-epi-2.0.0",
    });

    const stored = await env.DB.prepare(
      `SELECT r.email_ciphertext, p.upload_quota_bytes
       FROM contributor_registrations r
       JOIN projects p ON p.id = r.project_id
       WHERE r.id = ?1`,
    )
      .bind(input.registration_id)
      .first<{
        email_ciphertext: string;
        upload_quota_bytes: number | null;
      }>();
    expect(stored?.email_ciphertext).not.toContain(
      String(input.contact_email).toLowerCase(),
    );
    expect(stored?.upload_quota_bytes).toBeNull();

    const replay = await call("POST", "/v1/register", input);
    expect(replay.status).toBe(201);
    expect(await replay.json()).toEqual(registrationResponse);
  });

  it("rejects old clients and stale policy acceptance", async () => {
    const oldClient = await call(
      "POST",
      "/v1/register",
      registration("0.4.9"),
    );
    expect(oldClient.status).toBe(426);
    expect(await oldClient.json()).toMatchObject({
      error: {
        code: "CLIENT_UPDATE_REQUIRED",
        details: { minimum_client_version: "0.5.0" },
      },
    });

    const stalePolicy = await call(
      "POST",
      "/v1/register",
      registration("0.5.0", "open-epi-1.0.0"),
    );
    expect(stalePolicy.status).toBe(409);
    expect(await stalePolicy.json()).toMatchObject({
      error: {
        code: "CONSENT_POLICY_UPDATE_REQUIRED",
        details: { consent_policy_version: "open-epi-2.0.0" },
      },
    });
  });
});
