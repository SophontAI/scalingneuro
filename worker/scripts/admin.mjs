#!/usr/bin/env node

const endpoint = (process.env.SCALING_NEURO_API_URL ?? "http://127.0.0.1:8787").replace(/\/$/u, "");
const adminToken = process.env.ADMIN_API_TOKEN;

function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

function options(values) {
  const parsed = new Map();
  for (let index = 0; index < values.length; index += 2) {
    const key = values[index];
    const value = values[index + 1];
    if (!key?.startsWith("--") || value === undefined || value.startsWith("--")) {
      fail(`Invalid option near ${key ?? "end of command"}`);
    }
    parsed.set(key.slice(2), value);
  }
  return parsed;
}

function required(values, name) {
  const value = values.get(name);
  if (!value) fail(`Missing --${name}`);
  return value;
}

async function post(path, body) {
  if (!adminToken) fail("ADMIN_API_TOKEN is required");
  let response;
  try {
    response = await fetch(`${endpoint}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${adminToken}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(body ?? {}),
    });
  } catch (error) {
    fail(`Unable to reach ${endpoint}: ${error instanceof Error ? error.message : "network error"}`);
  }
  const text = await response.text();
  if (!response.ok) fail(`Request failed (${response.status}): ${text.trim()}`);
  process.stdout.write(text.endsWith("\n") ? text : `${text}\n`);
}

const [command, ...args] = process.argv.slice(2);
const values = options(args);

switch (command) {
  case "invite": {
    const body = {
      site_slug: required(values, "site-slug"),
      site_name: required(values, "site-name"),
      project_slug: required(values, "project-slug"),
      project_name: required(values, "project-name"),
      consent_policy_version: required(values, "consent-policy-version"),
    };
    if (values.has("expires-seconds")) body.expires_in_seconds = Number(values.get("expires-seconds"));
    if (values.has("max-uses")) body.max_uses = Number(values.get("max-uses"));
    await post("/v1/admin/invites", body);
    break;
  }
  case "revoke-invite":
    await post(`/v1/admin/invites/${required(values, "id")}/revoke`);
    break;
  case "revoke-device":
    await post(`/v1/admin/devices/${required(values, "id")}/revoke`);
    break;
  case "withdraw-upload":
    await post(`/v1/admin/uploads/${required(values, "id")}/withdraw`);
    break;
  default:
    fail(
      "Usage: admin.mjs invite --site-slug SLUG --site-name NAME --project-slug SLUG " +
        "--project-name NAME --consent-policy-version VERSION [--expires-seconds N] [--max-uses N]\n" +
        "       admin.mjs revoke-invite|revoke-device|withdraw-upload --id UUID",
    );
}
