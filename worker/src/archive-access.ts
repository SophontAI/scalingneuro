import {
  decryptArchiveAccessRequestEmail,
  encryptArchiveAccessEmail,
  encryptArchiveAccessRequestEmail,
  randomAccessToken,
  sha256Hex,
  utf8Bytes,
} from "./crypto";
import { AppError } from "./errors";
import type { Env } from "./env";
import { presignGetObject } from "./r2";
import {
  DATA_LICENSE_ID,
  DATA_LICENSE_URL,
  PUBLIC_CONSENT_POLICY_VERSION,
} from "./service";

const EMAIL =
  /^[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9](?:[A-Za-z0-9.-]{0,251}[A-Za-z0-9])?$/u;
const SERIES_ARCHIVE_ID = /^[a-f0-9]{24}$/u;
const MAX_ARCHIVE_ROWS = 200;
const MAX_REVIEW_ROWS = 100;
export const ARCHIVE_ACCESS_POLICY_VERSION = "archive-access-2.0.0";

export interface ArchiveAccessRequest {
  contact_name: string;
  contact_email: string;
  institution_name: string;
  lab_name: string;
  plans_to_contribute: boolean;
  contributor_attestation: boolean;
  accepted_contribution_policy_version: string | null;
  data_use_agreement: true;
  accepted_data_use_policy_version: string;
}

export interface SubmittedArchiveAccessRequest {
  response: Record<string, unknown>;
  notification: {
    request_id: string;
    contact_name: string;
    contact_email: string;
    institution_name: string;
    lab_name: string;
    plans_to_contribute: boolean;
    contributor_attestation: boolean;
    accepted_contribution_policy_version: string | null;
    accepted_data_use_policy_version: string;
    submitted_at: string;
  };
}

interface ArchiveAccessRow {
  id: string;
  data_use_agreement: number;
  accepted_data_use_policy_version: string | null;
  data_use_agreement_accepted_at: number | null;
  revoked_at: number | null;
}

type ArchiveAccessReviewStatus = "pending" | "approved" | "rejected";

interface ArchiveAccessRequestRow {
  id: string;
  email_hash: string;
  email_ciphertext: string;
  contact_name: string;
  institution_name: string;
  lab_name: string;
  status: ArchiveAccessReviewStatus;
  created_at: number;
  updated_at: number;
  reviewed_at: number | null;
  data_use_agreement: number;
  accepted_data_use_policy_version: string | null;
  data_use_agreement_accepted_at: number | null;
  plans_to_contribute: number | null;
  contributor_attestation: number | null;
  accepted_contribution_policy_version: string | null;
  contributor_attestation_accepted_at: number | null;
}

interface ArchiveSeriesRow {
  upload_id: string;
  series_archive_id: string;
  series_id: string;
  dicom_count: number;
  expected_size: number;
  expected_sha256: string;
  archive_prefix: string;
  archive_relative_key: string;
  received_at: number;
  data_license_id: string;
  data_license_granted_at: number;
}

function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

function requiredText(
  value: unknown,
  label: string,
  maximum: number,
): string {
  if (typeof value !== "string") {
    throw new AppError("INVALID_REQUEST", 400, `${label} must be text`);
  }
  const normalized = value.trim().replace(/\s+/gu, " ");
  if (normalized.length < 2 || normalized.length > maximum) {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      `${label} must contain 2-${maximum} characters`,
    );
  }
  return normalized;
}

