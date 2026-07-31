import {
  canonicalJson,
  decryptSiteKey,
  encryptRegistrationEmail,
  encryptSiteKey,
  pseudonymKeyBase64,
  randomBytes,
  sha256Hex,
} from "./crypto";
import {
  authenticateDeviceForPolicyAcceptance,
} from "./auth";
import { AppError } from "./errors";
import type { Env } from "./env";
import type {
  PublicPolicyAcceptanceRequest,
  PublicRegistrationRequest,
} from "./validation";
import packageManifest from "../package.json";

const MINIMUM_CLIENT_VERSION = "0.6.1";
export const MINIMUM_EPI_CLIENT_VERSION = "0.6.1";
const PUBLIC_PROJECT_NAME = "Scaling Neuro shared EPI archive";
const PUBLIC_PROJECT_SLUG = "shared-epi";
export const PUBLIC_CONSENT_POLICY_VERSION = "open-epi-3.0.0";
export const DATA_LICENSE_ID = "CC0-1.0";
export const DATA_LICENSE_URL =
  "https://creativecommons.org/publicdomain/zero/1.0/";

interface RegistrationRow {
  registration_id: string;
  request_hash: string;
  device_id: string;
  device_token_hash: string;
  revoked_at: number | null;
  site_id: string;
  project_id: string;
  project_name: string;
  consent_policy_version: string;
  pseudonym_key_ciphertext: string;
}

function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

function semanticVersion(
  value: string,
): { core: readonly [number, number, number]; prerelease: boolean } | null {
  const match =
    /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([A-Za-z0-9.-]+))?(?:\+[A-Za-z0-9.-]+)?$/u.exec(
      value,
    );
  if (!match) return null;
  const core: [number, number, number] = [
    Number(match[1]),
    Number(match[2]),
    Number(match[3]),
  ];
  if (core.some((part) => !Number.isSafeInteger(part))) return null;
  return {
    core,
    prerelease: match[4] !== undefined,
  };
}

export function clientVersionAtLeast(
  value: string,
  minimumValue: string,
): boolean {
  const current = semanticVersion(value);
  const minimum = semanticVersion(minimumValue);
  if (!current || !minimum) return false;
  for (let index = 0; index < current.core.length; index += 1) {
    const left = current.core[index]!;
    const right = minimum.core[index]!;
    if (left !== right) return left > right;
  }
  return !current.prerelease || minimum.prerelease;
}

function requireCurrentClient(value: string): void {
  if (!clientVersionAtLeast(value, MINIMUM_CLIENT_VERSION)) {
    throw new AppError(
      "CLIENT_UPDATE_REQUIRED",
      426,
      "Install the current neuro-sync release",
      { minimum_client_version: MINIMUM_CLIENT_VERSION },
    );
  }
}

function neuroSyncClientVersion(userAgent: string | null): string | null {
  const match = /^neuro-sync\/([^\s/]+)$/u.exec(userAgent ?? "");
  return match && semanticVersion(match[1]!) ? match[1]! : null;
}

export function publicContributionInfo(
  _userAgent: string | null,
): Record<string, unknown> {
  return {
    registration_open: true,
    project_name: PUBLIC_PROJECT_NAME,
    consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION,
    policy_url: "https://scalingneuro.com/docs/contribution-policy",
    data_license_id: DATA_LICENSE_ID,
    data_license_url: DATA_LICENSE_URL,
    self_service_quota_bytes: null,
    minimum_client_version: MINIMUM_CLIENT_VERSION,
  };
}

function registrationRequestHash(
  input: PublicRegistrationRequest,
): Promise<string> {
  return sha256Hex(
    canonicalJson({
      registration_id: input.registration_id,
      device_name: input.device_name,
      client_version: input.client_version,
      platform: input.platform,
      contact_email: input.contact_email,
      contact_name: input.contact_name,
      institution_name: input.institution_name,
      institution_ror_id: input.institution_ror_id ?? null,
      lab_name: input.lab_name,
      contact_opt_in: input.contact_opt_in,
      accepted_consent_policy_version:
        input.accepted_consent_policy_version,
    }),
  );
}

async function registrationResponse(
  env: Env,
  row: RegistrationRow,
  deviceToken: string,
): Promise<Record<string, unknown>> {
  const siteKey = await decryptSiteKey(
    row.pseudonym_key_ciphertext,
    row.site_id,
    env.SITE_KEY_ENCRYPTION_KEY_B64,
  );
  return {
    registration_id: row.registration_id,
    device_token: deviceToken,
    device_id: row.device_id,
    site_id: row.site_id,
    project_id: row.project_id,
    project_name: row.project_name,
    consent_policy_version: row.consent_policy_version,
    pseudonym_key_b64: pseudonymKeyBase64(siteKey),
  };
}

