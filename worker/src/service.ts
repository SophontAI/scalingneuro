import { authenticateAdmin, authenticateDevice } from "./auth";
import {
  canonicalJson,
  constantTimeEqual,
  decryptSiteKey,
  encryptSiteKey,
  pseudonymKeyBase64,
  randomBytes,
  randomOpaqueToken,
  sha256Hex,
  sha256StreamHex,
  utf8Bytes,
  utf8String,
} from "./crypto";
import { AppError } from "./errors";
import type { DeviceContext, Env, UploadStatus } from "./env";
import {
  assertNiftiMatchesSidecar,
  inspectGzipNifti,
  type NiftiFacts,
} from "./nifti";
import {
  deletePrefix,
  presignUploadPart as signR2UploadPart,
  uploadTtl,
} from "./r2";
import { validateSidecarBytes, type ValidatedSidecar } from "./sidecar";
import type {
  AdminInviteRequest,
  BundleDescriptor,
  CompleteUploadRequest,
  CreateUploadRequest,
  EnrollRequest,
  SignPartRequest,
} from "./validation";

interface InviteRow {
  id: string;
  site_id: string;
  project_id: string;
  expires_at: number;
  max_uses: number;
  uses: number;
  revoked_at: number | null;
  project_name: string;
  consent_policy_version: string;
  project_active: number;
  pseudonym_key_ciphertext: string;
}

interface EnrollmentRow {
  device_id: string;
  enrollment_id: string;
  token_hash: string;
  revoked_at: number | null;
  site_id: string;
  project_id: string;
  project_name: string;
  accepted_consent_policy_version: string;
  pseudonym_key_ciphertext: string;
}

interface SiteRow {
  id: string;
  name: string;
  pseudonym_key_ciphertext: string;
}

interface ProjectRow {
  id: string;
  name: string;
  consent_policy_version: string;
  active: number;
}

interface UploadRow {
  id: string;
  site_id: string;
  project_id: string;
  device_id: string;
  status: UploadStatus;
  archive_prefix: string;
  request_hash: string;
  client_version: string;
  consent_policy_version: string;
  series_count: number;
  total_bytes: number;
  created_at: number;
  updated_at: number;
  expires_at: number;
  committed_at: number | null;
  withdrawn_at: number | null;
  purged_at: number | null;
  manifest_object_key: string | null;
  manifest_sha256: string | null;
  operation_token: string | null;
  operation_kind: "initialize" | "verify" | "purge" | null;
  operation_expires_at: number | null;
}

interface BundleRow {
  upload_id: string;
  bundle_id: string;
  series_id: string;
  subject_id: string;
  session_id: string;
  protocol_group_id: string;
  bundle_hash: string;
  nii_relative_key: string;
  nii_size: number;
  nii_sha256: string;
  nii_uncompressed_sha256: string;
  metadata_relative_key: string;
  metadata_size: number;
  metadata_sha256: string;
}

interface CatalogRow {
  bundle_id: string;
  upload_id: string;
}

interface UploadObjectRow {
  upload_id: string;
  object_key: string;
  bundle_id: string;
  kind: "nii" | "metadata";
  expected_size: number;
  expected_sha256: string;
  r2_multipart_id: string | null;
  part_size: number | null;
  completed_at: number | null;
  etag: string | null;
}

interface ExpectedObject {
  key: string;
  size: number;
  sha256: string;
  bundle_id: string;
  kind: "nii" | "metadata";
}

interface VerifiedObject extends ExpectedObject {
  etag: string;
  nifti?: NiftiFacts;
  sidecar?: ValidatedSidecar;
}

class StoredObjectValidationError extends AppError {
  constructor(message: string, details?: Readonly<Record<string, unknown>>) {
    super("OBJECT_MISMATCH", 409, message, details);
    this.name = "StoredObjectValidationError";
  }
}

const BASE_PART_SIZE = 64 * 1024 * 1024;
const PART_SIZE_GRANULARITY = 1024 * 1024;
const INITIALIZE_LEASE_SECONDS = 5 * 60;
const VERIFY_LEASE_SECONDS = 60 * 60;

function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

function iso(seconds: number | null): string | null {
  return seconds === null ? null : new Date(seconds * 1000).toISOString();
}

function archiveManifestKey(upload: UploadRow): string {
  return `manifests/v1/${upload.site_id}/${upload.project_id}/${upload.id}.json`;
}

function auditStatement(
  env: Env,
  eventType: string,
  values: {
    siteId?: string | null;
    projectId?: string | null;
    deviceId?: string | null;
    uploadId?: string | null;
    subjectType?: string | null;
    subjectId?: string | null;
    detailCode?: string | null;
    createdAt?: number;
  },
): D1PreparedStatement {
  return env.DB.prepare(
    `INSERT INTO audit_events
       (id, event_type, site_id, project_id, device_id, upload_id,
        subject_type, subject_id, detail_code, created_at)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)`,
  ).bind(
    crypto.randomUUID(),
    eventType,
    values.siteId ?? null,
    values.projectId ?? null,
    values.deviceId ?? null,
    values.uploadId ?? null,
    values.subjectType ?? null,
    values.subjectId ?? null,
    values.detailCode ?? null,
    values.createdAt ?? nowSeconds(),
  );
}

function stripEtag(value: string): string {
  return value.replace(/^"|"$/gu, "");
}

function multipartPartSize(objectSize: number): number {
  const minimumForTenThousandParts = Math.ceil(objectSize / 10_000);
  const roundedMinimum =
    Math.ceil(minimumForTenThousandParts / PART_SIZE_GRANULARITY) *
    PART_SIZE_GRANULARITY;
  return Math.max(BASE_PART_SIZE, roundedMinimum);
}

async function claimUploadOperation(
  env: Env,
  upload: UploadRow,
  kind: "initialize" | "verify",
  leaseSeconds: number,
): Promise<{ upload: UploadRow; token: string } | null> {
  const timestamp = nowSeconds();
  const token = crypto.randomUUID();
  const claimed = await env.DB.prepare(
    `UPDATE uploads
     SET operation_token = ?1, operation_kind = ?2,
         operation_expires_at = ?3, updated_at = ?4
     WHERE id = ?5
       AND status IN ('created', 'uploading')
       AND expires_at > ?4
       AND (operation_token IS NULL OR operation_expires_at <= ?4)
     RETURNING *`,
  )
    .bind(token, kind, timestamp + leaseSeconds, timestamp, upload.id)
    .first<UploadRow>();
  return claimed ? { upload: claimed, token } : null;
}

async function releaseUploadOperation(
  env: Env,
  uploadId: string,
  token: string,
): Promise<void> {
  await env.DB.prepare(
    `UPDATE uploads
     SET operation_token = NULL, operation_kind = NULL,
         operation_expires_at = NULL, updated_at = ?1
     WHERE id = ?2 AND operation_token = ?3`,
  )
    .bind(nowSeconds(), uploadId, token)
    .run();
}

async function ensureMultipartUploads(
  env: Env,
  upload: UploadRow,
): Promise<UploadObjectRow[]> {
  let result = await env.DB.prepare(
    "SELECT * FROM upload_objects WHERE upload_id = ?1 ORDER BY object_key",
  )
    .bind(upload.id)
    .all<UploadObjectRow>();
  if (result.results.length !== upload.series_count * 2) {
    throw new AppError("INTERNAL", 500, "Upload object catalog is incomplete");
  }

  const missing = result.results.filter(
    (object) => object.r2_multipart_id === null || object.part_size === null,
  );
  for (let start = 0; start < missing.length; start += 8) {
    await Promise.all(
      missing.slice(start, start + 8).map(async (object) => {
        const partSize = multipartPartSize(object.expected_size);
        let multipart: R2MultipartUpload;
        try {
          multipart = await env.ARCHIVE.createMultipartUpload(
            object.object_key,
            {
              httpMetadata: {
                contentType:
                  object.kind === "nii"
                    ? "application/gzip"
                    : "application/json; charset=utf-8",
              },
              customMetadata: {
                upload_id: upload.id,
                sha256: object.expected_sha256,
              },
            },
          );
        } catch {
          throw new AppError(
            "STORAGE_UNAVAILABLE",
            502,
            "Unable to initialize multipart uploads",
          );
        }

        try {
          const update = await env.DB.prepare(
            `UPDATE upload_objects
             SET r2_multipart_id = ?1, part_size = ?2
             WHERE upload_id = ?3 AND object_key = ?4 AND r2_multipart_id IS NULL`,
          )
            .bind(multipart.uploadId, partSize, upload.id, object.object_key)
            .run();
          if ((update.meta.changes ?? 0) === 0) {
            try {
              await multipart.abort();
            } catch {
              // The winning initialization remains persisted and usable.
            }
          }
        } catch (error) {
          try {
            await multipart.abort();
          } catch {
            // R2 automatically aborts uncompleted multipart uploads after seven days.
          }
          if (error instanceof AppError) throw error;
          throw new AppError(
            "STORAGE_UNAVAILABLE",
            502,
            "Unable to persist multipart upload state",
          );
        }
      }),
    );
  }

  result = await env.DB.prepare(
    "SELECT * FROM upload_objects WHERE upload_id = ?1 ORDER BY object_key",
  )
    .bind(upload.id)
    .all<UploadObjectRow>();
  if (
    result.results.length !== upload.series_count * 2 ||
    result.results.some(
      (object) => !object.r2_multipart_id || !object.part_size,
    )
  ) {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Multipart upload initialization is incomplete",
    );
  }
  return result.results;
}