export function parseArchiveAccessRequest(
  value: unknown,
): ArchiveAccessRequest {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new AppError("INVALID_REQUEST", 400, "Request body must be an object");
  }
  const input = value as Record<string, unknown>;
  const expected = new Set([
    "contact_name",
    "contact_email",
    "institution_name",
    "lab_name",
    "plans_to_contribute",
    "contributor_attestation",
    "accepted_contribution_policy_version",
    "data_use_agreement",
    "accepted_data_use_policy_version",
  ]);
  if (Object.keys(input).some((key) => !expected.has(key))) {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      "Request contains an unknown field",
    );
  }
  const contactEmail = requiredText(input.contact_email, "Work email", 254)
    .toLowerCase();
  if (!EMAIL.test(contactEmail)) {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      "Work email has an invalid format",
    );
  }
  if (typeof input.plans_to_contribute !== "boolean") {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      "Choose whether you plan to contribute data",
    );
  }
  if (input.data_use_agreement !== true) {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      "The archive access and privacy agreement must be accepted",
    );
  }
  if (
    input.accepted_data_use_policy_version !== ARCHIVE_ACCESS_POLICY_VERSION
  ) {
    throw new AppError(
      "ARCHIVE_ACCESS_POLICY_UPDATE_REQUIRED",
      409,
      "Review and accept the current archive access and privacy agreement",
      { data_use_policy_version: ARCHIVE_ACCESS_POLICY_VERSION },
    );
  }
  if (
    input.plans_to_contribute &&
    input.contributor_attestation !== true
  ) {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      "The contributor attestation must be accepted before planning to upload data",
    );
  }
  if (
    input.plans_to_contribute &&
    input.accepted_contribution_policy_version !==
      PUBLIC_CONSENT_POLICY_VERSION
  ) {
    throw new AppError(
      "CONSENT_POLICY_UPDATE_REQUIRED",
      409,
      "Review and accept the current data contribution and CC0 policy",
      { consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION },
    );
  }
  if (
    !input.plans_to_contribute &&
    (input.contributor_attestation !== false ||
      input.accepted_contribution_policy_version !== null)
  ) {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      "Contributor attestation must be omitted when you do not plan to contribute data",
    );
  }
  return {
    contact_name: requiredText(input.contact_name, "Name", 120),
    contact_email: contactEmail,
    institution_name: requiredText(input.institution_name, "Institution", 160),
    lab_name: requiredText(input.lab_name, "Lab", 160),
    plans_to_contribute: input.plans_to_contribute,
    contributor_attestation: input.plans_to_contribute,
    accepted_contribution_policy_version: input.plans_to_contribute
      ? PUBLIC_CONSENT_POLICY_VERSION
      : null,
    data_use_agreement: true,
    accepted_data_use_policy_version: ARCHIVE_ACCESS_POLICY_VERSION,
  };
}

