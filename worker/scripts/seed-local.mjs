#!/usr/bin/env node

const endpoint = (process.env.SCALING_NEURO_API_URL ?? "http://127.0.0.1:8787").replace(/\/$/u, "");
const adminToken = process.env.ADMIN_API_TOKEN;

if (!adminToken) {
  process.stderr.write("ADMIN_API_TOKEN is required and must match worker/.dev.vars\n");
  process.exit(1);
}

const response = await fetch(`${endpoint}/v1/admin/invites`, {
  method: "POST",
  headers: {
    authorization: `Bearer ${adminToken}`,
    "content-type": "application/json",
  },
  body: JSON.stringify({
    site_slug: process.env.SEED_SITE_SLUG ?? "local-lab",
    site_name: process.env.SEED_SITE_NAME ?? "Local Development Lab",
    project_slug: process.env.SEED_PROJECT_SLUG ?? "epi-pilot",
    project_name: process.env.SEED_PROJECT_NAME ?? "EPI Pilot",
    consent_policy_version: process.env.SEED_CONSENT_POLICY_VERSION ?? "pilot-1",
    expires_in_seconds: 604800,
    max_uses: 1,
  }),
});

const text = await response.text();
if (!response.ok) {
  process.stderr.write(`Seed failed (${response.status}): ${text.trim()}\n`);
  process.exit(1);
}
process.stdout.write(text.endsWith("\n") ? text : `${text}\n`);
