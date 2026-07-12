import { constantTimeEqual, sha256Hex } from "./crypto";
import { AppError } from "./errors";
import type { DeviceContext, Env } from "./env";

interface DeviceRow extends DeviceContext {
  revoked_at: number | null;
  project_active: number;
}

function bearerToken(request: Request): string {
  const authorization = request.headers.get("authorization");
  if (!authorization?.startsWith("Bearer ")) {
    throw new AppError(
      "UNAUTHORIZED",
      401,
      "Bearer authentication is required",
    );
  }
  const token = authorization.slice("Bearer ".length);
  if (token.length < 24 || token.length > 256 || /\s/u.test(token)) {
    throw new AppError("UNAUTHORIZED", 401, "Bearer authentication is invalid");
  }
  return token;
}

export async function authenticateDevice(
  request: Request,
  env: Env,
): Promise<DeviceContext> {
  const tokenHash = await sha256Hex(bearerToken(request));
  const row = await env.DB.prepare(
    `SELECT d.id,
            d.site_id,
            d.project_id,
            d.accepted_consent_policy_version,
            d.revoked_at,
            p.consent_policy_version AS current_consent_policy_version,
            p.name AS project_name,
            p.active AS project_active
     FROM devices d
     JOIN projects p ON p.id = d.project_id
     WHERE d.token_hash = ?1
     LIMIT 1`,
  )
    .bind(tokenHash)
    .first<DeviceRow>();

  if (!row) throw new AppError("UNAUTHORIZED", 401, "Device token is invalid");
  if (row.revoked_at !== null || row.project_active !== 1) {
    throw new AppError(
      "DEVICE_REVOKED",
      403,
      "Device or project access has been revoked",
    );
  }
  if (
    row.accepted_consent_policy_version !== row.current_consent_policy_version
  ) {
    throw new AppError(
      "CONSENT_POLICY_UPDATE_REQUIRED",
      409,
      "Project contribution policy has changed; obtain a new enrollment invite",
      { consent_policy_version: row.current_consent_policy_version },
    );
  }

  await env.DB.prepare("UPDATE devices SET last_seen_at = ?1 WHERE id = ?2")
    .bind(Math.floor(Date.now() / 1000), row.id)
    .run();

  return {
    id: row.id,
    site_id: row.site_id,
    project_id: row.project_id,
    accepted_consent_policy_version: row.accepted_consent_policy_version,
    current_consent_policy_version: row.current_consent_policy_version,
    project_name: row.project_name,
  };
}

export async function authenticateAdmin(
  request: Request,
  env: Env,
): Promise<void> {
  const token = bearerToken(request);
  if (
    !env.ADMIN_API_TOKEN ||
    !(await constantTimeEqual(token, env.ADMIN_API_TOKEN))
  ) {
    throw new AppError("UNAUTHORIZED", 401, "Admin token is invalid");
  }
}