export async function submitArchiveAccessRequest(
  env: Env,
  input: ArchiveAccessRequest,
): Promise<SubmittedArchiveAccessRequest> {
  const emailHash = await sha256Hex(input.contact_email);
  const existing = await env.DB.prepare(
    `SELECT id FROM archive_access_requests
     WHERE email_hash = ?1 LIMIT 1`,
  )
    .bind(emailHash)
    .first<{ id: string }>();
  const id = existing?.id ?? crypto.randomUUID();
  const emailCiphertext = await encryptArchiveAccessRequestEmail(
    input.contact_email,
    id,
    env.SITE_KEY_ENCRYPTION_KEY_B64,
  );
  const timestamp = nowSeconds();
  await env.DB.prepare(
    `INSERT INTO archive_access_requests
       (id, email_hash, email_ciphertext, contact_name, institution_name,
        lab_name, participation_commitment, plans_to_contribute,
        contributor_attestation, accepted_contribution_policy_version,
        contributor_attestation_accepted_at, data_use_agreement,
        accepted_data_use_policy_version, data_use_agreement_accepted_at,
        status, created_at, updated_at,
        reviewed_at, approved_registration_id)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10, 1, ?11, ?12,
             'pending', ?12, ?12, NULL, NULL)
     ON CONFLICT(email_hash) DO UPDATE SET
       email_ciphertext = excluded.email_ciphertext,
       contact_name = excluded.contact_name,
       institution_name = excluded.institution_name,
       lab_name = excluded.lab_name,
       participation_commitment = 1,
       plans_to_contribute = excluded.plans_to_contribute,
       contributor_attestation = excluded.contributor_attestation,
       accepted_contribution_policy_version = excluded.accepted_contribution_policy_version,
       contributor_attestation_accepted_at = excluded.contributor_attestation_accepted_at,
       data_use_agreement = 1,
       accepted_data_use_policy_version = excluded.accepted_data_use_policy_version,
       data_use_agreement_accepted_at = excluded.data_use_agreement_accepted_at,
       status = 'pending',
       updated_at = excluded.updated_at,
       reviewed_at = NULL,
       approved_registration_id = NULL`,
  )
    .bind(
      id,
      emailHash,
      emailCiphertext,
      input.contact_name,
      input.institution_name,
      input.lab_name,
      input.plans_to_contribute ? 1 : 0,
      input.contributor_attestation ? 1 : 0,
      input.accepted_contribution_policy_version,
      input.contributor_attestation ? timestamp : null,
      input.accepted_data_use_policy_version,
      timestamp,
    )
    .run();
  return {
    response: {
      request_id: id,
      status: "pending_review",
      message:
        "Your request is pending review. We will email next steps to your work address.",
    },
    notification: {
      request_id: id,
      contact_name: input.contact_name,
      contact_email: input.contact_email,
      institution_name: input.institution_name,
      lab_name: input.lab_name,
      plans_to_contribute: input.plans_to_contribute,
      contributor_attestation: input.contributor_attestation,
      accepted_contribution_policy_version:
        input.accepted_contribution_policy_version,
      accepted_data_use_policy_version:
        input.accepted_data_use_policy_version,
      submitted_at: new Date(timestamp * 1000).toISOString(),
    },
  };
}

function adminBearerToken(request: Request): string {
  const authorization = request.headers.get("authorization");
  if (!authorization?.startsWith("Bearer ")) {
    throw new AppError("UNAUTHORIZED", 401, "Admin authentication is required");
  }
  const token = authorization.slice("Bearer ".length);
  if (token.length < 32 || token.length > 256 || /\s/u.test(token)) {
    throw new AppError("UNAUTHORIZED", 401, "Admin authentication is invalid");
  }
  return token;
}

export async function authenticateArchiveAccessAdmin(
  request: Request,
  env: Env,
): Promise<void> {
  const [providedHash, expectedHash] = await Promise.all([
    sha256Hex(adminBearerToken(request)),
    sha256Hex(env.ARCHIVE_ACCESS_ADMIN_TOKEN),
  ]);
  const workersSubtle = crypto.subtle as SubtleCrypto & {
    timingSafeEqual(
      first: Uint8Array<ArrayBuffer>,
      second: Uint8Array<ArrayBuffer>,
    ): boolean;
  };
  if (
    !workersSubtle.timingSafeEqual(
      utf8Bytes(providedHash),
      utf8Bytes(expectedHash),
    )
  ) {
    throw new AppError("UNAUTHORIZED", 401, "Admin authentication is invalid");
  }
}

function isoTime(value: number | null): string | null {
  return value === null ? null : new Date(value * 1000).toISOString();
}

async function requestForAdministration(
  env: Env,
  row: ArchiveAccessRequestRow,
): Promise<Record<string, unknown>> {
  const contactEmail = await decryptArchiveAccessRequestEmail(
    row.email_ciphertext,
    row.id,
    env.SITE_KEY_ENCRYPTION_KEY_B64,
  );
  return {
    request_id: row.id,
    status: row.status,
    contact_name: row.contact_name,
    contact_email: contactEmail,
    institution_name: row.institution_name,
    lab_name: row.lab_name,
    plans_to_contribute:
      row.plans_to_contribute === null
        ? null
        : row.plans_to_contribute === 1,
    contributor_attestation: row.contributor_attestation === 1,
    accepted_contribution_policy_version:
      row.accepted_contribution_policy_version,
    contributor_attestation_accepted_at:
      isoTime(row.contributor_attestation_accepted_at),
    data_use_agreement: row.data_use_agreement === 1,
    accepted_data_use_policy_version:
      row.accepted_data_use_policy_version,
    data_use_agreement_accepted_at:
      isoTime(row.data_use_agreement_accepted_at),
    submitted_at: isoTime(row.created_at),
    updated_at: isoTime(row.updated_at),
    reviewed_at: isoTime(row.reviewed_at),
  };
}