export async function registerContributor(
  env: Env,
  input: PublicRegistrationRequest,
): Promise<Record<string, unknown>> {
  requireCurrentClient(input.client_version);
  if (
    input.accepted_consent_policy_version !==
    PUBLIC_CONSENT_POLICY_VERSION
  ) {
    throw new AppError(
      "CONSENT_POLICY_UPDATE_REQUIRED",
      409,
      "Review and accept the current functional EPI contribution policy",
      { consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION },
    );
  }

  const [requestHash, deviceTokenHash] = await Promise.all([
    registrationRequestHash(input),
    sha256Hex(input.device_token),
  ]);
  const existing = await env.DB.prepare(
    `SELECT r.id AS registration_id, r.request_hash,
            d.id AS device_id, d.token_hash AS device_token_hash,
            d.revoked_at, d.site_id, d.project_id,
            p.name AS project_name,
            d.accepted_consent_policy_version AS consent_policy_version,
            s.pseudonym_key_ciphertext
     FROM contributor_registrations r
     JOIN devices d ON d.id = r.device_id
     JOIN projects p ON p.id = r.project_id
     JOIN sites s ON s.id = r.site_id
     WHERE r.id = ?1 LIMIT 1`,
  )
    .bind(input.registration_id)
    .first<RegistrationRow>();
  if (existing) {
    if (
      existing.revoked_at !== null ||
      existing.request_hash !== requestHash ||
      existing.device_token_hash !== deviceTokenHash
    ) {
      throw new AppError(
        "CONFLICT",
        409,
        "Registration operation conflicts with an existing workstation",
      );
    }
    await env.DB.prepare(
      `UPDATE devices SET last_seen_at = ?1, client_version = ?2
       WHERE id = ?3`,
    )
      .bind(nowSeconds(), input.client_version, existing.device_id)
      .run();
    return registrationResponse(env, existing, input.device_token);
  }

  const timestamp = nowSeconds();
  const siteId = crypto.randomUUID();
  const projectId = crypto.randomUUID();
  const deviceId = crypto.randomUUID();
  const siteKey = randomBytes(32);
  const [siteKeyCiphertext, emailCiphertext, emailHash] = await Promise.all([
    encryptSiteKey(siteKey, siteId, env.SITE_KEY_ENCRYPTION_KEY_B64),
    encryptRegistrationEmail(
      input.contact_email,
      input.registration_id,
      env.SITE_KEY_ENCRYPTION_KEY_B64,
    ),
    sha256Hex(input.contact_email),
  ]);
  const siteName = `${input.institution_name} / ${input.lab_name}`;
  try {
    await env.DB.batch([
      env.DB.prepare(
        `INSERT INTO sites
           (id, slug, name, pseudonym_key_ciphertext,
            pseudonym_key_version, created_at)
         VALUES (?1, ?2, ?3, ?4, 1, ?5)`,
      ).bind(
        siteId,
        `epi-${siteId}`,
        siteName,
        siteKeyCiphertext,
        timestamp,
      ),
      env.DB.prepare(
        `INSERT INTO projects
           (id, site_id, slug, name, consent_policy_version,
            active, created_at, upload_quota_bytes)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, NULL)`,
      ).bind(
        projectId,
        siteId,
        PUBLIC_PROJECT_SLUG,
        PUBLIC_PROJECT_NAME,
        PUBLIC_CONSENT_POLICY_VERSION,
        timestamp,
      ),
      env.DB.prepare(
        `INSERT INTO devices
           (id, site_id, project_id, token_hash, device_name,
            platform, client_version, accepted_consent_policy_version,
            created_at, last_seen_at, revoked_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, NULL)`,
      ).bind(
        deviceId,
        siteId,
        projectId,
        deviceTokenHash,
        input.device_name,
        input.platform,
        input.client_version,
        PUBLIC_CONSENT_POLICY_VERSION,
        timestamp,
      ),
      env.DB.prepare(
        `INSERT INTO contributor_registrations
           (id, site_id, project_id, device_id, request_hash, email_hash,
            email_ciphertext, contact_name, institution_name,
            institution_ror_id, lab_name, contact_opt_in, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)`,
      ).bind(
        input.registration_id,
        siteId,
        projectId,
        deviceId,
        requestHash,
        emailHash,
        emailCiphertext,
        input.contact_name,
        input.institution_name,
        input.institution_ror_id ?? null,
        input.lab_name,
        input.contact_opt_in ? 1 : 0,
        timestamp,
      ),
      env.DB.prepare(
        `INSERT INTO audit_events
           (id, event_type, site_id, project_id, device_id,
            subject_type, subject_id, detail_code, created_at)
         VALUES (?1, 'device.registered', ?2, ?3, ?4,
                 'device', ?4, ?5, ?6)`,
      ).bind(
        crypto.randomUUID(),
        siteId,
        projectId,
        deviceId,
        PUBLIC_CONSENT_POLICY_VERSION,
        timestamp,
      ),
    ]);
  } catch {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Unable to register this workstation; retry the same command",
    );
  }

  return {
    registration_id: input.registration_id,
    device_token: input.device_token,
    device_id: deviceId,
    site_id: siteId,
    project_id: projectId,
    project_name: PUBLIC_PROJECT_NAME,
    consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION,
    pseudonym_key_b64: pseudonymKeyBase64(siteKey),
  };
}