async function abortMultipartUploads(
  env: Env,
  uploadId: string,
): Promise<void> {
  const result = await env.DB.prepare(
    `SELECT * FROM upload_objects
     WHERE upload_id = ?1 AND r2_multipart_id IS NOT NULL AND completed_at IS NULL`,
  )
    .bind(uploadId)
    .all<UploadObjectRow>();
  for (let start = 0; start < result.results.length; start += 8) {
    await Promise.all(
      result.results.slice(start, start + 8).map(async (object) => {
        try {
          await env.ARCHIVE.resumeMultipartUpload(
            object.object_key,
            object.r2_multipart_id as string,
          ).abort();
        } catch (error) {
          const message =
            error instanceof Error ? `${error.name} ${error.message}` : "";
          if (
            /NoSuchUpload|10024|does not exist|already (?:completed|aborted)/iu.test(
              message,
            )
          ) {
            return;
          }
          throw new AppError(
            "STORAGE_UNAVAILABLE",
            502,
            "Unable to abort multipart upload",
          );
        }
      }),
    );
  }
}

async function bundleHash(bundle: BundleDescriptor): Promise<string> {
  return sha256Hex(
    canonicalJson({
      series_id: bundle.series_id,
      subject_id: bundle.subject_id,
      session_id: bundle.session_id,
      nii: { uncompressed_sha256: bundle.nii.uncompressed_sha256 },
    }),
  );
}

async function getUploadForDevice(
  env: Env,
  uploadId: string,
  deviceId: string,
): Promise<UploadRow> {
  const upload = await env.DB.prepare(
    "SELECT * FROM uploads WHERE id = ?1 AND device_id = ?2 LIMIT 1",
  )
    .bind(uploadId, deviceId)
    .first<UploadRow>();
  if (!upload) throw new AppError("NOT_FOUND", 404, "Upload was not found");
  return upload;
}

function uploadStatusResponse(upload: UploadRow): Record<string, unknown> {
  const response: Record<string, unknown> = {
    upload_id: upload.id,
    status: upload.status,
    object_prefix: upload.archive_prefix,
    series_count: upload.series_count,
    total_bytes: upload.total_bytes,
    consent_policy_version: upload.consent_policy_version,
    created_at: iso(upload.created_at),
    updated_at: iso(upload.updated_at),
  };
  if (upload.committed_at !== null)
    response.committed_at = iso(upload.committed_at);
  if (upload.withdrawn_at !== null)
    response.withdrawn_at = iso(upload.withdrawn_at);
  if (upload.manifest_object_key && upload.manifest_sha256) {
    response.manifest = {
      key: upload.manifest_object_key,
      sha256: upload.manifest_sha256,
    };
  }
  return response;
}

async function activeDuplicates(
  env: Env,
  device: DeviceContext,
  bundles: ReadonlyArray<{ descriptor: BundleDescriptor; hash: string }>,
): Promise<CatalogRow[]> {
  const rows: CatalogRow[] = [];
  for (let start = 0; start < bundles.length; start += 40) {
    const bundleIds = [
      ...new Set(
        bundles
          .slice(start, start + 40)
          .map((item) => item.descriptor.bundle_id),
      ),
    ];
    const placeholders = bundleIds
      .map((_, index) => `?${index + 3}`)
      .join(", ");
    const result = await env.DB.prepare(
      `SELECT bundle_id, upload_id
       FROM catalog_series
       WHERE site_id = ?1
         AND project_id = ?2
         AND bundle_id IN (${placeholders})`,
    )
      .bind(device.site_id, device.project_id, ...bundleIds)
      .all<CatalogRow>();
    rows.push(...result.results);
  }
  return rows;
}

async function createCredentialsResponse(
  env: Env,
  upload: UploadRow,
): Promise<Record<string, unknown>> {
  if (upload.status === "committed") {
    return {
      upload_id: upload.id,
      status: upload.status,
      object_prefix: upload.archive_prefix,
      multipart_objects: [],
    };
  }
  if (upload.status === "expired" || upload.status === "withdrawn") {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Upload is no longer writable",
    );
  }
  if (upload.expires_at <= nowSeconds()) {
    await env.DB.prepare(
      `UPDATE uploads SET status = 'expired', updated_at = ?1
       WHERE id = ?2 AND status IN ('created', 'uploading')
         AND (operation_token IS NULL OR operation_expires_at <= ?1)`,
    )
      .bind(nowSeconds(), upload.id)
      .run();
    throw new AppError("UPLOAD_NOT_WRITABLE", 409, "Upload has expired");
  }

  const operation = await claimUploadOperation(
    env,
    upload,
    "initialize",
    INITIALIZE_LEASE_SECONDS,
  );
  if (!operation) {
    const current = await env.DB.prepare(
      "SELECT * FROM uploads WHERE id = ?1 LIMIT 1",
    )
      .bind(upload.id)
      .first<UploadRow>();
    if (current?.status === "committed") {
      return {
        upload_id: current.id,
        status: current.status,
        object_prefix: current.archive_prefix,
        multipart_objects: [],
      };
    }
    if (
      !current ||
      current.status === "expired" ||
      current.status === "withdrawn"
    ) {
      throw new AppError(
        "UPLOAD_NOT_WRITABLE",
        409,
        "Upload is no longer writable",
      );
    }
    throw new AppError(
      "CONFLICT",
      409,
      "Upload is busy; retry shortly",
    );
  }

  let multipartObjects: UploadObjectRow[];
  try {
    multipartObjects = await ensureMultipartUploads(env, operation.upload);
  } catch (error) {
    await releaseUploadOperation(env, upload.id, operation.token);
    throw error;
  }

  const timestamp = nowSeconds();
  const finalized = await env.DB.prepare(
    `UPDATE uploads
     SET status = 'uploading', updated_at = ?1, last_credential_at = ?1,
         operation_token = NULL, operation_kind = NULL,
         operation_expires_at = NULL
     WHERE id = ?2 AND status IN ('created', 'uploading')
       AND operation_token = ?3
       AND EXISTS (
         SELECT 1
         FROM projects p
         JOIN devices d ON d.id = uploads.device_id
         WHERE p.id = uploads.project_id
           AND p.active = 1
           AND p.consent_policy_version = uploads.consent_policy_version
           AND d.accepted_consent_policy_version = p.consent_policy_version
           AND d.revoked_at IS NULL
       )`,
  )
    .bind(timestamp, upload.id, operation.token)
    .run();
  if ((finalized.meta.changes ?? 0) !== 1) {
    await abortMultipartUploads(env, upload.id);
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Upload changed state during credential allocation",
    );
  }
  return {
    upload_id: upload.id,
    status: "uploading",
    object_prefix: upload.archive_prefix,
    multipart_objects: multipartObjects.map((object) => ({
      key: object.object_key,
      upload_id: object.r2_multipart_id,
      part_size: object.part_size,
    })),
  };
}