export async function listArchiveAccessRequests(
  request: Request,
  env: Env,
): Promise<Record<string, unknown>> {
  await authenticateArchiveAccessAdmin(request, env);
  const requestedStatus = new URL(request.url).searchParams.get("status");
  if (
    requestedStatus !== null &&
    !["pending", "approved", "rejected", "all"].includes(requestedStatus)
  ) {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      "Status must be pending, approved, rejected, or all",
    );
  }
  const status = requestedStatus ?? "pending";
  const statement =
    status === "all"
      ? env.DB.prepare(
          `SELECT id, email_hash, email_ciphertext, contact_name,
                  institution_name, lab_name, status, created_at, updated_at,
                  reviewed_at, plans_to_contribute, contributor_attestation,
                  accepted_contribution_policy_version,
                  contributor_attestation_accepted_at, data_use_agreement,
                  accepted_data_use_policy_version,
                  data_use_agreement_accepted_at
           FROM archive_access_requests
           ORDER BY created_at ASC
           LIMIT ?1`,
        ).bind(MAX_REVIEW_ROWS)
      : env.DB.prepare(
          `SELECT id, email_hash, email_ciphertext, contact_name,
                  institution_name, lab_name, status, created_at, updated_at,
                  reviewed_at, plans_to_contribute, contributor_attestation,
                  accepted_contribution_policy_version,
                  contributor_attestation_accepted_at, data_use_agreement,
                  accepted_data_use_policy_version,
                  data_use_agreement_accepted_at
           FROM archive_access_requests
           WHERE status = ?1
           ORDER BY created_at ASC
           LIMIT ?2`,
        ).bind(status, MAX_REVIEW_ROWS);
  const rows = (await statement.all<ArchiveAccessRequestRow>()).results;
  return {
    requests: await Promise.all(
      rows.map((row) => requestForAdministration(env, row)),
    ),
  };
}

async function archiveAccessRequestRow(
  env: Env,
  requestId: string,
): Promise<ArchiveAccessRequestRow> {
  const row = await env.DB.prepare(
    `SELECT id, email_hash, email_ciphertext, contact_name, institution_name,
            lab_name, status, created_at, updated_at, reviewed_at,
            plans_to_contribute, contributor_attestation,
            accepted_contribution_policy_version,
            contributor_attestation_accepted_at,
            data_use_agreement, accepted_data_use_policy_version,
            data_use_agreement_accepted_at
     FROM archive_access_requests
     WHERE id = ?1
     LIMIT 1`,
  )
    .bind(requestId)
    .first<ArchiveAccessRequestRow>();
  if (!row) {
    throw new AppError("NOT_FOUND", 404, "Archive access request was not found");
  }
  return row;
}