export async function acceptPublicContributionPolicy(
  request: Request,
  env: Env,
  input: PublicPolicyAcceptanceRequest,
): Promise<Record<string, unknown>> {
  const device = await authenticateDeviceForPolicyAcceptance(request, env);
  const clientVersion = neuroSyncClientVersion(
    request.headers.get("user-agent"),
  );
  if (!clientVersion) {
    throw new AppError(
      "CLIENT_UPDATE_REQUIRED",
      426,
      "Use the current neuro-sync client to accept this policy",
      { minimum_client_version: MINIMUM_CLIENT_VERSION },
    );
  }
  requireCurrentClient(clientVersion);
  if (
    input.accepted_consent_policy_version !==
    PUBLIC_CONSENT_POLICY_VERSION
  ) {
    throw new AppError(
      "CONSENT_POLICY_UPDATE_REQUIRED",
      409,
      "Review and accept the current functional EPI contribution policy",
      { consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION },
    );
  }
  const timestamp = nowSeconds();
  const results = await env.DB.batch([
    env.DB.prepare(
      `UPDATE projects SET slug = ?1, name = ?2,
                           consent_policy_version = ?3
       WHERE id = ?4 AND site_id = ?5`,
    ).bind(
      PUBLIC_PROJECT_SLUG,
      PUBLIC_PROJECT_NAME,
      PUBLIC_CONSENT_POLICY_VERSION,
      device.project_id,
      device.site_id,
    ),
    env.DB.prepare(
      `UPDATE devices SET accepted_consent_policy_version = ?1,
                          client_version = ?2, last_seen_at = ?3
       WHERE id = ?4`,
    ).bind(
      PUBLIC_CONSENT_POLICY_VERSION,
      clientVersion,
      timestamp,
      device.id,
    ),
    env.DB.prepare(
      `INSERT INTO audit_events
         (id, event_type, site_id, project_id, device_id,
          subject_type, subject_id, detail_code, created_at)
       VALUES (?1, 'device.policy_accepted', ?2, ?3, ?4,
               'device', ?4, ?5, ?6)`,
    ).bind(
      crypto.randomUUID(),
      device.site_id,
      device.project_id,
      device.id,
      PUBLIC_CONSENT_POLICY_VERSION,
      timestamp,
    ),
  ]);
  if (
    (results[0]?.meta.changes ?? 0) !== 1 ||
    (results[1]?.meta.changes ?? 0) !== 1
  ) {
    throw new AppError(
      "CONFLICT",
      409,
      "Policy acceptance could not be persisted",
    );
  }
  return {
    status: "accepted",
    device_id: device.id,
    site_id: device.site_id,
    project_id: device.project_id,
    project_name: PUBLIC_PROJECT_NAME,
    consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION,
  };
}

export async function health(env: Env): Promise<Record<string, unknown>> {
  try {
    const result = await env.DB.prepare("SELECT 1 AS ok").first<{
      ok: number;
    }>();
    if (result?.ok !== 1 || !env.ARCHIVE) throw new Error("binding unavailable");
  } catch {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      503,
      "Archive storage is unavailable",
    );
  }
  return {
    status: "ok",
    service: "scaling-neuro-sync",
    version: packageManifest.version,
  };
}