async function retireExpiredUploadAttempt(
  env: Env,
  upload: UploadRow,
): Promise<void> {
  const timestamp = nowSeconds();
  const token = crypto.randomUUID();
  const claimed = await env.DB.prepare(
    `UPDATE uploads
     SET operation_token = ?1, operation_kind = 'purge',
         operation_expires_at = ?2, updated_at = ?3
     WHERE id = ?4 AND status = 'expired'
       AND (operation_token IS NULL OR operation_expires_at <= ?3)
     RETURNING id`,
  )
    .bind(token, timestamp + INITIALIZE_LEASE_SECONDS, timestamp, upload.id)
    .first<{ id: string }>();
  if (!claimed) {
    throw new AppError(
      "CONFLICT",
      409,
      "Expired upload cleanup is already in progress; retry shortly",
    );
  }
  try {
    await abortMultipartUploads(env, upload.id);
    await deletePrefix(env.ARCHIVE, upload.archive_prefix);
    await env.ARCHIVE.delete(archiveManifestKey(upload));
  } catch {
    await releaseUploadOperation(env, upload.id, token);
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Expired upload cleanup must finish before retry",
    );
  }

  try {
    const retired = await env.DB.prepare(
      `UPDATE uploads
       SET request_hash = request_hash || ':expired:' || id,
           purged_at = ?1, updated_at = ?1,
           operation_token = NULL, operation_kind = NULL,
           operation_expires_at = NULL
       WHERE id = ?2 AND status = 'expired' AND operation_token = ?3`,
    )
      .bind(nowSeconds(), upload.id, token)
      .run();
    if ((retired.meta.changes ?? 0) !== 1) throw new Error("purge lease lost");
  } catch {
    throw new AppError(
      "CONFLICT",
      409,
      "Expired upload changed state during cleanup",
    );
  }
}

export async function enroll(
  env: Env,
  input: EnrollRequest,
): Promise<Record<string, unknown>> {
  const inviteHash = await sha256Hex(input.invite_code);
  const deviceTokenHash = await sha256Hex(input.device_token);

  const findReplay = async (): Promise<EnrollmentRow | null> =>
    env.DB.prepare(
      `SELECT d.id AS device_id,
              d.enrollment_id,
              d.token_hash,
              d.revoked_at,
              d.site_id,
              d.project_id,
              p.name AS project_name,
              d.accepted_consent_policy_version,
              s.pseudonym_key_ciphertext
       FROM devices d
       JOIN invites i ON i.id = d.invite_id
       JOIN projects p ON p.id = d.project_id
       JOIN sites s ON s.id = d.site_id
       WHERE i.code_hash = ?1
         AND d.enrollment_id = ?2
       LIMIT 1`,
    )
      .bind(inviteHash, input.enrollment_id)
      .first<EnrollmentRow>();

  const replayResponse = async (
    existing: EnrollmentRow,
  ): Promise<Record<string, unknown> | null> => {
    if (
      existing.revoked_at !== null ||
      !(await constantTimeEqual(existing.token_hash, deviceTokenHash))
    ) {
      return null;
    }
    const siteKey = await decryptSiteKey(
      existing.pseudonym_key_ciphertext,
      existing.site_id,
      env.SITE_KEY_ENCRYPTION_KEY_B64,
    );
    return {
      enrollment_id: existing.enrollment_id,
      device_token: input.device_token,
      device_id: existing.device_id,
      site_id: existing.site_id,
      project_id: existing.project_id,
      project_name: existing.project_name,
      consent_policy_version: existing.accepted_consent_policy_version,
      pseudonym_key_b64: pseudonymKeyBase64(siteKey),
    };
  };

  const existing = await findReplay();
  if (existing) {
    const response = await replayResponse(existing);
    if (response) return response;
    throw new AppError(
      "INVALID_INVITE",
      401,
      "Invite or enrollment operation is invalid",
    );
  }

  const invite = await env.DB.prepare(
    `SELECT i.id,
            i.site_id,
            i.project_id,
            i.expires_at,
            i.max_uses,
            i.uses,
            i.revoked_at,
            p.name AS project_name,
            p.consent_policy_version,
            p.active AS project_active,
            s.pseudonym_key_ciphertext
     FROM invites i
     JOIN projects p ON p.id = i.project_id
     JOIN sites s ON s.id = i.site_id
     WHERE i.code_hash = ?1
     LIMIT 1`,
  )
    .bind(inviteHash)
    .first<InviteRow>();

  const timestamp = nowSeconds();
  if (
    !invite ||
    invite.revoked_at !== null ||
    invite.project_active !== 1 ||
    invite.expires_at <= timestamp ||
    invite.uses >= invite.max_uses
  ) {
    throw new AppError(
      "INVALID_INVITE",
      401,
      "Invite or enrollment operation is invalid",
    );
  }

  const siteKey = await decryptSiteKey(
    invite.pseudonym_key_ciphertext,
    invite.site_id,
    env.SITE_KEY_ENCRYPTION_KEY_B64,
  );
  const deviceId = crypto.randomUUID();

  try {
    await env.DB.batch([
      env.DB.prepare(
        `INSERT INTO devices
           (id, enrollment_id, invite_id, site_id, project_id, token_hash,
            device_name, platform, client_version,
            accepted_consent_policy_version, created_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)`,
      ).bind(
        deviceId,
        input.enrollment_id,
        invite.id,
        invite.site_id,
        invite.project_id,
        deviceTokenHash,
        input.device_name,
        input.platform,
        input.client_version,
        invite.consent_policy_version,
        timestamp,
      ),
      auditStatement(env, "device.enrolled", {
        siteId: invite.site_id,
        projectId: invite.project_id,
        deviceId,
        subjectType: "device",
        subjectId: deviceId,
        createdAt: timestamp,
      }),
    ]);
  } catch {
    // A concurrent copy of the same request may have committed after our
    // preflight read but before this insert. Recover only when all three
    // replay bindings (invite, enrollment UUID, and token hash) match.
    const raced = await findReplay();
    if (raced) {
      const response = await replayResponse(raced);
      if (response) return response;
    }
    throw new AppError(
      "INVALID_INVITE",
      401,
      "Invite or enrollment operation is invalid",
    );
  }

  return {
    enrollment_id: input.enrollment_id,
    device_token: input.device_token,
    device_id: deviceId,
    site_id: invite.site_id,
    project_id: invite.project_id,
    project_name: invite.project_name,
    consent_policy_version: invite.consent_policy_version,
    pseudonym_key_b64: pseudonymKeyBase64(siteKey),
  };
}