export async function approveArchiveAccessRequest(
  request: Request,
  env: Env,
  requestId: string,
): Promise<Record<string, unknown>> {
  await authenticateArchiveAccessAdmin(request, env);
  const accessRequest = await archiveAccessRequestRow(env, requestId);
  if (accessRequest.status !== "pending") {
    throw new AppError(
      "CONFLICT",
      409,
      "Only a pending archive access request can be approved",
    );
  }
  if (
    accessRequest.data_use_agreement !== 1 ||
    accessRequest.accepted_data_use_policy_version !==
      ARCHIVE_ACCESS_POLICY_VERSION ||
    accessRequest.data_use_agreement_accepted_at === null
  ) {
    throw new AppError(
      "ARCHIVE_ACCESS_POLICY_UPDATE_REQUIRED",
      409,
      "The requester must submit a new acceptance of the current archive access and privacy agreement",
      { data_use_policy_version: ARCHIVE_ACCESS_POLICY_VERSION },
    );
  }
  if (
    accessRequest.plans_to_contribute === 1 &&
    (accessRequest.contributor_attestation !== 1 ||
      accessRequest.accepted_contribution_policy_version !==
        PUBLIC_CONSENT_POLICY_VERSION ||
      accessRequest.contributor_attestation_accepted_at === null)
  ) {
    throw new AppError(
      "CONSENT_POLICY_UPDATE_REQUIRED",
      409,
      "The requester must submit a current contributor attestation",
      { consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION },
    );
  }
  const contactEmail = await decryptArchiveAccessRequestEmail(
    accessRequest.email_ciphertext,
    accessRequest.id,
    env.SITE_KEY_ENCRYPTION_KEY_B64,
  );
  const existingRegistration = await env.DB.prepare(
    `SELECT id FROM archive_access_registrations
     WHERE email_hash = ?1
     LIMIT 1`,
  )
    .bind(accessRequest.email_hash)
    .first<{ id: string }>();
  const registrationId = existingRegistration?.id ?? crypto.randomUUID();
  const token = randomAccessToken();
  const [tokenHash, registrationEmailCiphertext] = await Promise.all([
    sha256Hex(token),
    encryptArchiveAccessEmail(
      contactEmail,
      registrationId,
      env.SITE_KEY_ENCRYPTION_KEY_B64,
    ),
  ]);
  const timestamp = nowSeconds();
  const results = await env.DB.batch([
    env.DB.prepare(
      `INSERT INTO archive_access_registrations
         (id, token_hash, email_hash, email_ciphertext, contact_name,
          institution_name, lab_name, participation_commitment,
          plans_to_contribute, contributor_attestation,
          accepted_contribution_policy_version,
          contributor_attestation_accepted_at,
          data_use_agreement, accepted_data_use_policy_version,
          data_use_agreement_accepted_at,
          created_at, updated_at, revoked_at)
       SELECT ?1, ?2, r.email_hash, ?3, r.contact_name, r.institution_name,
              r.lab_name, 1, r.plans_to_contribute,
              r.contributor_attestation,
              r.accepted_contribution_policy_version,
              r.contributor_attestation_accepted_at, r.data_use_agreement,
              r.accepted_data_use_policy_version,
              r.data_use_agreement_accepted_at, ?4, ?4, NULL
       FROM archive_access_requests r
       WHERE r.id = ?5 AND r.status = 'pending'
       ON CONFLICT(email_hash) DO UPDATE SET
         token_hash = excluded.token_hash,
         email_ciphertext = excluded.email_ciphertext,
         contact_name = excluded.contact_name,
         institution_name = excluded.institution_name,
         lab_name = excluded.lab_name,
         participation_commitment = 1,
         plans_to_contribute = excluded.plans_to_contribute,
         contributor_attestation = excluded.contributor_attestation,
         accepted_contribution_policy_version = excluded.accepted_contribution_policy_version,
         contributor_attestation_accepted_at = excluded.contributor_attestation_accepted_at,
         data_use_agreement = excluded.data_use_agreement,
         accepted_data_use_policy_version = excluded.accepted_data_use_policy_version,
         data_use_agreement_accepted_at = excluded.data_use_agreement_accepted_at,
         updated_at = excluded.updated_at,
         revoked_at = NULL`,
    ).bind(
      registrationId,
      tokenHash,
      registrationEmailCiphertext,
      timestamp,
      requestId,
    ),
    env.DB.prepare(
      `UPDATE archive_access_requests
       SET status = 'approved', updated_at = ?1, reviewed_at = ?1,
           approved_registration_id = ?2
       WHERE id = ?3 AND status = 'pending'`,
    ).bind(timestamp, registrationId, requestId),
  ]);
  if (results[0]?.meta.changes !== 1 || results[1]?.meta.changes !== 1) {
    throw new AppError(
      "CONFLICT",
      409,
      "Archive access request is no longer pending",
    );
  }
  return {
    request_id: requestId,
    status: "approved",
    contact_name: accessRequest.contact_name,
    contact_email: contactEmail,
    institution_name: accessRequest.institution_name,
    lab_name: accessRequest.lab_name,
    plans_to_contribute:
      accessRequest.plans_to_contribute === null
        ? null
        : accessRequest.plans_to_contribute === 1,
    contributor_attestation:
      accessRequest.contributor_attestation === 1,
    accepted_contribution_policy_version:
      accessRequest.accepted_contribution_policy_version,
    contributor_attestation_accepted_at:
      isoTime(accessRequest.contributor_attestation_accepted_at),
    accepted_data_use_policy_version:
      accessRequest.accepted_data_use_policy_version,
    data_use_agreement_accepted_at:
      isoTime(accessRequest.data_use_agreement_accepted_at),
    access_token: token,
    token_type: "Bearer",
    archive_url: "https://scalingneuro.org/v1/archive",
  };
}

