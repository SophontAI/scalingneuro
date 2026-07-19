import { authenticateAdmin, authenticateDevice } from "./auth";
import {
  canonicalJson,
  constantTimeEqual,
  decryptRegistrationEmail,
  decryptSiteKey,
  encryptRegistrationEmail,
  encryptSiteKey,
  pseudonymKeyBase64,
  randomBytes,
  randomOpaqueToken,
  sha256Hex,
  sha256PassThrough,
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
  deleteObject,
  deletePrefix,
  presignUploadPart as signR2UploadPart,
  uploadTtl,
} from "./r2";
import {
  ACTIVE_METADATA_POLICY_ID,
  ACTIVE_METADATA_POLICY_VERSION,
  validateSidecarBytes,
  type ValidatedSidecar,
} from "./sidecar";
import type {
  AdminInviteRequest,
  BundleDescriptor,
  CompleteUploadRequest,
  CreateUploadRequest,
  EnrollRequest,
  PublicRegistrationRequest,
  SignPartRequest,
} from "./validation";
import packageManifest from "../package.json";

const MINIMUM_CLIENT_VERSION = "0.1.1";
const MINIMUM_SELF_SERVICE_CLIENT_VERSION = "0.2.8";
const LEGACY_UNCAPPED_QUOTA_SENTINEL = Number.MAX_SAFE_INTEGER;
const SERVICE_VERSION = packageManifest.version;
const PUBLIC_PROJECT_NAME = "Scaling Neuro public EPI contribution";
const PUBLIC_PROJECT_SLUG = "public-epi";
const PUBLIC_CONSENT_POLICY_VERSION = "open-epi-1.0.0";

function semanticVersion(
  value: string,
): { core: readonly [number, number, number]; prerelease: boolean } | null {
  const match =
    /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([A-Za-z0-9.-]+))?(?:\+[A-Za-z0-9.-]+)?$/u.exec(
      value,
    );
  if (!match) return null;
  const parts = match.slice(1, 4).map(Number);
  if (parts.some((part) => !Number.isSafeInteger(part))) return null;
  return {
    core: parts as unknown as readonly [number, number, number],
    prerelease: match[4] !== undefined,
  };
}

export function clientVersionAtLeast(
  value: string,
  minimumValue: string,
): boolean {
  const current = semanticVersion(value);
  const minimum = semanticVersion(minimumValue);
  const firstDifference =
    current === null || minimum === null
      ? -2
      : current.core.findIndex((part, index) => part !== minimum.core[index]);
  return (
    current !== null &&
    minimum !== null &&
    ((firstDifference === -1 && (!current.prerelease || minimum.prerelease)) ||
      (firstDifference >= 0 &&
        current.core[firstDifference]! > minimum.core[firstDifference]!))
  );
}

export function clientVersionIsSupported(value: string): boolean {
  return clientVersionAtLeast(value, MINIMUM_CLIENT_VERSION);
}

function requireSupportedClientVersion(value: string): void {
  if (!clientVersionIsSupported(value)) {
    throw new AppError(
      "CLIENT_UPDATE_REQUIRED",
      426,
      "This client is older than the active privacy contract; install the current release",
      { minimum_client_version: MINIMUM_CLIENT_VERSION },
    );
  }
}

function requireSelfServiceClientVersion(value: string): void {
  if (!clientVersionAtLeast(value, MINIMUM_SELF_SERVICE_CLIENT_VERSION)) {
    throw new AppError(
      "CLIENT_UPDATE_REQUIRED",
      426,
      "Install the current client to use open self-service registration",
      { minimum_client_version: MINIMUM_SELF_SERVICE_CLIENT_VERSION },
    );
  }
}

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

interface PublicRegistrationRow extends EnrollmentRow {
  registration_id: string;
  request_hash: string;
}