export async function createUpload(
  request: Request,
  env: Env,
  input: CreateUploadRequest,
): Promise<{ body: Record<string, unknown>; created: boolean }> {
  const device = await authenticateDevice(request, env);
  const bundlesWithHashes = await Promise.all(
    input.bundles.map(async (descriptor) => ({
      descriptor,
      hash: await bundleHash(descriptor),
    })),
  );
  bundlesWithHashes.sort((left, right) =>
    left.descriptor.bundle_id < right.descriptor.bundle_id
      ? -1
      : left.descriptor.bundle_id > right.descriptor.bundle_id
        ? 1
        : 0,
  );
  const requestDedupKeys = new Set<string>();
  for (const { descriptor, hash } of bundlesWithHashes) {
    const dedupKey = `${descriptor.series_id}\0${hash}`;
    if (requestDedupKeys.has(dedupKey)) {
      throw new AppError(
        "DUPLICATE_BUNDLE",
        409,
        "Upload request repeats a series bundle",
        {
          bundle_id: descriptor.bundle_id,
          series_id: descriptor.series_id,
        },
      );
    }
    requestDedupKeys.add(dedupKey);
  }
  const requestHash = await sha256Hex(
    canonicalJson({
      client_version: input.client_version,
      bundles: bundlesWithHashes.map(({ descriptor, hash }) => ({
        ...descriptor,
        bundle_hash: hash,
      })),
    }),
  );

  const existing = await env.DB.prepare(
    "SELECT * FROM uploads WHERE device_id = ?1 AND request_hash = ?2 LIMIT 1",
  )
    .bind(device.id, requestHash)
    .first<UploadRow>();
  if (existing && existing.status !== "expired") {
    return {
      body: await createCredentialsResponse(env, existing),
      created: false,
    };
  }
  if (existing?.status === "expired") {
    await retireExpiredUploadAttempt(env, existing);
  }

  const timestamp = nowSeconds();
  await env.DB.prepare(
    `UPDATE uploads
     SET status = 'expired', updated_at = ?1
     WHERE device_id = ?2 AND status IN ('created', 'uploading')
       AND expires_at <= ?1
       AND (operation_token IS NULL OR operation_expires_at <= ?1)`,
  )
    .bind(timestamp, device.id)
    .run();
  const activeUpload = await env.DB.prepare(
    `SELECT id FROM uploads
     WHERE device_id = ?1 AND status IN ('created', 'uploading')
     LIMIT 1`,
  )
    .bind(device.id)
    .first<{ id: string }>();
  if (activeUpload) {
    throw new AppError(
      "CONFLICT",
      409,
      "This device already has an active upload; resume it before starting another",
      { upload_id: activeUpload.id },
    );
  }

  const duplicates = await activeDuplicates(env, device, bundlesWithHashes);
  const exactDuplicate = bundlesWithHashes.find(({ descriptor }) =>
    duplicates.some((row) => row.bundle_id === descriptor.bundle_id),
  );
  if (exactDuplicate) {
    throw new AppError(
      "DUPLICATE_BUNDLE",
      409,
      "Series bundle is already cataloged or withdrawn",
      {
        bundle_id: exactDuplicate.descriptor.bundle_id,
        series_id: exactDuplicate.descriptor.series_id,
      },
    );
  }

  const uploadId = crypto.randomUUID();
  const archivePrefix = `archive/v1/${device.site_id}/${device.project_id}/${uploadId}/`;
  const expiresAt = timestamp + uploadTtl(env);
  const totalBytes = input.bundles.reduce(
    (sum, bundle) => sum + bundle.nii.size + bundle.metadata.size,
    0,
  );

  const statements: D1PreparedStatement[] = [
    env.DB.prepare(
      `INSERT INTO uploads
         (id, site_id, project_id, device_id, status, archive_prefix, request_hash,
          client_version, consent_policy_version, series_count, total_bytes,
          created_at, updated_at, expires_at)
       VALUES (?1, ?2, ?3, ?4, 'created', ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12)`,
    ).bind(
      uploadId,
      device.site_id,
      device.project_id,
      device.id,
      archivePrefix,
      requestHash,
      input.client_version,
      device.current_consent_policy_version,
      input.bundles.length,
      totalBytes,
      timestamp,
      expiresAt,
    ),
  ];
  for (const { descriptor, hash } of bundlesWithHashes) {
    statements.push(
      env.DB.prepare(
        `INSERT INTO upload_bundles
           (upload_id, bundle_id, series_id, subject_id, session_id,
            protocol_group_id, bundle_hash,
            nii_relative_key, nii_size, nii_sha256, nii_uncompressed_sha256,
            metadata_relative_key, metadata_size, metadata_sha256)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)`,
      ).bind(
        uploadId,
        descriptor.bundle_id,
        descriptor.series_id,
        descriptor.subject_id,
        descriptor.session_id,
        descriptor.protocol_group_id,
        hash,
        descriptor.nii.relative_key,
        descriptor.nii.size,
        descriptor.nii.sha256,
        descriptor.nii.uncompressed_sha256,
        descriptor.metadata.relative_key,
        descriptor.metadata.size,
        descriptor.metadata.sha256,
      ),
    );
  }
  statements.push(
    env.DB.prepare(
      `INSERT INTO upload_objects
         (upload_id, object_key, bundle_id, kind, expected_size, expected_sha256)
       SELECT ub.upload_id,
              u.archive_prefix || ub.nii_relative_key,
              ub.bundle_id,
              'nii',
              ub.nii_size,
              ub.nii_sha256
       FROM upload_bundles ub
       JOIN uploads u ON u.id = ub.upload_id
       WHERE ub.upload_id = ?1
       UNION ALL
       SELECT ub.upload_id,
              u.archive_prefix || ub.metadata_relative_key,
              ub.bundle_id,
              'metadata',
              ub.metadata_size,
              ub.metadata_sha256
       FROM upload_bundles ub
       JOIN uploads u ON u.id = ub.upload_id
       WHERE ub.upload_id = ?1`,
    ).bind(uploadId),
  );
  statements.push(
    auditStatement(env, "upload.created", {
      siteId: device.site_id,
      projectId: device.project_id,
      deviceId: device.id,
      uploadId,
      subjectType: "upload",
      subjectId: uploadId,
      createdAt: timestamp,
    }),
  );

  try {
    await env.DB.batch(statements);
  } catch {
    const raced = await env.DB.prepare(
      "SELECT * FROM uploads WHERE device_id = ?1 AND request_hash = ?2 LIMIT 1",
    )
      .bind(device.id, requestHash)
      .first<UploadRow>();
    if (raced)
      return {
        body: await createCredentialsResponse(env, raced),
        created: false,
      };
    throw new AppError("CONFLICT", 409, "Unable to allocate upload");
  }

  const upload = await getUploadForDevice(env, uploadId, device.id);
  return { body: await createCredentialsResponse(env, upload), created: true };
}

export async function refreshUploadCredentials(
  request: Request,
  env: Env,
  uploadId: string,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  const upload = await getUploadForDevice(env, uploadId, device.id);
  return createCredentialsResponse(env, upload);
}

export async function createUploadPartUrl(
  request: Request,
  env: Env,
  uploadId: string,
  input: SignPartRequest,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  const upload = await getUploadForDevice(env, uploadId, device.id);
  const timestamp = nowSeconds();
  if (
    upload.status === "committed" ||
    upload.status === "expired" ||
    upload.status === "withdrawn" ||
    upload.expires_at <= timestamp
  ) {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Upload is no longer writable",
    );
  }
  if (
    upload.operation_token !== null &&
    upload.operation_expires_at !== null &&
    upload.operation_expires_at > timestamp
  ) {
    throw new AppError("CONFLICT", 409, "Upload is busy; retry shortly");
  }

  const object = await env.DB.prepare(
    `SELECT * FROM upload_objects
     WHERE upload_id = ?1 AND object_key = ?2
     LIMIT 1`,
  )
    .bind(upload.id, input.key)
    .first<UploadObjectRow>();
  if (!object || !object.r2_multipart_id || !object.part_size) {
    throw new AppError(
      "OBJECT_MISSING",
      404,
      "Allocated multipart object was not found",
    );
  }
  if (object.completed_at !== null) {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Object is already complete",
    );
  }
  const partCount = Math.ceil(object.expected_size / object.part_size);
  if (input.part_number > partCount) {
    throw new AppError(
      "OBJECT_MISMATCH",
      409,
      "Part number exceeds the allocated object",
    );
  }
  const expectedPartSize =
    input.part_number === partCount
      ? object.expected_size - object.part_size * (partCount - 1)
      : object.part_size;
  if (input.size !== expectedPartSize) {
    throw new AppError(
      "OBJECT_MISMATCH",
      409,
      "Part size does not match the allocated object",
    );
  }
  return { ...(await signR2UploadPart(env, {
    key: object.object_key,
    uploadId: object.r2_multipart_id,
    partNumber: input.part_number,
    size: input.size,
    sha256: input.sha256,
  })) };
}

export async function getUploadStatus(
  request: Request,
  env: Env,
  uploadId: string,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  const upload = await getUploadForDevice(env, uploadId, device.id);
  return uploadStatusResponse(upload);
}

function expectedObjects(
  upload: UploadRow,
  bundles: BundleRow[],
): ExpectedObject[] {
  return bundles.flatMap((bundle) => [
    {
      key: `${upload.archive_prefix}${bundle.nii_relative_key}`,
      size: bundle.nii_size,
      sha256: bundle.nii_sha256,
      bundle_id: bundle.bundle_id,
      kind: "nii" as const,
    },
    {
      key: `${upload.archive_prefix}${bundle.metadata_relative_key}`,
      size: bundle.metadata_size,
      sha256: bundle.metadata_sha256,
      bundle_id: bundle.bundle_id,
      kind: "metadata" as const,
    },
  ]);
}