export async function rejectArchiveAccessRequest(
  request: Request,
  env: Env,
  requestId: string,
): Promise<Record<string, unknown>> {
  await authenticateArchiveAccessAdmin(request, env);
  const timestamp = nowSeconds();
  const result = await env.DB.prepare(
    `UPDATE archive_access_requests
     SET status = 'rejected', updated_at = ?1, reviewed_at = ?1,
         approved_registration_id = NULL
     WHERE id = ?2 AND status = 'pending'`,
  )
    .bind(timestamp, requestId)
    .run();
  if (result.meta.changes !== 1) {
    const existing = await archiveAccessRequestRow(env, requestId);
    throw new AppError(
      "CONFLICT",
      409,
      `Only a pending archive access request can be rejected; current status is ${existing.status}`,
    );
  }
  return { request_id: requestId, status: "rejected" };
}

function bearerToken(request: Request): string {
  const authorization = request.headers.get("authorization");
  if (!authorization?.startsWith("Bearer ")) {
    throw new AppError(
      "UNAUTHORIZED",
      401,
      "Archive access token is required",
    );
  }
  const token = authorization.slice("Bearer ".length);
  if (
    !token.startsWith("sn_access_") ||
    token.length > 128 ||
    /\s/u.test(token)
  ) {
    throw new AppError("UNAUTHORIZED", 401, "Archive access token is invalid");
  }
  return token;
}

async function authenticateArchiveAccess(
  request: Request,
  env: Env,
): Promise<ArchiveAccessRow> {
  const tokenHash = await sha256Hex(bearerToken(request));
  const row = await env.DB.prepare(
    `SELECT id, data_use_agreement, accepted_data_use_policy_version,
            data_use_agreement_accepted_at, revoked_at
     FROM archive_access_registrations
     WHERE token_hash = ?1 LIMIT 1`,
  )
    .bind(tokenHash)
    .first<ArchiveAccessRow>();
  if (!row || row.revoked_at !== null) {
    throw new AppError(
      "UNAUTHORIZED",
      401,
      "Archive access token is invalid",
    );
  }
  if (
    row.data_use_agreement !== 1 ||
    row.accepted_data_use_policy_version !== ARCHIVE_ACCESS_POLICY_VERSION ||
    row.data_use_agreement_accepted_at === null
  ) {
    throw new AppError(
      "ARCHIVE_ACCESS_POLICY_UPDATE_REQUIRED",
      403,
      "Archive access requires acceptance of the current access and privacy agreement",
      { data_use_policy_version: ARCHIVE_ACCESS_POLICY_VERSION },
    );
  }
  await env.DB.prepare(
    `UPDATE archive_access_registrations SET last_seen_at = ?1
     WHERE id = ?2`,
  )
    .bind(nowSeconds(), row.id)
    .run();
  return row;
}