interface ContributorRegistrationRow {
  id: string;
  site_id: string;
  project_id: string;
  device_id: string;
  email_ciphertext: string;
  contact_name: string;
  institution_name: string;
  institution_ror_id: string | null;
  lab_name: string;
  contact_opt_in: number;
  created_at: number;
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
  ingest_format: "nifti-v1" | "dicom-series-v1";
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
  received_at: number | null;
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
  series_id: string;
  subject_id: string;
  session_id: string;
  protocol_group_id: string;
  bundle_hash: string;
  nii_uncompressed_sha256: string;
  metadata_policy_id: string | null;
  metadata_policy_version: string | null;
  withdrawn_at: number | null;
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
  verified_at: number | null;
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

type VerificationPhase =
  | "finalizing_objects"
  | "validating_scans"
  | "committing_archive";

interface VerificationProgress {
  phase: VerificationPhase;
  finalized_series: number;
  verified_series: number;
  total_series: number;
}

interface CompletionObjectState {
  item: ExpectedObject;
  clientObject: CompleteUploadRequest["objects"][number];
  row: UploadObjectRow;
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
// Each completion request performs one bounded, durable step. Match the
// maximum Pages CPU window so an interrupted step becomes reclaimable without
// leaving the workstation blocked behind a long orphaned lease.
const VERIFY_LEASE_SECONDS = 5 * 60;

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

async function abortDicomMultipartUploads(
  env: Env,
  uploadId: string,
): Promise<void> {
  const result = await env.DB.prepare(
    `SELECT u.archive_prefix, d.archive_relative_key, d.r2_multipart_id
     FROM dicom_upload_series d JOIN uploads u ON u.id = d.upload_id
     WHERE d.upload_id = ?1 AND d.r2_multipart_id IS NOT NULL
       AND d.completed_at IS NULL`,
  )
    .bind(uploadId)
    .all<{
      archive_prefix: string;
      archive_relative_key: string;
      r2_multipart_id: string;
    }>();
  for (let offset = 0; offset < result.results.length; offset += 8) {
    await Promise.all(
      result.results.slice(offset, offset + 8).map(async (row) => {
        try {
          await env.ARCHIVE.resumeMultipartUpload(
            `${row.archive_prefix}${row.archive_relative_key}`,
            row.r2_multipart_id,
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
            "Unable to abort DICOM multipart upload",
          );
        }
      }),
    );
  }
}

async function abortAllMultipartUploads(
  env: Env,
  uploadId: string,
): Promise<void> {
  await Promise.all([
    abortMultipartUploads(env, uploadId),
    abortDicomMultipartUploads(env, uploadId),
  ]);
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

function uploadStatusResponse(
  upload: UploadRow,
  verification?: VerificationProgress,
): Record<string, unknown> {
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
  if (upload.status === "uploading" && verification !== undefined) {
    response.verification = verification;
  }
  return response;
}

async function verificationProgress(
  env: Env,
  upload: UploadRow,
): Promise<VerificationProgress> {
  const result = await env.DB.prepare(
    `SELECT
       (SELECT COUNT(*) FROM upload_bundles b
        WHERE b.upload_id = ?1
          AND 2 = (
            SELECT COUNT(*) FROM upload_objects o
            WHERE o.upload_id = b.upload_id AND o.bundle_id = b.bundle_id
              AND o.completed_at IS NOT NULL AND o.etag IS NOT NULL
          )) AS finalized_series,
       (SELECT COUNT(*) FROM upload_bundles b
        WHERE b.upload_id = ?1
          AND 2 = (
            SELECT COUNT(*) FROM upload_objects o
            WHERE o.upload_id = b.upload_id AND o.bundle_id = b.bundle_id
              AND o.verified_at IS NOT NULL AND o.etag IS NOT NULL
          )) AS verified_series`,
  )
    .bind(upload.id)
    .first<{ finalized_series: number; verified_series: number }>();
  const finalizedSeries = result?.finalized_series ?? 0;
  const verifiedSeries = result?.verified_series ?? 0;
  return {
    phase:
      finalizedSeries < upload.series_count
        ? "finalizing_objects"
        : verifiedSeries < upload.series_count
          ? "validating_scans"
          : "committing_archive",
    finalized_series: finalizedSeries,
    verified_series: verifiedSeries,
    total_series: upload.series_count,
  };
}

async function uploadStatusWithProgress(
  env: Env,
  upload: UploadRow,
): Promise<Record<string, unknown>> {
  return uploadStatusResponse(
    upload,
    upload.status === "uploading"
      ? await verificationProgress(env, upload)
      : undefined,
  );
}

async function catalogBundles(
  env: Env,
  device: DeviceContext,
  bundles: ReadonlyArray<{ descriptor: BundleDescriptor; hash: string }>,
): Promise<CatalogRow[]> {
  return catalogRowsByBundleId(
    env,
    device.site_id,
    device.project_id,
    bundles.map((item) => item.descriptor.bundle_id),
  );
}

async function catalogRowsByBundleId(
  env: Env,
  siteId: string,
  projectId: string,
  requestedBundleIds: readonly string[],
): Promise<CatalogRow[]> {
  const rows: CatalogRow[] = [];
  const uniqueBundleIds = [...new Set(requestedBundleIds)];
  for (let start = 0; start < uniqueBundleIds.length; start += 40) {
    const bundleIds = [...uniqueBundleIds.slice(start, start + 40)];
    const placeholders = bundleIds
      .map((_, index) => `?${index + 3}`)
      .join(", ");
    const result = await env.DB.prepare(
      `SELECT bundle_id, upload_id, series_id, subject_id, session_id,
              protocol_group_id, bundle_hash, nii_uncompressed_sha256,
              metadata_policy_id, metadata_policy_version,
              withdrawn_at
       FROM catalog_series
       WHERE site_id = ?1
         AND project_id = ?2
         AND bundle_id IN (${placeholders})`,
    )
      .bind(siteId, projectId, ...bundleIds)
      .all<CatalogRow>();
    rows.push(...result.results);
  }
  return rows;
}

function existingBundleDetails(row: CatalogRow): Record<string, string> {
  return {
    bundle_id: row.bundle_id,
    series_id: row.series_id,
    subject_id: row.subject_id,
    session_id: row.session_id,
    protocol_group_id: row.protocol_group_id,
    upload_id: row.upload_id,
    nii_uncompressed_sha256: row.nii_uncompressed_sha256,
  };
}

function catalogIdentityMatches(
  row: CatalogRow,
  descriptor: BundleDescriptor,
  hash: string,
): boolean {
  return (
    row.bundle_id === descriptor.bundle_id &&
    row.series_id === descriptor.series_id &&
    row.subject_id === descriptor.subject_id &&
    row.session_id === descriptor.session_id &&
    row.protocol_group_id === descriptor.protocol_group_id &&
    row.bundle_hash === hash &&
    row.nii_uncompressed_sha256 === descriptor.nii.uncompressed_sha256
  );
}

function catalogPrivacyContractMatches(row: CatalogRow): boolean {
  return (
    row.metadata_policy_id === ACTIVE_METADATA_POLICY_ID &&
    row.metadata_policy_version === ACTIVE_METADATA_POLICY_VERSION
  );
}

function catalogRowMatchesCommittedBundle(
  row: CatalogRow,
  bundle: BundleRow,
): boolean {
  return (
    row.bundle_id === bundle.bundle_id &&
    row.series_id === bundle.series_id &&
    row.subject_id === bundle.subject_id &&
    row.session_id === bundle.session_id &&
    row.protocol_group_id === bundle.protocol_group_id &&
    row.bundle_hash === bundle.bundle_hash &&
    row.nii_uncompressed_sha256 === bundle.nii_uncompressed_sha256 &&
    catalogPrivacyContractMatches(row) &&
    row.withdrawn_at === null
  );
}

async function createCredentialsResponse(
  env: Env,
  upload: UploadRow,
): Promise<Record<string, unknown>> {
  requireSupportedClientVersion(upload.client_version);
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
    throw new AppError("CONFLICT", 409, "Upload is busy; retry shortly");
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
    await abortAllMultipartUploads(env, upload.id);
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
    await abortAllMultipartUploads(env, upload.id);
    await deletePrefix(env, upload.archive_prefix);
    await deleteObject(env, archiveManifestKey(upload));
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

async function retireUnsupportedActiveUpload(
  env: Env,
  device: DeviceContext,
): Promise<void> {
  const upload = await env.DB.prepare(
    `SELECT * FROM uploads
     WHERE device_id = ?1 AND ingest_format = 'nifti-v1'
       AND status IN ('created', 'uploading')
     LIMIT 1`,
  )
    .bind(device.id)
    .first<UploadRow>();
  if (!upload || clientVersionIsSupported(upload.client_version)) return;

  const timestamp = nowSeconds();
  const token = crypto.randomUUID();
  const claimed = await env.DB.prepare(
    `UPDATE uploads
     SET operation_token = ?1, operation_kind = 'purge',
         operation_expires_at = ?2, updated_at = ?3
     WHERE id = ?4 AND device_id = ?5
       AND status IN ('created', 'uploading')
       AND (operation_token IS NULL OR operation_expires_at <= ?3)
     RETURNING *`,
  )
    .bind(
      token,
      timestamp + INITIALIZE_LEASE_SECONDS,
      timestamp,
      upload.id,
      device.id,
    )
    .first<UploadRow>();
  if (!claimed) {
    throw new AppError(
      "CONFLICT",
      409,
      "The outdated upload is busy; retry privacy cleanup shortly",
      { upload_id: upload.id },
    );
  }

  try {
    await abortAllMultipartUploads(env, upload.id);
    await deletePrefix(env, upload.archive_prefix);
    await deleteObject(env, archiveManifestKey(upload));
  } catch {
    await releaseUploadOperation(env, upload.id, token);
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "The outdated upload must be purged before a replacement can start",
      { upload_id: upload.id },
    );
  }

  const retiredAt = nowSeconds();
  const retired = await env.DB.prepare(
    `UPDATE uploads
     SET status = 'expired', request_hash = request_hash || ':privacy:' || id,
         purged_at = ?1, updated_at = ?1,
         operation_token = NULL, operation_kind = NULL,
         operation_expires_at = NULL
     WHERE id = ?2 AND device_id = ?3
       AND status IN ('created', 'uploading') AND operation_token = ?4`,
  )
    .bind(retiredAt, upload.id, device.id, token)
    .run();
  if ((retired.meta.changes ?? 0) !== 1) {
    throw new AppError(
      "CONFLICT",
      409,
      "The outdated upload changed state during privacy cleanup",
      { upload_id: upload.id },
    );
  }
  await auditStatement(env, "upload.expired", {
    siteId: upload.site_id,
    projectId: upload.project_id,
    deviceId: upload.device_id,
    uploadId: upload.id,
    subjectType: "upload",
    subjectId: upload.id,
    detailCode: "client_privacy_contract_superseded",
    createdAt: retiredAt,
  }).run();
}

export function publicContributionInfo(
  userAgent: string | null,
): Record<string, unknown> {
  // neuro-sync 0.2.x modeled this field as a required u64 and cannot decode
  // JSON null. Keep the backend project quota truly NULL/unlimited, but give
  // only that exact legacy client family a JSON-safe compatibility sentinel
  // during the two-phase 0.3 release cutover. Browsers and 0.3+ clients see
  // the canonical null contract.
  const legacyQuotaCompatibility =
    userAgent !== null &&
    /^neuro-sync\/0\.2\.(?:0|[1-9]\d*)(?:[-+][A-Za-z0-9.-]+)?$/u.test(
      userAgent,
    );
  return {
    registration_open: true,
    project_name: PUBLIC_PROJECT_NAME,
    consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION,
    policy_url: "https://scalingneuro.com/docs/contribution-policy",
    self_service_quota_bytes: legacyQuotaCompatibility
      ? LEGACY_UNCAPPED_QUOTA_SENTINEL
      : null,
    minimum_client_version: MINIMUM_SELF_SERVICE_CLIENT_VERSION,
  };
}

function publicRegistrationRequestHash(
  input: PublicRegistrationRequest,
): Promise<string> {
  return sha256Hex(
    canonicalJson({
      registration_id: input.registration_id,
      device_name: input.device_name,
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

async function findPublicRegistrationReplay(
  env: Env,
  registrationId: string,
): Promise<PublicRegistrationRow | null> {
  return env.DB.prepare(
    `SELECT r.id AS registration_id,
            r.request_hash,
            d.id AS device_id,
            d.enrollment_id,
            d.token_hash,
            d.revoked_at,
            d.site_id,
            d.project_id,
            p.name AS project_name,
            d.accepted_consent_policy_version,
            s.pseudonym_key_ciphertext
     FROM contributor_registrations r
     JOIN devices d ON d.id = r.device_id
     JOIN projects p ON p.id = r.project_id
     JOIN sites s ON s.id = r.site_id
     WHERE r.id = ?1
     LIMIT 1`,
  )
    .bind(registrationId)
    .first<PublicRegistrationRow>();
}

async function publicRegistrationResponse(
  env: Env,
  existing: PublicRegistrationRow,
  input: PublicRegistrationRequest,
  requestHash: string,
  deviceTokenHash: string,
): Promise<Record<string, unknown> | null> {
  if (
    existing.revoked_at !== null ||
    existing.request_hash !== requestHash ||
    !(await constantTimeEqual(existing.token_hash, deviceTokenHash))
  ) {
    return null;
  }
  await env.DB.prepare(
    "UPDATE devices SET client_version = ?1, platform = ?2, last_seen_at = ?3 WHERE id = ?4",
  )
    .bind(input.client_version, input.platform, nowSeconds(), existing.device_id)
    .run();
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
}

export async function registerContributor(
  env: Env,
  input: PublicRegistrationRequest,
): Promise<Record<string, unknown>> {
  requireSelfServiceClientVersion(input.client_version);
  if (
    input.accepted_consent_policy_version !== PUBLIC_CONSENT_POLICY_VERSION
  ) {
    throw new AppError(
      "CONSENT_POLICY_UPDATE_REQUIRED",
      409,
      "Review and accept the current public contribution policy",
      { consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION },
    );
  }
  const requestHash = await publicRegistrationRequestHash(input);
  const deviceTokenHash = await sha256Hex(input.device_token);
  const existing = await findPublicRegistrationReplay(
    env,
    input.registration_id,
  );
  if (existing) {
    const response = await publicRegistrationResponse(
      env,
      existing,
      input,
      requestHash,
      deviceTokenHash,
    );
    if (response) return response;
    throw new AppError(
      "CONFLICT",
      409,
      "Registration operation conflicts with an existing enrollment",
    );
  }

  const timestamp = nowSeconds();
  const siteId = crypto.randomUUID();
  const projectId = crypto.randomUUID();
  const deviceId = crypto.randomUUID();
  const siteKey = randomBytes(32);
  const siteKeyCiphertext = await encryptSiteKey(
    siteKey,
    siteId,
    env.SITE_KEY_ENCRYPTION_KEY_B64,
  );
  const emailCiphertext = await encryptRegistrationEmail(
    input.contact_email,
    input.registration_id,
    env.SITE_KEY_ENCRYPTION_KEY_B64,
  );
  const emailHash = await sha256Hex(input.contact_email);
  const siteName = `${input.lab_name} — ${input.institution_name}`;
  const siteSlug = `public-${input.registration_id}`;

  try {
    await env.DB.batch([
      env.DB.prepare(
        `INSERT INTO sites
           (id, slug, name, pseudonym_key_ciphertext, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)`,
      ).bind(siteId, siteSlug, siteName, siteKeyCiphertext, timestamp),
      env.DB.prepare(
        `INSERT INTO projects
           (id, site_id, slug, name, consent_policy_version, active,
            upload_quota_bytes, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)`,
      ).bind(
        projectId,
        siteId,
        PUBLIC_PROJECT_SLUG,
        PUBLIC_PROJECT_NAME,
        PUBLIC_CONSENT_POLICY_VERSION,
        null,
        timestamp,
      ),
      env.DB.prepare(
        `INSERT INTO devices
           (id, enrollment_id, invite_id, site_id, project_id, token_hash,
            device_name, platform, client_version,
            accepted_consent_policy_version, created_at, last_seen_at)
         VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)`,
      ).bind(
        deviceId,
        input.registration_id,
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
      auditStatement(env, "contributor.registered", {
        siteId,
        projectId,
        deviceId,
        subjectType: "registration",
        subjectId: input.registration_id,
        createdAt: timestamp,
      }),
    ]);
  } catch {
    const raced = await findPublicRegistrationReplay(
      env,
      input.registration_id,
    );
    if (raced) {
      const response = await publicRegistrationResponse(
        env,
        raced,
        input,
        requestHash,
        deviceTokenHash,
      );
      if (response) return response;
    }
    // A UUID collision or exact concurrent replay is resolved above. Any
    // remaining D1 failure is transient or internal storage trouble, not a
    // semantic conflict the researcher can fix by changing their lab details.
    // Returning a retryable 5xx lets the client safely replay the same
    // registration operation and device token.
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Unable to persist registration; retry the same operation",
    );
  }

  return {
    enrollment_id: input.registration_id,
    device_token: input.device_token,
    device_id: deviceId,
    site_id: siteId,
    project_id: projectId,
    project_name: PUBLIC_PROJECT_NAME,
    consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION,
    pseudonym_key_b64: pseudonymKeyBase64(siteKey),
  };
}

export async function listContributorRegistrations(
  request: Request,
  env: Env,
): Promise<Record<string, unknown>> {
  await authenticateAdmin(request, env);
  const rows = await env.DB.prepare(
    `SELECT r.id, r.site_id, r.project_id, r.device_id,
            r.email_ciphertext, r.contact_name, r.institution_name,
            r.institution_ror_id, r.lab_name, r.contact_opt_in, r.created_at,
            d.platform, d.client_version, d.last_seen_at, d.revoked_at,
            COALESCE(SUM(CASE WHEN u.status = 'committed' THEN u.series_count ELSE 0 END), 0)
              AS committed_series,
            COALESCE(SUM(CASE WHEN u.status = 'committed' THEN u.total_bytes ELSE 0 END), 0)
              AS committed_bytes,
            COUNT(DISTINCT CASE WHEN u.status = 'committed' THEN u.id END)
              AS committed_uploads
     FROM contributor_registrations r
     JOIN devices d ON d.id = r.device_id
     LEFT JOIN uploads u ON u.project_id = r.project_id
     GROUP BY r.id
     ORDER BY r.created_at DESC
     LIMIT 200`,
  ).all<
    ContributorRegistrationRow & {
      platform: string;
      client_version: string;
      last_seen_at: number;
      revoked_at: number | null;
      committed_series: number;
      committed_bytes: number;
      committed_uploads: number;
    }
  >();
  const registrations = await Promise.all(
    rows.results.map(async (row) => ({
      registration_id: row.id,
      site_id: row.site_id,
      project_id: row.project_id,
      device_id: row.device_id,
      contact_email: await decryptRegistrationEmail(
        row.email_ciphertext,
        row.id,
        env.SITE_KEY_ENCRYPTION_KEY_B64,
      ),
      contact_name: row.contact_name,
      institution_name: row.institution_name,
      institution_ror_id: row.institution_ror_id,
      lab_name: row.lab_name,
      contact_opt_in: row.contact_opt_in === 1,
      platform: row.platform,
      client_version: row.client_version,
      status: row.revoked_at === null ? "active" : "revoked",
      created_at: iso(row.created_at),
      last_seen_at: iso(row.last_seen_at),
      committed_uploads: row.committed_uploads,
      committed_series: row.committed_series,
      committed_bytes: row.committed_bytes,
    })),
  );
  return { registrations };
}

export async function enroll(
  env: Env,
  input: EnrollRequest,
): Promise<Record<string, unknown>> {
  requireSupportedClientVersion(input.client_version);
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
  requireSupportedClientVersion(input.client_version);
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
  if (
    existing &&
    existing.status !== "expired" &&
    existing.status !== "withdrawn"
  ) {
    return {
      body: await createCredentialsResponse(env, existing),
      created: false,
    };
  }
  if (existing?.status === "expired") {
    await retireExpiredUploadAttempt(env, existing);
  }

  // Reconcile stable catalog identity before the one-active-upload check. This
  // ordering makes a lost response recoverable when an earlier reconciliation
  // already created an upload for only the new subset: the client can remove
  // the same committed bundles again, then replay that exact subset request.
  const catalogRows = await catalogBundles(env, device, bundlesWithHashes);
  const catalogByBundleId = new Map(
    catalogRows.map((row) => [row.bundle_id, row]),
  );
  const committedMatches: CatalogRow[] = [];
  for (const { descriptor, hash } of bundlesWithHashes) {
    const row = catalogByBundleId.get(descriptor.bundle_id);
    if (!row) continue;
    if (row.withdrawn_at !== null) {
      throw new AppError(
        "DUPLICATE_BUNDLE",
        409,
        "Series bundle has been withdrawn and remains tombstoned",
        {
          reason: "withdrawn_tombstone",
          bundle_id: descriptor.bundle_id,
          series_id: descriptor.series_id,
        },
      );
    }
    if (!catalogIdentityMatches(row, descriptor, hash)) {
      throw new AppError(
        "DUPLICATE_BUNDLE",
        409,
        "Bundle identifier conflicts with existing archive identity",
        {
          reason: "identity_conflict",
          bundle_id: descriptor.bundle_id,
          series_id: descriptor.series_id,
        },
      );
    }
    if (!catalogPrivacyContractMatches(row)) {
      throw new AppError(
        "DUPLICATE_BUNDLE",
        409,
        "Bundle exists under an older metadata privacy contract",
        {
          reason: "privacy_contract_stale",
          bundle_id: descriptor.bundle_id,
          series_id: descriptor.series_id,
        },
      );
    }
    committedMatches.push(row);
  }
  if (committedMatches.length > 0) {
    committedMatches.sort((left, right) =>
      left.bundle_id < right.bundle_id
        ? -1
        : left.bundle_id > right.bundle_id
          ? 1
          : 0,
    );
    if (
      committedMatches.length === bundlesWithHashes.length &&
      new Set(committedMatches.map((row) => row.upload_id)).size === 1
    ) {
      const receivedUpload = await env.DB.prepare(
        "SELECT * FROM uploads WHERE id = ?1 LIMIT 1",
      )
        .bind(committedMatches[0]!.upload_id)
        .first<UploadRow>();
      if (receivedUpload?.status === "committed") {
        return { body: uploadStatusResponse(receivedUpload), created: false };
      }
    }
    throw new AppError(
      "DUPLICATE_BUNDLE",
      409,
      "One or more series bundles are already committed",
      {
        reason: "active_exact_match",
        existing_bundles: committedMatches.map(existingBundleDetails),
      },
    );
  }

  const reservationRows: Array<{
    bundle_id: string;
    upload_id: string;
    series_id: string;
    bundle_hash: string;
    withdrawn_at: number | null;
  }> = [];
  for (let offset = 0; offset < bundlesWithHashes.length; offset += 40) {
    const chunk = bundlesWithHashes.slice(offset, offset + 40);
    const placeholders = chunk.map((_, index) => `?${index + 3}`).join(", ");
    const rows = await env.DB.prepare(
      `SELECT bundle_id, upload_id, series_id, bundle_hash, withdrawn_at
       FROM received_series_reservations
       WHERE site_id = ?1 AND project_id = ?2
         AND bundle_id IN (${placeholders})`,
    )
      .bind(
        device.site_id,
        device.project_id,
        ...chunk.map(({ descriptor }) => descriptor.bundle_id),
      )
      .all<{
        bundle_id: string;
        upload_id: string;
        series_id: string;
        bundle_hash: string;
        withdrawn_at: number | null;
      }>();
    reservationRows.push(...rows.results);
  }
  if (reservationRows.length > 0) {
    const byId = new Map(reservationRows.map((row) => [row.bundle_id, row]));
    for (const { descriptor, hash } of bundlesWithHashes) {
      const row = byId.get(descriptor.bundle_id);
      if (!row) continue;
      if (row.withdrawn_at !== null) {
        throw new AppError(
          "DUPLICATE_BUNDLE",
          409,
          "Series bundle has been withdrawn and remains tombstoned",
          { reason: "withdrawn_tombstone", bundle_id: descriptor.bundle_id },
        );
      }
      if (row.series_id !== descriptor.series_id || row.bundle_hash !== hash) {
        throw new AppError(
          "DUPLICATE_BUNDLE",
          409,
          "Bundle identifier conflicts with an existing receipt",
          { reason: "identity_conflict", bundle_id: descriptor.bundle_id },
        );
      }
    }
    if (
      reservationRows.length === bundlesWithHashes.length &&
      new Set(reservationRows.map((row) => row.upload_id)).size === 1
    ) {
      const receivedUpload = await env.DB.prepare(
        "SELECT * FROM uploads WHERE id = ?1 LIMIT 1",
      )
        .bind(reservationRows[0]!.upload_id)
        .first<UploadRow>();
      if (receivedUpload?.status === "committed") {
        return { body: uploadStatusResponse(receivedUpload), created: false };
      }
    }
    throw new AppError(
      "DUPLICATE_BUNDLE",
      409,
      "One or more series bundles were already received",
      {
        reason: "active_exact_match",
        existing_bundles: reservationRows.map((row) => ({
          bundle_id: row.bundle_id,
          series_id: row.series_id,
          upload_id: row.upload_id,
        })),
      },
    );
  }

  // A privacy-contract release may strand a prepared upload created by an
  // older client. Keep its active slot until every staged object is purged,
  // then let the current client allocate a clean replacement.
  await retireUnsupportedActiveUpload(env, device);

  const timestamp = nowSeconds();
  await env.DB.prepare(
    `UPDATE uploads
     SET status = 'expired', updated_at = ?1
     WHERE device_id = ?2 AND ingest_format = 'nifti-v1'
       AND status IN ('created', 'uploading')
       AND expires_at <= ?1
       AND (operation_token IS NULL OR operation_expires_at <= ?1)`,
  )
    .bind(timestamp, device.id)
    .run();
  const activeUpload = await env.DB.prepare(
    `SELECT id FROM uploads
     WHERE device_id = ?1 AND ingest_format = 'nifti-v1'
       AND status IN ('created', 'uploading')
     LIMIT 1`,
  )
    .bind(device.id)
    .first<{ id: string }>();
  if (activeUpload) {
    throw new AppError(
      "CONFLICT",
      409,
      "This device already has an active upload; rerun the same folder command to continue it before syncing another folder",
      { upload_id: activeUpload.id },
    );
  }

  const uploadId = crypto.randomUUID();
  const archivePrefix = `archive/v1/${device.site_id}/${device.project_id}/${uploadId}/`;
  const expiresAt = timestamp + uploadTtl(env);
  const totalBytes = input.bundles.reduce(
    (sum, bundle) => sum + bundle.nii.size + bundle.metadata.size,
    0,
  );
  if (device.upload_quota_bytes !== null) {
    const usage = await env.DB.prepare(
      `SELECT COALESCE(SUM(total_bytes), 0) AS used_bytes
       FROM uploads
       WHERE project_id = ?1
         AND status IN ('created', 'uploading', 'committed')`,
    )
      .bind(device.project_id)
      .first<{ used_bytes: number }>();
    const usedBytes = Number(usage?.used_bytes ?? 0);
    if (usedBytes + totalBytes > device.upload_quota_bytes) {
      throw new AppError(
        "QUOTA_EXCEEDED",
        413,
        "This self-service project has reached its upload allowance; contact Scaling Neuro to continue",
        {
          quota_bytes: device.upload_quota_bytes,
          used_bytes: usedBytes,
          requested_bytes: totalBytes,
        },
      );
    }
  }

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
  requireSupportedClientVersion(upload.client_version);
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
  return {
    ...(await signR2UploadPart(env, {
      key: object.object_key,
      uploadId: object.r2_multipart_id,
      partNumber: input.part_number,
      size: input.size,
      sha256: input.sha256,
    })),
  };
}

export async function getUploadStatus(
  request: Request,
  env: Env,
  uploadId: string,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  const upload = await getUploadForDevice(env, uploadId, device.id);
  return uploadStatusWithProgress(env, upload);
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

async function completionObjectState(
  env: Env,
  upload: UploadRow,
  expected: ExpectedObject[],
  input: CompleteUploadRequest,
): Promise<CompletionObjectState[]> {
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
  if (rows.size !== expected.length) {
    throw new AppError(
      "INTERNAL",
      500,
      "Multipart object catalog is incomplete",
    );
  }

  const state = expected.map((item) => {
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

  return state;
}

function assertStoredObjectIdentity(
  upload: UploadRow,
  item: ExpectedObject,
  object: R2Object,
): void {
  const metadataHash = object.customMetadata?.sha256;
  const metadataUploadId =
    object.customMetadata?.upload_id ??
    object.customMetadata?.["upload-id"];
  if (
    object.size !== item.size ||
    metadataHash !== item.sha256 ||
    metadataUploadId !== upload.id
  ) {
    throw new StoredObjectValidationError(
      "Stored object metadata does not match",
      { key: item.key },
    );
  }
}

async function persistedObjectHead(
  env: Env,
  item: ExpectedObject,
  failureMessage: string,
): Promise<R2Object | null> {
  try {
    return await env.ARCHIVE.head(item.key);
  } catch {
    throw new AppError("STORAGE_UNAVAILABLE", 502, failureMessage);
  }
}

async function checkpointFinalizedBundle(
  env: Env,
  upload: UploadRow,
  chunk: CompletionObjectState[],
  heads: ReadonlyMap<string, R2Object>,
): Promise<void> {
  const [first, second] = chunk;
  if (!first || !second) {
    throw new AppError("INTERNAL", 500, "Bundle object pair is incomplete");
  }
  const checkpointedAt = nowSeconds();
  // One SQL statement is the atomic pair boundary. CASE preserves each
  // object's distinct ETag while preventing a half-finalized bundle.
  const checkpoint = await env.DB.prepare(
    `UPDATE upload_objects
     SET completed_at = COALESCE(completed_at, ?1),
         etag = CASE object_key WHEN ?2 THEN ?3 WHEN ?4 THEN ?5 END
     WHERE upload_id = ?6 AND object_key IN (?2, ?4)
       AND EXISTS (
         SELECT 1 FROM uploads
         WHERE id = ?6 AND operation_token = ?7
           AND operation_kind = 'verify'
       )`,
  )
    .bind(
      checkpointedAt,
      first.item.key,
      heads.get(first.item.key)!.etag,
      second.item.key,
      heads.get(second.item.key)!.etag,
      upload.id,
      upload.operation_token,
    )
    .run();
  if ((checkpoint.meta.changes ?? 0) !== 2) {
    throw new AppError(
      "CONFLICT",
      409,
      "Upload object-finalization checkpoint lost its lease",
    );
  }
  const released = await env.DB.prepare(
    `UPDATE uploads
     SET updated_at = ?1, operation_token = NULL,
         operation_kind = NULL, operation_expires_at = NULL
     WHERE id = ?2 AND operation_token = ?3
       AND operation_kind = 'verify'`,
  )
    .bind(checkpointedAt, upload.id, upload.operation_token)
    .run();
  if ((released.meta.changes ?? 0) !== 1) {
    throw new AppError(
      "CONFLICT",
      409,
      "Upload object-finalization checkpoint lost its lease",
    );
  }
}

async function finalizeNextBundle(
  env: Env,
  upload: UploadRow,
  bundles: BundleRow[],
  state: CompletionObjectState[],
): Promise<boolean> {
  for (const bundle of bundles) {
    const chunk = state.filter(
      ({ item }) => item.bundle_id === bundle.bundle_id,
    );
    if (chunk.length !== 2) {
      throw new AppError("INTERNAL", 500, "Bundle object pair is incomplete");
    }
    if (
      chunk.every(
        ({ row }) => row.completed_at !== null && row.etag !== null,
      )
    ) {
      continue;
    }

    // HEAD first is essential for lost-response recovery. R2 may already have
    // committed an object even when the preceding Worker invocation ended
    // before D1 recorded the receipt. Never retry a stale multipart completion
    // until the authoritative object lookup says it is actually absent.
    const heads = new Map<string, R2Object>();
    for (const { item, clientObject, row } of chunk) {
      let head = await persistedObjectHead(
        env,
        item,
        "Unable to inspect multipart objects",
      );
      if (!head) {
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
          // A successful R2 completion can outlive a lost Worker response.
          // The second HEAD below distinguishes that case from a real storage
          // failure without retransmitting any part.
        }
        head = await persistedObjectHead(
          env,
          item,
          "Unable to complete multipart objects",
        );
      }
      if (!head) {
        throw new AppError(
          "STORAGE_UNAVAILABLE",
          502,
          "Persisted archive object is temporarily unavailable",
        );
      }
      assertStoredObjectIdentity(upload, item, head);
      heads.set(item.key, head);
    }

    // Object durability is checkpointed before any gzip or scientific
    // validation work. A terminated validation request can therefore resume
    // from R2 without touching multipart state.
    await checkpointFinalizedBundle(env, upload, chunk, heads);
    return true;
  }
  return false;
}

async function verifyStoredObject(
  env: Env,
  upload: UploadRow,
  bundle: BundleRow,
  state: CompletionObjectState,
): Promise<VerifiedObject> {
  const { item } = state;
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
  if (!stored) {
    throw new StoredObjectValidationError("Stored archive object is missing", {
      key: item.key,
    });
  }
  assertStoredObjectIdentity(upload, item, stored);

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
    const hashed = sha256PassThrough(stored.body);
    try {
      nifti = await inspectGzipNifti(
        hashed.body,
        bundle.nii_uncompressed_sha256,
      );
      storedSha256 = await hashed.sha256;
    } catch {
      void hashed.sha256.catch(() => undefined);
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
    if (!metadataBytes) {
      throw new AppError("INTERNAL", 500, "Metadata bytes are unavailable");
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

  const verified: VerifiedObject = { ...item, etag: stored.etag };
  if (nifti) verified.nifti = nifti;
  if (sidecar) verified.sidecar = sidecar;
  return verified;
}

async function checkpointVerifiedBundle(
  env: Env,
  upload: UploadRow,
  objects: VerifiedObject[],
): Promise<void> {
  const [first, second] = objects;
  if (!first || !second) {
    throw new AppError("INTERNAL", 500, "Bundle object pair is incomplete");
  }
  const checkpointedAt = nowSeconds();
  const checkpoint = await env.DB.prepare(
    `UPDATE upload_objects
     SET verified_at = ?1,
         etag = CASE object_key WHEN ?2 THEN ?3 WHEN ?4 THEN ?5 END
     WHERE upload_id = ?6 AND object_key IN (?2, ?4)
       AND completed_at IS NOT NULL
       AND EXISTS (
         SELECT 1 FROM uploads
         WHERE id = ?6 AND operation_token = ?7
           AND operation_kind = 'verify'
       )`,
  )
    .bind(
      checkpointedAt,
      first.key,
      first.etag,
      second.key,
      second.etag,
      upload.id,
      upload.operation_token,
    )
    .run();
  if ((checkpoint.meta.changes ?? 0) !== 2) {
    throw new AppError(
      "CONFLICT",
      409,
      "Upload verification checkpoint lost its lease",
    );
  }
  const released = await env.DB.prepare(
    `UPDATE uploads
     SET updated_at = ?1, operation_token = NULL,
         operation_kind = NULL, operation_expires_at = NULL
     WHERE id = ?2 AND operation_token = ?3
       AND operation_kind = 'verify'`,
  )
    .bind(checkpointedAt, upload.id, upload.operation_token)
    .run();
  if ((released.meta.changes ?? 0) !== 1) {
    throw new AppError(
      "CONFLICT",
      409,
      "Upload verification checkpoint lost its lease",
    );
  }
}

async function verifyNextBundle(
  env: Env,
  upload: UploadRow,
  bundles: BundleRow[],
  state: CompletionObjectState[],
): Promise<boolean> {
  for (const bundle of bundles) {
    const chunk = state.filter(
      ({ item }) => item.bundle_id === bundle.bundle_id,
    );
    if (chunk.length !== 2) {
      throw new AppError("INTERNAL", 500, "Bundle object pair is incomplete");
    }
    if (
      chunk.every(({ row }) => row.verified_at !== null && row.etag !== null)
    ) {
      continue;
    }
    if (
      chunk.some(
        ({ row }) => row.completed_at === null || row.etag === null,
      )
    ) {
      throw new AppError(
        "INTERNAL",
        500,
        "Scientific validation started before object finalization",
      );
    }

    // One series pair is the maximum expensive unit per invocation. The small
    // sidecar and streaming NIfTI read can run together without buffering the
    // scan in Worker memory.
    const results = await Promise.all(
      chunk.map((object) => verifyStoredObject(env, upload, bundle, object)),
    );
    const nifti = results.find((object) => object.kind === "nii")?.nifti;
    const sidecar = results.find(
      (object) => object.kind === "metadata",
    )?.sidecar;
    if (!nifti || !sidecar) {
      throw new AppError(
        "INTERNAL",
        500,
        "Verified bundle facts are incomplete",
      );
    }
    try {
      assertNiftiMatchesSidecar(nifti, sidecar.image);
    } catch {
      throw new StoredObjectValidationError(
        "NIfTI header does not match its metadata sidecar",
        { bundle_id: bundle.bundle_id },
      );
    }

    await checkpointVerifiedBundle(env, upload, results);
    return true;
  }
  return false;
}

function persistedVerifiedObjects(
  state: CompletionObjectState[],
): VerifiedObject[] {
  if (
    state.some(
      ({ row }) =>
        row.completed_at === null || row.verified_at === null || !row.etag,
    )
  ) {
    throw new AppError(
      "INTERNAL",
      500,
      "Verified object index is incomplete",
    );
  }

  return state
    .map(({ item, row }) => ({ ...item, etag: row.etag as string }))
    .sort((left, right) =>
      left.key < right.key ? -1 : left.key > right.key ? 1 : 0,
    );
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
    await abortAllMultipartUploads(env, upload.id);
    await deletePrefix(env, upload.archive_prefix);
    await deleteObject(env, archiveManifestKey(upload));
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

async function purgeConcurrentDuplicateUpload(
  env: Env,
  upload: UploadRow,
  operationToken: string,
): Promise<void> {
  const timestamp = nowSeconds();
  const expired = await env.DB.prepare(
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
  if ((expired.meta.changes ?? 0) !== 1) {
    throw new AppError(
      "CONFLICT",
      409,
      "Concurrent duplicate cleanup lost its upload lease",
    );
  }

  try {
    await abortAllMultipartUploads(env, upload.id);
    await deletePrefix(env, upload.archive_prefix);
    await deleteObject(env, archiveManifestKey(upload));
    const purgedAt = nowSeconds();
    const purged = await env.DB.prepare(
      `UPDATE uploads
       SET request_hash = request_hash || ':duplicate:' || id,
           purged_at = ?1, updated_at = ?1,
           operation_token = NULL, operation_kind = NULL,
           operation_expires_at = NULL
       WHERE id = ?2 AND status = 'expired' AND operation_token = ?3`,
    )
      .bind(purgedAt, upload.id, operationToken)
      .run();
    if ((purged.meta.changes ?? 0) !== 1) {
      throw new Error("duplicate purge lease changed");
    }
    await auditStatement(env, "upload.expired", {
      siteId: upload.site_id,
      projectId: upload.project_id,
      deviceId: upload.device_id,
      uploadId: upload.id,
      subjectType: "upload",
      subjectId: upload.id,
      detailCode: "concurrent_duplicate_reconciled",
      createdAt: purgedAt,
    }).run();
  } catch (error) {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Concurrent duplicate cleanup must finish before reconciliation",
      { upload_id: upload.id },
    );
  }
}

async function receiveLegacyNiftiUpload(
  env: Env,
  upload: UploadRow,
  bundles: BundleRow[],
  state: CompletionObjectState[],
): Promise<
  Array<{
    bundle_id: string;
    upload_id: string;
    series_id: string;
    bundle_hash: string;
    withdrawn_at: number | null;
  }> | null
> {
  const heads = new Map<string, R2Object>();
  // Multipart completion and HEAD are storage-control operations. Bound
  // concurrency keeps a full scanning session fast without ever opening a
  // NIfTI, gzip stream, or sidecar in the edge Worker.
  for (let offset = 0; offset < state.length; offset += 8) {
    await Promise.all(
      state.slice(offset, offset + 8).map(async ({ item, clientObject, row }) => {
        let head = await persistedObjectHead(
          env,
          item,
          "Unable to inspect uploaded objects",
        );
        if (!head) {
          try {
            await env.ARCHIVE.resumeMultipartUpload(
              item.key,
              row.r2_multipart_id as string,
            ).complete(
              clientObject.parts.map((part) => ({
                partNumber: part.part_number,
                etag: stripEtag(part.etag),
              })),
            );
          } catch {
            // A successful completion may outlive a disconnected response.
          }
          head = await persistedObjectHead(
            env,
            item,
            "Unable to complete uploaded objects",
          );
        }
        if (!head) {
          throw new AppError(
            "STORAGE_UNAVAILABLE",
            502,
            "Uploaded object is temporarily unavailable",
          );
        }
        assertStoredObjectIdentity(upload, item, head);
        heads.set(item.key, head);
      }),
    );
  }

  const receivedAt = nowSeconds();
  const statements: D1PreparedStatement[] = state.map(({ item }) =>
    env.DB.prepare(
      `UPDATE upload_objects
       SET completed_at = COALESCE(completed_at, ?1), etag = ?2
       WHERE upload_id = ?3 AND object_key = ?4
         AND EXISTS (
           SELECT 1 FROM uploads WHERE id = ?3 AND operation_token = ?5
             AND operation_kind = 'verify'
         )`,
    ).bind(
      receivedAt,
      heads.get(item.key)!.etag,
      upload.id,
      item.key,
      upload.operation_token,
    ),
  );
  for (const bundle of bundles) {
    statements.push(
      env.DB.prepare(
        `INSERT INTO received_series_reservations
           (upload_id, bundle_id, site_id, project_id, series_id, bundle_hash,
            input_format, received_at)
         SELECT ?1, ?2, u.site_id, u.project_id, ?3, ?4, 'nifti-v1', ?5
         FROM uploads u
         JOIN devices d ON d.id = u.device_id
         JOIN projects p ON p.id = u.project_id
         WHERE u.id = ?1 AND u.operation_token = ?6
           AND u.operation_kind = 'verify' AND d.revoked_at IS NULL
           AND p.active = 1
           AND p.consent_policy_version = u.consent_policy_version
           AND d.accepted_consent_policy_version = p.consent_policy_version`,
      ).bind(
        upload.id,
        bundle.bundle_id,
        bundle.series_id,
        bundle.bundle_hash,
        receivedAt,
        upload.operation_token,
      ),
      env.DB.prepare(
        `INSERT OR IGNORE INTO processing_jobs
           (id, upload_id, bundle_id, input_format, status, attempt,
            next_attempt_at, created_at, updated_at)
         SELECT ?1, ?2, ?3, 'nifti-v1', 'queued', 0, ?4, ?4, ?4
         WHERE EXISTS (
           SELECT 1 FROM uploads u
           JOIN devices d ON d.id = u.device_id
           JOIN projects p ON p.id = u.project_id
           WHERE u.id = ?2 AND u.operation_token = ?5
             AND u.operation_kind = 'verify' AND d.revoked_at IS NULL
             AND p.active = 1
             AND p.consent_policy_version = u.consent_policy_version
             AND d.accepted_consent_policy_version = p.consent_policy_version
         )`,
      ).bind(
        crypto.randomUUID(),
        upload.id,
        bundle.bundle_id,
        receivedAt,
        upload.operation_token,
      ),
    );
  }
  statements.push(
    env.DB.prepare(
      `UPDATE uploads
       SET status = 'committed', received_at = ?1, committed_at = ?1,
           updated_at = ?1, operation_token = NULL, operation_kind = NULL,
           operation_expires_at = NULL
       WHERE id = ?2 AND operation_token = ?3 AND operation_kind = 'verify'
         AND ingest_format = 'nifti-v1'
         AND EXISTS (
           SELECT 1 FROM devices d JOIN projects p ON p.id = uploads.project_id
           WHERE d.id = uploads.device_id AND d.revoked_at IS NULL
             AND p.active = 1
             AND p.consent_policy_version = uploads.consent_policy_version
             AND d.accepted_consent_policy_version = p.consent_policy_version
         )`,
    ).bind(receivedAt, upload.id, upload.operation_token),
    env.DB.prepare(
      `INSERT INTO audit_events
         (id, event_type, site_id, project_id, device_id, upload_id,
          subject_type, subject_id, detail_code, created_at)
       SELECT ?1, 'upload.received', site_id, project_id, device_id, id,
              'upload', id, 'nifti-v1_processing_queued', ?2
       FROM uploads WHERE id = ?3 AND status = 'committed'
         AND received_at = ?2`,
    ).bind(crypto.randomUUID(), receivedAt, upload.id),
  );
  try {
    await env.DB.batch(statements);
  } catch {
    const placeholders = bundles.map((_, index) => `?${index + 3}`).join(", ");
    const conflicts = await env.DB.prepare(
      `SELECT bundle_id, upload_id, series_id, bundle_hash, withdrawn_at
       FROM received_series_reservations
       WHERE site_id = ?1 AND project_id = ?2
         AND bundle_id IN (${placeholders}) AND upload_id != ?${bundles.length + 3}
       ORDER BY bundle_id`,
    )
      .bind(
        upload.site_id,
        upload.project_id,
        ...bundles.map((bundle) => bundle.bundle_id),
        upload.id,
      )
      .all<{
        bundle_id: string;
        upload_id: string;
        series_id: string;
        bundle_hash: string;
        withdrawn_at: number | null;
      }>();
    if (conflicts.results.length > 0) {
      const byId = new Map(
        conflicts.results.map((conflict) => [conflict.bundle_id, conflict]),
      );
      const exact = bundles.every((bundle) => {
        const conflict = byId.get(bundle.bundle_id);
        return (
          conflict &&
          conflict.withdrawn_at === null &&
          conflict.series_id === bundle.series_id &&
          conflict.bundle_hash === bundle.bundle_hash
        );
      });
      if (!exact || conflicts.results.length !== bundles.length) {
        throw new AppError(
          "DUPLICATE_BUNDLE",
          409,
          "Received series identity conflicts with this upload",
          {
            reason: conflicts.results.some(
              (conflict) => conflict.withdrawn_at !== null,
            )
              ? "withdrawn_tombstone"
              : "identity_conflict",
          },
        );
      }
      await purgeConcurrentDuplicateUpload(
        env,
        upload,
        upload.operation_token as string,
      );
      return conflicts.results;
    }
    throw new AppError("CONFLICT", 409, "Upload receipt could not be recorded");
  }
  const current = await env.DB.prepare(
    "SELECT status FROM uploads WHERE id = ?1 LIMIT 1",
  )
    .bind(upload.id)
    .first<{ status: UploadStatus }>();
  if (current?.status !== "committed") {
    throw new AppError("CONFLICT", 409, "Upload receipt lost its lease");
  }
  return null;
}

export async function completeUpload(
  request: Request,
  env: Env,
  uploadId: string,
  input: CompleteUploadRequest,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  const initialUpload = await getUploadForDevice(env, uploadId, device.id);
  requireSupportedClientVersion(initialUpload.client_version);
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
    // Completion is a client-driven state machine. A concurrent or
    // disconnected invocation is an ordinary in-progress state, not a failed
    // upload; expose its durable counters so the caller can poll once and
    // continue without issuing hidden conflict retries.
    return uploadStatusWithProgress(env, current);
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

    const state = await completionObjectState(
      env,
      upload,
      expectedObjects(upload, bundles),
      input,
    );
    // Scientific validation now belongs to the Sophont processor. The edge
    // only establishes an authoritative, durable object receipt and enqueues
    // one independently leasable job per series. Old clients still receive
    // the committed status they use as their terminal success signal.
    if (upload.ingest_format === "nifti-v1") {
      const duplicate = await receiveLegacyNiftiUpload(
        env,
        upload,
        bundles,
        state,
      );
      if (duplicate) {
        return {
          upload_id: upload.id,
          status: "committed",
          deduplicated: true,
          existing_bundles: duplicate.map((row) => ({
            bundle_id: row.bundle_id,
            upload_id: row.upload_id,
          })),
        };
      }
      const received = await getUploadForDevice(env, upload.id, device.id);
      return uploadStatusResponse(received);
    }
    if (await finalizeNextBundle(env, upload, bundles, state)) {
      const current = await getUploadForDevice(env, upload.id, device.id);
      return uploadStatusWithProgress(env, current);
    }
    if (await verifyNextBundle(env, upload, bundles, state)) {
      const current = await getUploadForDevice(env, upload.id, device.id);
      return uploadStatusWithProgress(env, current);
    }
    const verified = persistedVerifiedObjects(state);
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
      control_plane: { service_version: SERVICE_VERSION },
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
        const existingManifest = JSON.parse(
          utf8String(existingBytes),
        ) as Record<string, unknown>;
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
            metadata_object_key, metadata_size, metadata_sha256,
            metadata_policy_id, metadata_policy_version, committed_at)
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
                ?4,
                ?5,
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
        ).bind(
          committedAt,
          upload.id,
          operation.token,
          ACTIVE_METADATA_POLICY_ID,
          ACTIVE_METADATA_POLICY_VERSION,
        ),
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
      const winners = await catalogRowsByBundleId(
        env,
        upload.site_id,
        upload.project_id,
        bundles.map((bundle) => bundle.bundle_id),
      );
      const winnerById = new Map(
        winners.map((winner) => [winner.bundle_id, winner]),
      );
      const exactWinners = bundles.every((bundle) => {
        const winner = winnerById.get(bundle.bundle_id);
        return winner && catalogRowMatchesCommittedBundle(winner, bundle);
      });
      if (exactWinners && winners.length === bundles.length) {
        await purgeConcurrentDuplicateUpload(env, upload, operation.token);
        throw new AppError(
          "DUPLICATE_BUNDLE",
          409,
          "Equivalent series bundles were committed concurrently",
          {
            reason: "active_exact_match",
            existing_bundles: winners
              .sort((left, right) =>
                left.bundle_id.localeCompare(right.bundle_id),
              )
              .map(existingBundleDetails),
          },
        );
      }
      try {
        await env.ARCHIVE.delete(manifestKey);
      } catch {
        // Scheduled cleanup removes this uncommitted manifest if deletion is temporarily unavailable.
      }
      throw new AppError(
        "DUPLICATE_BUNDLE",
        409,
        "A conflicting series bundle was committed concurrently",
        { reason: "identity_conflict" },
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
               WHEN (operation_token IS NULL OR operation_expires_at <= ?1)
                AND (receipt_token IS NULL OR receipt_expires_at <= ?1)
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
           operation_expires_at = NULL, receipt_token = NULL,
           receipt_expires_at = NULL
       WHERE id = ?2 AND status != 'withdrawn'
         AND (operation_token IS NULL OR operation_expires_at <= ?1)
         AND (receipt_token IS NULL OR receipt_expires_at <= ?1)`,
    ).bind(timestamp, upload.id),
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
      `UPDATE received_series_reservations
       SET withdrawn_at = (
         SELECT withdrawn_at FROM uploads WHERE id = ?1
       )
       WHERE upload_id = ?1 AND withdrawn_at IS NULL
         AND EXISTS (
           SELECT 1 FROM uploads WHERE id = ?1 AND status = 'withdrawn'
         )`,
    ).bind(upload.id),
    env.DB.prepare(
      `UPDATE processing_jobs
       SET status = 'failed', failed_at = ?1, updated_at = ?1,
           error_code = 'UPLOAD_WITHDRAWN',
           error_message = 'The source upload was withdrawn',
           processor_id = NULL, lease_token = NULL, lease_expires_at = NULL
       WHERE upload_id = ?2 AND status IN ('queued', 'processing')
         AND EXISTS (
           SELECT 1 FROM uploads WHERE id = ?2 AND status = 'withdrawn'
         )`,
    ).bind(timestamp, upload.id),
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
  upload = (await env.DB.prepare("SELECT * FROM uploads WHERE id = ?1 LIMIT 1")
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
      await abortAllMultipartUploads(env, upload.id);
      await deletePrefix(env, upload.archive_prefix);
      await deleteObject(env, archiveManifestKey(upload));
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
       AND (receipt_token IS NULL OR receipt_expires_at <= ?1)
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
         AND (receipt_token IS NULL OR receipt_expires_at <= ?1)
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
       AND (receipt_token IS NULL OR receipt_expires_at <= ?1)
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
         AND (receipt_token IS NULL OR receipt_expires_at <= ?3)
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
      await abortAllMultipartUploads(env, upload.id);
      await deletePrefix(env, upload.archive_prefix);
      await deleteObject(env, archiveManifestKey(upload));
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
    version: SERVICE_VERSION,
  };
}