async function verifyObjects(
  env: Env,
  upload: UploadRow,
  bundles: BundleRow[],
  expected: ExpectedObject[],
  input: CompleteUploadRequest,
): Promise<VerifiedObject[]> {
  if (input.objects.length !== expected.length) {
    throw new AppError(
      "OBJECT_MISMATCH",
      409,
      "Completion must list every expected object exactly once",
    );
  }
  const declared = new Map(input.objects.map((object) => [object.key, object]));
  const objectResult = await env.DB.prepare(
    "SELECT * FROM upload_objects WHERE upload_id = ?1 ORDER BY object_key",
  )
    .bind(upload.id)
    .all<UploadObjectRow>();
  const rows = new Map(
    objectResult.results.map((object) => [object.object_key, object]),
  );
  const bundlesById = new Map(
    bundles.map((bundle) => [bundle.bundle_id, bundle]),
  );
  if (rows.size !== expected.length) {
    throw new AppError(
      "INTERNAL",
      500,
      "Multipart object catalog is incomplete",
    );
  }

  const pending = expected.map((item) => {
    const clientObject = declared.get(item.key);
    const row = rows.get(item.key);
    if (!clientObject) {
      throw new AppError(
        "OBJECT_MISSING",
        409,
        "Expected multipart object was not declared",
        {
          key: item.key,
        },
      );
    }
    if (
      clientObject.size !== item.size ||
      clientObject.sha256 !== item.sha256
    ) {
      throw new AppError(
        "OBJECT_MISMATCH",
        409,
        "Declared object does not match its bundle manifest",
        {
          key: item.key,
        },
      );
    }
    if (
      !row ||
      row.expected_size !== item.size ||
      row.expected_sha256 !== item.sha256 ||
      !row.r2_multipart_id ||
      !row.part_size
    ) {
      throw new AppError(
        "INTERNAL",
        500,
        "Multipart object state is incomplete",
      );
    }
    const expectedPartCount = Math.ceil(item.size / row.part_size);
    if (clientObject.parts.length !== expectedPartCount) {
      throw new AppError(
        "OBJECT_MISMATCH",
        409,
        "Multipart receipt count does not match object size",
        {
          key: item.key,
        },
      );
    }
    return { item, clientObject, row };
  });
  if (declared.size !== expected.length) {
    throw new AppError(
      "OBJECT_MISMATCH",
      409,
      "Completion contains an unexpected object",
    );
  }

  const verified: VerifiedObject[] = [];
  for (let start = 0; start < pending.length; start += 4) {
    const chunk = pending.slice(start, start + 4);
    const results = await Promise.all(
      chunk.map(
        async ({ item, clientObject, row }): Promise<VerifiedObject> => {
          let head: R2Object | null;
          if (row.completed_at === null) {
            try {
              const multipart = env.ARCHIVE.resumeMultipartUpload(
                item.key,
                row.r2_multipart_id as string,
              );
              await multipart.complete(
                clientObject.parts.map((part) => ({
                  partNumber: part.part_number,
                  etag: stripEtag(part.etag),
                })),
              );
            } catch {
              // An identical retry may arrive after R2 committed the object
              // but before D1 checkpointing. The authoritative HEAD below
              // distinguishes that case from a transient completion failure.
            }
            // Cloudflare's live multipart complete result does not reliably
            // populate customMetadata. The persisted object HEAD is the
            // authoritative verification boundary after both a successful
            // completion and an idempotent already-completed retry.
            try {
              head = await env.ARCHIVE.head(item.key);
            } catch {
              throw new AppError(
                "STORAGE_UNAVAILABLE",
                502,
                "Unable to complete multipart objects",
              );
            }
          } else {
            try {
              head = await env.ARCHIVE.head(item.key);
            } catch {
              throw new AppError(
                "STORAGE_UNAVAILABLE",
                502,
                "Unable to verify archive objects",
              );
            }
          }
          if (!head) {
            throw new AppError(
              "STORAGE_UNAVAILABLE",
              502,
              "Persisted archive object is temporarily unavailable",
            );
          }
          const metadataHash = head.customMetadata?.sha256;
          const metadataUploadId =
            head.customMetadata?.upload_id ??
            head.customMetadata?.["upload-id"];
          if (
            head.size !== item.size ||
            metadataHash !== item.sha256 ||
            metadataUploadId !== upload.id
          ) {
            throw new StoredObjectValidationError(
              "Stored object metadata does not match",
              {
                key: item.key,
              },
            );
          }

          // The custom metadata above is server-owned, but it is initialized
          // from the client's declaration. Read the completed object back and
          // independently hash its bytes before allowing a catalog commit.
          // DigestStream performs this incrementally, so large EPI objects are
          // never buffered in Worker memory.
          let stored: R2ObjectBody | null;
          try {
            stored = await env.ARCHIVE.get(item.key);
          } catch {
            throw new AppError(
              "STORAGE_UNAVAILABLE",
              502,
              "Unable to verify archived object bytes",
            );
          }
          if (!stored || stored.size !== item.size) {
            throw new StoredObjectValidationError(
              "Stored object size does not match",
              { key: item.key },
            );
          }
          let storedSha256: string;
          let metadataBytes: Uint8Array<ArrayBuffer> | undefined;
          let nifti: NiftiFacts | undefined;
          if (item.kind === "metadata") {
            try {
              metadataBytes = new Uint8Array(await stored.arrayBuffer());
              storedSha256 = await sha256Hex(metadataBytes);
            } catch {
              throw new AppError(
                "STORAGE_UNAVAILABLE",
                502,
                "Unable to verify archived object bytes",
              );
            }
          } else {
            const bundle = bundlesById.get(item.bundle_id);
            if (!bundle) {
              throw new AppError(
                "INTERNAL",
                500,
                "NIfTI bundle index is incomplete",
              );
            }
            const [compressedBody, niftiBody] = stored.body.tee();
            try {
              [storedSha256, nifti] = await Promise.all([
                sha256StreamHex(compressedBody),
                inspectGzipNifti(
                  niftiBody,
                  bundle.nii_uncompressed_sha256,
                ),
              ]);
            } catch {
              throw new StoredObjectValidationError(
                "Stored NIfTI gzip or header is invalid",
                { key: item.key },
              );
            }
          }
          if (storedSha256 !== item.sha256) {
            throw new StoredObjectValidationError(
              "Stored object checksum does not match",
              { key: item.key },
            );
          }
          let sidecar: ValidatedSidecar | undefined;
          if (item.kind === "metadata") {
            const bundle = bundlesById.get(item.bundle_id);
            if (!bundle || !metadataBytes) {
              throw new AppError(
                "INTERNAL",
                500,
                "Metadata bundle index is incomplete",
              );
            }
            try {
              sidecar = validateSidecarBytes(metadataBytes, {
                bundle_id: bundle.bundle_id,
                series_id: bundle.series_id,
                subject_id: bundle.subject_id,
                session_id: bundle.session_id,
                protocol_group_id: bundle.protocol_group_id,
                client_version: upload.client_version,
                nii_relative_key: bundle.nii_relative_key,
                nii_size: bundle.nii_size,
                nii_sha256: bundle.nii_sha256,
                nii_uncompressed_sha256: bundle.nii_uncompressed_sha256,
              });
            } catch (error) {
              if (error instanceof AppError && error.code === "OBJECT_MISMATCH") {
                throw new StoredObjectValidationError(error.message, {
                  key: item.key,
                });
              }
              throw error;
            }
          }
          await env.DB.prepare(
            `UPDATE upload_objects
           SET completed_at = COALESCE(completed_at, ?1), etag = ?2
           WHERE upload_id = ?3 AND object_key = ?4`,
          )
            .bind(nowSeconds(), head.etag, upload.id, item.key)
            .run();
          const verifiedObject: VerifiedObject = { ...item, etag: head.etag };
          if (nifti) verifiedObject.nifti = nifti;
          if (sidecar) verifiedObject.sidecar = sidecar;
          return verifiedObject;
        },
      ),
    );
    verified.push(...results);
  }

  const sorted = verified.sort((left, right) =>
    left.key < right.key ? -1 : left.key > right.key ? 1 : 0,
  );
  for (const bundle of bundles) {
    const nifti = sorted.find(
      (object) => object.bundle_id === bundle.bundle_id && object.kind === "nii",
    )?.nifti;
    const sidecar = sorted.find(
      (object) =>
        object.bundle_id === bundle.bundle_id && object.kind === "metadata",
    )?.sidecar;
    if (!nifti || !sidecar) {
      throw new AppError("INTERNAL", 500, "Verified bundle facts are incomplete");
    }
    try {
      assertNiftiMatchesSidecar(nifti, sidecar.image);
    } catch {
      throw new StoredObjectValidationError(
        "NIfTI header does not match its metadata sidecar",
        { bundle_id: bundle.bundle_id },
      );
    }
  }
  return sorted;
}