export async function listArchive(
  request: Request,
  env: Env,
): Promise<Record<string, unknown>> {
  await authenticateArchiveAccess(request, env);
  const rows = (
    await env.DB.prepare(
      `SELECT u.id AS upload_id, d.series_archive_id, d.series_id,
              d.dicom_count, d.expected_size, d.expected_sha256,
              u.archive_prefix, d.archive_relative_key, u.received_at,
              u.data_license_id,
              COALESCE(u.data_license_granted_at, u.publication_scheduled_at)
                AS data_license_granted_at
       FROM dicom_upload_series d
       JOIN uploads u ON u.id = d.upload_id
       JOIN received_series_reservations r
         ON r.upload_id = d.upload_id AND r.bundle_id = d.series_archive_id
       WHERE u.status = 'committed' AND u.withdrawn_at IS NULL
         AND r.withdrawn_at IS NULL AND d.series_kind = 'functional_epi'
         AND d.completed_at IS NOT NULL
         AND u.data_license_id = ?1
         AND u.publication_scheduled_at IS NOT NULL
         AND u.publication_scheduled_at <= ?2
       ORDER BY u.received_at DESC, d.series_archive_id
       LIMIT ?3`,
    )
      .bind(DATA_LICENSE_ID, nowSeconds(), MAX_ARCHIVE_ROWS)
      .all<ArchiveSeriesRow>()
  ).results;
  return {
    format: "dicom-tar-zstd",
    series: rows.map((row) => ({
      upload_id: row.upload_id,
      series_archive_id: row.series_archive_id,
      series_id: row.series_id,
      dicom_count: row.dicom_count,
      size: row.expected_size,
      sha256: row.expected_sha256,
      received_at: new Date(row.received_at * 1000).toISOString(),
      data_license: {
        id: row.data_license_id,
        url: DATA_LICENSE_URL,
        granted_at: new Date(
          row.data_license_granted_at * 1000,
        ).toISOString(),
      },
      download_url:
        `https://scalingneuro.com/v1/archive/${row.upload_id}/` +
        `${row.series_archive_id}/download`,
    })),
  };
}

export async function signArchiveDownload(
  request: Request,
  env: Env,
  uploadId: string,
  seriesArchiveId: string,
): Promise<string> {
  await authenticateArchiveAccess(request, env);
  if (!SERIES_ARCHIVE_ID.test(seriesArchiveId)) {
    throw new AppError("NOT_FOUND", 404, "Archive series was not found");
  }
  const row = await env.DB.prepare(
    `SELECT u.id AS upload_id, d.series_archive_id, d.series_id,
            d.dicom_count, d.expected_size, d.expected_sha256,
            u.archive_prefix, d.archive_relative_key, u.received_at,
            u.data_license_id,
            COALESCE(u.data_license_granted_at, u.publication_scheduled_at)
              AS data_license_granted_at
     FROM dicom_upload_series d
     JOIN uploads u ON u.id = d.upload_id
     JOIN received_series_reservations r
       ON r.upload_id = d.upload_id AND r.bundle_id = d.series_archive_id
     WHERE u.id = ?1 AND d.series_archive_id = ?2
       AND u.status = 'committed' AND u.withdrawn_at IS NULL
       AND r.withdrawn_at IS NULL AND d.series_kind = 'functional_epi'
       AND d.completed_at IS NOT NULL
       AND u.data_license_id = ?3
       AND u.publication_scheduled_at IS NOT NULL
       AND u.publication_scheduled_at <= ?4
     LIMIT 1`,
  )
    .bind(uploadId, seriesArchiveId, DATA_LICENSE_ID, nowSeconds())
    .first<ArchiveSeriesRow>();
  if (!row) {
    throw new AppError("NOT_FOUND", 404, "Archive series was not found");
  }
  return (
    await presignGetObject(
    env,
    `${row.archive_prefix}${row.archive_relative_key}`,
    )
  ).url;
}