async function rejectStoredUpload(
  env: Env,
  upload: UploadRow,
  operationToken: string,
): Promise<void> {
  const timestamp = nowSeconds();
  const rejected = await env.DB.prepare(
    `UPDATE uploads
     SET status = 'expired', updated_at = ?1,
         operation_kind = 'purge', operation_expires_at = ?2
     WHERE id = ?3 AND status IN ('created', 'uploading')
       AND operation_token = ?4`,
  )
    .bind(
      timestamp,
      timestamp + INITIALIZE_LEASE_SECONDS,
      upload.id,
      operationToken,
    )
    .run();
  if ((rejected.meta.changes ?? 0) !== 1) return;

  await auditStatement(env, "upload.rejected", {
    siteId: upload.site_id,
    projectId: upload.project_id,
    deviceId: upload.device_id,
    uploadId: upload.id,
    subjectType: "upload",
    subjectId: upload.id,
    detailCode: "stored_object_validation_failed",
    createdAt: timestamp,
  }).run();

  try {
    await abortMultipartUploads(env, upload.id);
    await deletePrefix(env.ARCHIVE, upload.archive_prefix);
    await env.ARCHIVE.delete(archiveManifestKey(upload));
    await env.DB.prepare(
      `UPDATE uploads
       SET purged_at = ?1, updated_at = ?1,
           operation_token = NULL, operation_kind = NULL,
           operation_expires_at = NULL
       WHERE id = ?2 AND status = 'expired' AND operation_token = ?3`,
    )
      .bind(nowSeconds(), upload.id, operationToken)
      .run();
  } catch {
    console.warn(
      JSON.stringify({
        event: "rejected_upload_cleanup_pending",
        upload_id: upload.id,
      }),
    );
  }
}

export async function completeUpload(
  request: Request,
  env: Env,
  uploadId: string,
  input: CompleteUploadRequest,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  const initialUpload = await getUploadForDevice(env, uploadId, device.id);
  if (initialUpload.status === "committed")
    return uploadStatusResponse(initialUpload);
  if (
    initialUpload.status === "expired" ||
    initialUpload.status === "withdrawn" ||
    initialUpload.expires_at <= nowSeconds()
  ) {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Upload is no longer writable",
    );
  }

  const operation = await claimUploadOperation(
    env,
    initialUpload,
    "verify",
    VERIFY_LEASE_SECONDS,
  );
  if (!operation) {
    const current = await getUploadForDevice(env, uploadId, device.id);
    if (current.status === "committed") return uploadStatusResponse(current);
    if (current.status === "expired" || current.status === "withdrawn") {
      throw new AppError(
        "UPLOAD_NOT_WRITABLE",
        409,
        "Upload is no longer writable",
      );
    }
    throw new AppError(
      "CONFLICT",
      409,
      "Upload verification is already in progress",
    );
  }
  const upload = operation.upload;

  try {
    const bundleResult = await env.DB.prepare(
    "SELECT * FROM upload_bundles WHERE upload_id = ?1 ORDER BY bundle_id",
    )
      .bind(upload.id)
      .all<BundleRow>();
    const bundles = bundleResult.results;
    if (bundles.length !== upload.series_count) {
      throw new AppError("INTERNAL", 500, "Upload catalog is incomplete");
    }

    const verified = await verifyObjects(
      env,
      upload,
      bundles,
      expectedObjects(upload, bundles),
      input,
    );
  let committedAt = nowSeconds();
  const manifestKey = archiveManifestKey(upload);
  const manifest = {
    schema_version: "scaling-neuro.archive-manifest.v1",
    upload_id: upload.id,
    site_id: upload.site_id,
    project_id: upload.project_id,
    consent_policy_version: upload.consent_policy_version,
    archive_prefix: upload.archive_prefix,
    client_version: upload.client_version,
    created_at: iso(upload.created_at),
    committed_at: iso(committedAt),
    bundles: bundles.map((bundle) => {
      const nii = verified.find(
        (object) =>
          object.bundle_id === bundle.bundle_id && object.kind === "nii",
      );
      const metadata = verified.find(
        (object) =>
          object.bundle_id === bundle.bundle_id && object.kind === "metadata",
      );
      if (!nii || !metadata)
        throw new AppError(
          "INTERNAL",
          500,
          "Verified object index is incomplete",
        );
      return {
        bundle_id: bundle.bundle_id,
        series_id: bundle.series_id,
        subject_id: bundle.subject_id,
        session_id: bundle.session_id,
        protocol_group_id: bundle.protocol_group_id,
        bundle_hash: bundle.bundle_hash,
        nii: {
          key: nii.key,
          size: nii.size,
          sha256: nii.sha256,
          uncompressed_sha256: bundle.nii_uncompressed_sha256,
          etag: nii.etag,
        },
        metadata: {
          key: metadata.key,
          size: metadata.size,
          sha256: metadata.sha256,
          etag: metadata.etag,
        },
      };
    }),
    control_plane: { service_version: env.SERVICE_VERSION },
  };
  const manifestJson = canonicalJson(manifest);
  const manifestPayload = `${manifestJson}\n`;
  let manifestSha256 = await sha256Hex(manifestPayload);
  try {
    const stored = await env.ARCHIVE.put(
      manifestKey,
      utf8Bytes(manifestPayload),
      {
        onlyIf: { etagDoesNotMatch: "*" },
        httpMetadata: { contentType: "application/json; charset=utf-8" },
        customMetadata: { upload_id: upload.id, sha256: manifestSha256 },
      },
    );
    if (!stored) {
      const existing = await env.ARCHIVE.get(manifestKey);
      if (!existing) throw new Error("conditional manifest disappeared");
      const existingBytes = await existing.arrayBuffer();
      const existingManifest = JSON.parse(utf8String(existingBytes)) as Record<
        string,
        unknown
      >;
      if (
        existingManifest.schema_version !==
          "scaling-neuro.archive-manifest.v1" ||
        existingManifest.upload_id !== upload.id ||
        existingManifest.site_id !== upload.site_id ||
        existingManifest.project_id !== upload.project_id ||
        existingManifest.archive_prefix !== upload.archive_prefix ||
        !Array.isArray(existingManifest.bundles) ||
        existingManifest.bundles.length !== bundles.length ||
        typeof existingManifest.committed_at !== "string"
      ) {
        throw new Error("existing manifest does not match upload");
      }
      const parsedCommittedAt =
        Date.parse(existingManifest.committed_at) / 1000;
      if (
        !Number.isInteger(parsedCommittedAt) ||
        parsedCommittedAt < upload.created_at
      ) {
        throw new Error("existing manifest timestamp is invalid");
      }
      committedAt = parsedCommittedAt;
      manifestSha256 = await sha256Hex(new Uint8Array(existingBytes));
      if (
        existing.customMetadata?.upload_id !== upload.id ||
        existing.customMetadata?.sha256 !== manifestSha256
      ) {
        throw new Error("existing manifest metadata is invalid");
      }
    }
  } catch {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Unable to write archive manifest",
    );
  }

  try {
    await env.DB.batch([
      env.DB.prepare(
        `INSERT INTO catalog_series
           (id, upload_id, bundle_id, site_id, project_id, series_id, subject_id,
            session_id, protocol_group_id, bundle_hash,
            nii_object_key, nii_size, nii_sha256,
            nii_uncompressed_sha256,
            metadata_object_key, metadata_size, metadata_sha256, committed_at)
         SELECT lower(hex(randomblob(16))),
                ub.upload_id,
                ub.bundle_id,
                u.site_id,
                u.project_id,
                ub.series_id,
                ub.subject_id,
                ub.session_id,
                ub.protocol_group_id,
                ub.bundle_hash,
                u.archive_prefix || ub.nii_relative_key,
                ub.nii_size,
                ub.nii_sha256,
                ub.nii_uncompressed_sha256,
                u.archive_prefix || ub.metadata_relative_key,
                ub.metadata_size,
                ub.metadata_sha256,
                ?1
         FROM upload_bundles ub
         JOIN uploads u ON u.id = ub.upload_id
         JOIN devices d ON d.id = u.device_id
         JOIN projects p ON p.id = u.project_id
         WHERE ub.upload_id = ?2
           AND u.status IN ('created', 'uploading')
           AND u.operation_token = ?3
           AND d.revoked_at IS NULL
           AND p.active = 1
           AND p.consent_policy_version = u.consent_policy_version
           AND d.accepted_consent_policy_version = p.consent_policy_version`,
      ).bind(committedAt, upload.id, operation.token),
      env.DB.prepare(
        `UPDATE uploads
         SET status = 'committed', updated_at = ?1, committed_at = ?1,
             manifest_object_key = ?2, manifest_sha256 = ?3,
             operation_token = NULL, operation_kind = NULL,
             operation_expires_at = NULL
         WHERE id = ?4 AND status IN ('created', 'uploading')
           AND operation_token = ?5
           AND EXISTS (
             SELECT 1
             FROM devices
             JOIN projects ON projects.id = uploads.project_id
             WHERE devices.id = uploads.device_id
               AND devices.revoked_at IS NULL
               AND projects.active = 1
               AND projects.consent_policy_version = uploads.consent_policy_version
               AND devices.accepted_consent_policy_version = projects.consent_policy_version
           )`,
      ).bind(
        committedAt,
        manifestKey,
        manifestSha256,
        upload.id,
        operation.token,
      ),
      env.DB.prepare(
        `INSERT INTO audit_events
           (id, event_type, site_id, project_id, device_id, upload_id,
            subject_type, subject_id, detail_code, created_at)
         SELECT ?1, 'upload.committed', ?2, ?3, ?4, ?5, 'upload', ?5, NULL, ?6
         WHERE EXISTS (
           SELECT 1 FROM uploads
           WHERE id = ?5 AND status = 'committed' AND committed_at = ?6
         )`,
      ).bind(
        crypto.randomUUID(),
        upload.site_id,
        upload.project_id,
        upload.device_id,
        upload.id,
        committedAt,
      ),
    ]);
  } catch {
    const raced = await getUploadForDevice(env, upload.id, device.id);
    if (raced.status === "committed") return uploadStatusResponse(raced);
    try {
      await env.ARCHIVE.delete(manifestKey);
    } catch {
      // Scheduled cleanup removes this uncommitted manifest if deletion is temporarily unavailable.
    }
    throw new AppError(
      "DUPLICATE_BUNDLE",
      409,
      "A series bundle was committed concurrently",
    );
  }

  const committed = await getUploadForDevice(env, upload.id, device.id);
  if (committed.status !== "committed") {
    try {
      await env.ARCHIVE.delete(manifestKey);
    } catch {
      // Scheduled cleanup is the fallback.
    }
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Upload changed state during completion",
    );
  }
    return uploadStatusResponse(committed);
  } catch (error) {
    if (error instanceof StoredObjectValidationError) {
      console.warn(
        JSON.stringify({
          event: "stored_object_validation_failed",
          upload_id: upload.id,
          reason: error.message,
        }),
      );
      await rejectStoredUpload(env, upload, operation.token);
    } else {
      await releaseUploadOperation(env, upload.id, operation.token);
    }
    throw error;
  }
}

export async function createAdminInvite(
  request: Request,
  env: Env,
  input: AdminInviteRequest,
): Promise<Record<string, unknown>> {
  await authenticateAdmin(request, env);
  const timestamp = nowSeconds();
  let site = await env.DB.prepare("SELECT * FROM sites WHERE slug = ?1 LIMIT 1")
    .bind(input.site_slug)
    .first<SiteRow>();
  if (!site) {
    const siteId = crypto.randomUUID();
    const ciphertext = await encryptSiteKey(
      randomBytes(32),
      siteId,
      env.SITE_KEY_ENCRYPTION_KEY_B64,
    );
    try {
      await env.DB.prepare(
        `INSERT INTO sites (id, slug, name, pseudonym_key_ciphertext, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)`,
      )
        .bind(siteId, input.site_slug, input.site_name, ciphertext, timestamp)
        .run();
    } catch {
      // A concurrent admin request may have created the same site.
    }
    site = await env.DB.prepare("SELECT * FROM sites WHERE slug = ?1 LIMIT 1")
      .bind(input.site_slug)
      .first<SiteRow>();
  }
  if (!site) throw new AppError("INTERNAL", 500, "Unable to create site");
  if (site.name !== input.site_name) {
    throw new AppError(
      "CONFLICT",
      409,
      "Site slug already exists with a different name",
    );
  }

  let project = await env.DB.prepare(
    "SELECT * FROM projects WHERE site_id = ?1 AND slug = ?2 LIMIT 1",
  )
    .bind(site.id, input.project_slug)
    .first<ProjectRow>();
  if (!project) {
    const projectId = crypto.randomUUID();
    try {
      await env.DB.prepare(
        `INSERT INTO projects
           (id, site_id, slug, name, consent_policy_version, active, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)`,
      )
        .bind(
          projectId,
          site.id,
          input.project_slug,
          input.project_name,
          input.consent_policy_version,
          timestamp,
        )
        .run();
    } catch {
      // A concurrent admin request may have created the same project.
    }
    project = await env.DB.prepare(
      "SELECT * FROM projects WHERE site_id = ?1 AND slug = ?2 LIMIT 1",
    )
      .bind(site.id, input.project_slug)
      .first<ProjectRow>();
  }
  if (!project) throw new AppError("INTERNAL", 500, "Unable to create project");
  if (
    project.name !== input.project_name ||
    project.consent_policy_version !== input.consent_policy_version ||
    project.active !== 1
  ) {
    throw new AppError(
      "CONFLICT",
      409,
      "Project exists with different settings or is inactive",
    );
  }

  const inviteCode = randomOpaqueToken("sn_invite");
  const inviteId = crypto.randomUUID();
  const expiresAt = timestamp + input.expires_in_seconds;
  await env.DB.batch([
    env.DB.prepare(
      `INSERT INTO invites
         (id, site_id, project_id, code_hash, max_uses, uses, expires_at, created_at)
       VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)`,
    ).bind(
      inviteId,
      site.id,
      project.id,
      await sha256Hex(inviteCode),
      input.max_uses,
      expiresAt,
      timestamp,
    ),
    auditStatement(env, "invite.created", {
      siteId: site.id,
      projectId: project.id,
      subjectType: "invite",
      subjectId: inviteId,
      createdAt: timestamp,
    }),
  ]);

  return {
    invite_id: inviteId,
    invite_code: inviteCode,
    site_id: site.id,
    project_id: project.id,
    expires_at: iso(expiresAt),
    max_uses: input.max_uses,
  };
}

export async function revokeInvite(
  request: Request,
  env: Env,
  inviteId: string,
): Promise<Record<string, unknown>> {
  await authenticateAdmin(request, env);
  const invite = await env.DB.prepare(
    "SELECT id, site_id, project_id, revoked_at FROM invites WHERE id = ?1 LIMIT 1",
  )
    .bind(inviteId)
    .first<{
      id: string;
      site_id: string;
      project_id: string;
      revoked_at: number | null;
    }>();
  if (!invite) throw new AppError("NOT_FOUND", 404, "Invite was not found");
  const timestamp = invite.revoked_at ?? nowSeconds();
  if (invite.revoked_at === null) {
    await env.DB.batch([
      env.DB.prepare(
        "UPDATE invites SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
      ).bind(timestamp, invite.id),
      auditStatement(env, "invite.revoked", {
        siteId: invite.site_id,
        projectId: invite.project_id,
        subjectType: "invite",
        subjectId: invite.id,
        createdAt: timestamp,
      }),
    ]);
  }
  return {
    invite_id: invite.id,
    status: "revoked",
    revoked_at: iso(timestamp),
  };
}

export async function revokeDevice(
  request: Request,
  env: Env,
  deviceId: string,
): Promise<Record<string, unknown>> {
  await authenticateAdmin(request, env);
  const device = await env.DB.prepare(
    "SELECT id, site_id, project_id, revoked_at FROM devices WHERE id = ?1 LIMIT 1",
  )
    .bind(deviceId)
    .first<{
      id: string;
      site_id: string;
      project_id: string;
      revoked_at: number | null;
    }>();
  if (!device) throw new AppError("NOT_FOUND", 404, "Device was not found");
  const timestamp = device.revoked_at ?? nowSeconds();
  if (device.revoked_at === null) {
    await env.DB.batch([
      env.DB.prepare(
        "UPDATE devices SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
      ).bind(timestamp, device.id),
      env.DB.prepare(
        `UPDATE uploads
         SET expires_at = ?1, updated_at = ?1,
             status = CASE
               WHEN operation_token IS NULL OR operation_expires_at <= ?1
               THEN 'expired'
               ELSE status
             END
         WHERE device_id = ?2 AND status IN ('created', 'uploading')`,
      ).bind(timestamp, device.id),
      auditStatement(env, "device.revoked", {
        siteId: device.site_id,
        projectId: device.project_id,
        deviceId: device.id,
        subjectType: "device",
        subjectId: device.id,
        createdAt: timestamp,
      }),
    ]);
  }
  return {
    device_id: device.id,
    status: "revoked",
    revoked_at: iso(timestamp),
  };
}

export async function withdrawUpload(
  request: Request,
  env: Env,
  uploadId: string,
): Promise<Record<string, unknown>> {
  await authenticateAdmin(request, env);
  let upload = await env.DB.prepare(
    "SELECT * FROM uploads WHERE id = ?1 LIMIT 1",
  )
    .bind(uploadId)
    .first<UploadRow>();
  if (!upload) throw new AppError("NOT_FOUND", 404, "Upload was not found");
  const timestamp = upload.withdrawn_at ?? nowSeconds();
  await env.DB.batch([
    env.DB.prepare(
      `UPDATE uploads
       SET status = 'withdrawn', withdrawn_at = ?1, updated_at = ?1,
           operation_token = NULL, operation_kind = NULL,
           operation_expires_at = NULL
       WHERE id = ?2 AND status != 'withdrawn'
         AND (operation_token IS NULL OR operation_expires_at <= ?1)`,
    )
      .bind(timestamp, upload.id),
    env.DB.prepare(
      `UPDATE catalog_series
       SET withdrawn_at = (
         SELECT withdrawn_at FROM uploads WHERE id = ?1
       )
       WHERE upload_id = ?1 AND withdrawn_at IS NULL
         AND EXISTS (
           SELECT 1 FROM uploads WHERE id = ?1 AND status = 'withdrawn'
         )`,
    ).bind(upload.id),
    env.DB.prepare(
      `INSERT INTO audit_events
         (id, event_type, site_id, project_id, device_id, upload_id,
          subject_type, subject_id, detail_code, created_at)
       SELECT ?1, 'upload.withdrawn', site_id, project_id, device_id, id,
              'upload', id, NULL, withdrawn_at
       FROM uploads
       WHERE id = ?2 AND status = 'withdrawn'
         AND NOT EXISTS (
           SELECT 1 FROM audit_events
           WHERE event_type = 'upload.withdrawn' AND upload_id = ?2
         )`,
    ).bind(crypto.randomUUID(), upload.id),
  ]);
  upload = (await env.DB.prepare(
    "SELECT * FROM uploads WHERE id = ?1 LIMIT 1",
  )
    .bind(upload.id)
    .first<UploadRow>()) as UploadRow;
  if (upload.status !== "withdrawn") {
    throw new AppError(
      "CONFLICT",
      409,
      "Upload is busy; retry withdrawal shortly",
    );
  }

  if (upload.purged_at === null) {
    try {
      await abortMultipartUploads(env, upload.id);
      await deletePrefix(env.ARCHIVE, upload.archive_prefix);
      await env.ARCHIVE.delete(archiveManifestKey(upload));
      await env.DB.prepare(
        "UPDATE uploads SET purged_at = ?1, updated_at = ?1 WHERE id = ?2",
      )
        .bind(nowSeconds(), upload.id)
        .run();
    } catch {
      throw new AppError(
        "STORAGE_UNAVAILABLE",
        502,
        "Upload is withdrawn but archive deletion is pending retry",
      );
    }
  }
  return {
    upload_id: upload.id,
    status: "withdrawn",
    withdrawn_at: iso(timestamp),
  };
}

export async function cleanupAbandoned(env: Env): Promise<void> {
  const timestamp = nowSeconds();
  const purgeClaims: Array<{ upload: UploadRow; token: string }> = [];
  const activeCandidates = await env.DB.prepare(
    `SELECT id FROM uploads
     WHERE status IN ('created', 'uploading')
       AND expires_at <= ?1
       AND (operation_token IS NULL OR operation_expires_at <= ?1)
     ORDER BY expires_at
     LIMIT 50`,
  )
    .bind(timestamp)
    .all<{ id: string }>();
  for (const candidate of activeCandidates.results) {
    const token = crypto.randomUUID();
    const claimed = await env.DB.prepare(
      `UPDATE uploads
       SET status = 'expired', updated_at = ?1,
           operation_token = ?2, operation_kind = 'purge',
           operation_expires_at = ?3
       WHERE id = ?4 AND status IN ('created', 'uploading')
         AND expires_at <= ?1
         AND (operation_token IS NULL OR operation_expires_at <= ?1)
       RETURNING *`,
    )
      .bind(
        timestamp,
        token,
        timestamp + INITIALIZE_LEASE_SECONDS,
        candidate.id,
      )
      .first<UploadRow>();
    if (claimed) purgeClaims.push({ upload: claimed, token });
  }

  const terminalCandidates = await env.DB.prepare(
    `SELECT id FROM uploads
     WHERE status IN ('expired', 'withdrawn') AND purged_at IS NULL
       AND (operation_token IS NULL OR operation_expires_at <= ?1)
     ORDER BY updated_at
     LIMIT 50`,
  )
    .bind(timestamp)
    .all<{ id: string }>();
  for (const candidate of terminalCandidates.results) {
    const token = crypto.randomUUID();
    const claimed = await env.DB.prepare(
      `UPDATE uploads
       SET operation_token = ?1, operation_kind = 'purge',
           operation_expires_at = ?2, updated_at = ?3
       WHERE id = ?4 AND status IN ('expired', 'withdrawn')
         AND purged_at IS NULL
         AND (operation_token IS NULL OR operation_expires_at <= ?3)
       RETURNING *`,
    )
      .bind(
        token,
        timestamp + INITIALIZE_LEASE_SECONDS,
        timestamp,
        candidate.id,
      )
      .first<UploadRow>();
    if (claimed) purgeClaims.push({ upload: claimed, token });
  }

  for (const { upload, token } of purgeClaims) {
    try {
      await abortMultipartUploads(env, upload.id);
      await deletePrefix(env.ARCHIVE, upload.archive_prefix);
      await env.ARCHIVE.delete(archiveManifestKey(upload));
      const purged = await env.DB.prepare(
        `UPDATE uploads
         SET purged_at = ?1, updated_at = ?1,
             operation_token = NULL, operation_kind = NULL,
             operation_expires_at = NULL
         WHERE id = ?2 AND operation_token = ?3
           AND status IN ('expired', 'withdrawn')`,
      )
        .bind(timestamp, upload.id, token)
        .run();
      if ((purged.meta.changes ?? 0) === 1 && upload.status === "expired") {
        await auditStatement(env, "upload.expired", {
          siteId: upload.site_id,
          projectId: upload.project_id,
          deviceId: upload.device_id,
          uploadId: upload.id,
          subjectType: "upload",
          subjectId: upload.id,
          createdAt: timestamp,
        }).run();
      }
    } catch {
      console.warn(
        JSON.stringify({
          event: "cleanup_failed",
          upload_id: upload.id,
          status: upload.status,
        }),
      );
    }
  }
}

export async function adminCleanup(
  request: Request,
  env: Env,
): Promise<Record<string, unknown>> {
  await authenticateAdmin(request, env);
  await cleanupAbandoned(env);
  return { status: "ok" };
}

export async function health(env: Env): Promise<Record<string, unknown>> {
  try {
    const result = await env.DB.prepare("SELECT 1 AS ok").first<{
      ok: number;
    }>();
    if (result?.ok !== 1 || !env.ARCHIVE)
      throw new Error("binding unavailable");
  } catch {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      503,
      "Control-plane storage is unavailable",
    );
  }
  return {
    status: "ok",
    service: "scaling-neuro-ingest",
    version: env.SERVICE_VERSION,
  };
}
