import { authenticateDevice } from "./auth";
import { canonicalJson, sha256Hex } from "./crypto";
import { AppError } from "./errors";
import type { DeviceContext, Env, UploadStatus } from "./env";
import { presignUploadPart, uploadTtl } from "./r2";
import {
  clientVersionAtLeast,
  DATA_LICENSE_ID,
  DATA_LICENSE_URL,
  MINIMUM_EPI_CLIENT_VERSION,
  PUBLIC_CONSENT_POLICY_VERSION,
} from "./service";
import type {
  CompleteUploadRequest,
  CreateDicomUploadRequest,
  SignPartRequest,
} from "./validation";

export const DICOM_DEIDENTIFICATION_POLICY_ID =
  "scaling-neuro.dicom-deidentification";
export const DICOM_DEIDENTIFICATION_POLICY_VERSION = "2.0.0";

const BASE_PART_SIZE = 64 * 1024 * 1024;
const PART_SIZE_GRANULARITY = 1024 * 1024;
const RECEIPT_LEASE_SECONDS = 10 * 60;
const PROVISIONAL_RETENTION_SECONDS = 90 * 24 * 60 * 60;
export const BETA_PUBLICATION_DELAY_SECONDS = 7 * 24 * 60 * 60;

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
  data_license_id: string | null;
  data_license_granted_at: number | null;
  publication_scheduled_at: number | null;
  series_count: number;
  total_bytes: number;
  created_at: number;
  updated_at: number;
  expires_at: number;
  provisional_expires_at: number | null;
  received_at: number | null;
  withdrawn_at: number | null;
  deidentification_policy_id: string | null;
  deidentification_policy_version: string | null;
  receipt_token: string | null;
  receipt_expires_at: number | null;
}

interface DicomSeriesRow {
  upload_id: string;
  series_archive_id: string;
  series_id: string;
  subject_id: string;
  session_id: string;
  protocol_group_id: string;
  bundle_hash: string;
  dicom_count: number;
  archive_relative_key: string;
  expected_size: number;
  expected_sha256: string;
  r2_multipart_id: string | null;
  part_size: number | null;
  completed_at: number | null;
  etag: string | null;
  series_kind: "functional_epi";
  archive_route: "functional-epi-v1";
  pixel_data_policy: "scanner-native-not-defaced";
}

interface ReservationRow {
  upload_id: string;
  bundle_id: string;
  series_id: string;
  bundle_hash: string;
  series_kind: string;
  archive_route: string;
  pixel_data_policy: string;
  withdrawn_at: number | null;
}

function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

function iso(seconds: number | null): string | null {
  return seconds === null ? null : new Date(seconds * 1000).toISOString();
}

function publicationStatus(
  upload: UploadRow,
  timestamp = nowSeconds(),
): "staged" | "published" | null {
  if (
    upload.status !== "committed" ||
    upload.publication_scheduled_at === null
  ) {
    return null;
  }
  return upload.publication_scheduled_at <= timestamp
    ? "published"
    : "staged";
}

function writableUntil(upload: UploadRow): number {
  return upload.provisional_expires_at ?? upload.expires_at;
}

async function applyCurrentLicenseToWritableUpload(
  env: Env,
  upload: UploadRow,
): Promise<UploadRow> {
  if (
    !["created", "uploading"].includes(upload.status) ||
    (upload.data_license_id === DATA_LICENSE_ID &&
      upload.consent_policy_version === PUBLIC_CONSENT_POLICY_VERSION)
  ) {
    return upload;
  }
  const updated = await env.DB.prepare(
    `UPDATE uploads
     SET data_license_id = ?1, consent_policy_version = ?2,
         data_license_granted_at = NULL
     WHERE id = ?3 AND status IN ('created', 'uploading')
       AND data_license_granted_at IS NULL
     RETURNING *`,
  )
    .bind(DATA_LICENSE_ID, PUBLIC_CONSENT_POLICY_VERSION, upload.id)
    .first<UploadRow>();
  return updated ?? upload;
}

function stripEtag(value: string): string {
  return value.replace(/^"|"$/gu, "");
}

function multipartPartSize(objectSize: number): number {
  const minimum = Math.ceil(objectSize / 10_000);
  const rounded =
    Math.ceil(minimum / PART_SIZE_GRANULARITY) * PART_SIZE_GRANULARITY;
  return Math.max(BASE_PART_SIZE, rounded);
}

function requireCurrentClient(version: string): void {
  if (!clientVersionAtLeast(version, MINIMUM_EPI_CLIENT_VERSION)) {
    throw new AppError(
      "CLIENT_UPDATE_REQUIRED",
      426,
      "Install the current neuro-sync release",
      { minimum_client_version: MINIMUM_EPI_CLIENT_VERSION },
    );
  }
}

function requireCurrentContract(
  input: CreateDicomUploadRequest,
  device: DeviceContext,
): void {
  requireCurrentClient(input.client_version);
  if (
    device.accepted_consent_policy_version !==
      PUBLIC_CONSENT_POLICY_VERSION ||
    input.deidentification.policy_id !==
      DICOM_DEIDENTIFICATION_POLICY_ID ||
    input.deidentification.policy_version !==
      DICOM_DEIDENTIFICATION_POLICY_VERSION
  ) {
    throw new AppError(
      "CONSENT_POLICY_UPDATE_REQUIRED",
      409,
      "Review and accept the current data contribution and CC0 policy",
      {
        consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION,
        deidentification_policy_id: DICOM_DEIDENTIFICATION_POLICY_ID,
        deidentification_policy_version:
          DICOM_DEIDENTIFICATION_POLICY_VERSION,
      },
    );
  }
}

async function getUpload(
  env: Env,
  uploadId: string,
  deviceId: string,
): Promise<UploadRow> {
  const upload = await env.DB.prepare(
    `SELECT * FROM uploads
     WHERE id = ?1 AND device_id = ?2 LIMIT 1`,
  )
    .bind(uploadId, deviceId)
    .first<UploadRow>();
  if (!upload) throw new AppError("NOT_FOUND", 404, "Upload was not found");
  return upload;
}

async function getSeries(env: Env, uploadId: string): Promise<DicomSeriesRow> {
  const row = await env.DB.prepare(
    `SELECT * FROM dicom_upload_series WHERE upload_id = ?1 LIMIT 1`,
  )
    .bind(uploadId)
    .first<DicomSeriesRow>();
  if (!row) {
    throw new AppError("INTERNAL", 500, "EPI upload record is incomplete");
  }
  return row;
}

async function expireIfNeeded(
  env: Env,
  upload: UploadRow,
): Promise<UploadRow> {
  if (
    !["created", "uploading"].includes(upload.status) ||
    writableUntil(upload) > nowSeconds()
  ) {
    return upload;
  }
  const expired = await env.DB.prepare(
    `UPDATE uploads
     SET status = 'expired', updated_at = ?1, receipt_token = NULL,
         receipt_expires_at = NULL
     WHERE id = ?2 AND status IN ('created', 'uploading')
       AND COALESCE(provisional_expires_at, expires_at) <= ?1
     RETURNING *`,
  )
    .bind(nowSeconds(), upload.id)
    .first<UploadRow>();
  return expired ?? upload;
}

async function statusResponse(
  env: Env,
  upload: UploadRow,
): Promise<Record<string, unknown>> {
  const series = await getSeries(env, upload.id);
  const received = series.completed_at === null ? 0 : 1;
  const publication = publicationStatus(upload);
  const status =
    upload.status === "uploading" && received === 1
      ? "checkpointed"
      : upload.status;
  return {
    upload_id: upload.id,
    status,
    format: "dicom-series-v1",
    object_prefix: upload.archive_prefix,
    series_count: 1,
    total_bytes: upload.total_bytes,
    consent_policy_version: upload.consent_policy_version,
    ...(publication !== "published" || upload.data_license_id === null
      ? {}
      : {
          data_license: {
            id: upload.data_license_id,
            url: DATA_LICENSE_URL,
            granted_at: iso(
              upload.data_license_granted_at ??
                upload.publication_scheduled_at,
            ),
          },
        }),
    ...(publication === null
      ? {}
      : {
          publication: {
            status: publication,
            scheduled_at: iso(upload.publication_scheduled_at),
            ...(publication === "published"
              ? { published_at: iso(upload.publication_scheduled_at) }
              : { cancellation_email: "admin@sophont.med" }),
          },
        }),
    deidentification: {
      policy_id: upload.deidentification_policy_id,
      policy_version: upload.deidentification_policy_version,
    },
    receipt: {
      received_series: received,
      received_bytes: received === 1 ? series.expected_size : 0,
      total_series: 1,
      total_bytes: upload.total_bytes,
    },
    created_at: iso(upload.created_at),
    updated_at: iso(upload.updated_at),
    ...(upload.received_at === null
      ? {}
      : { received_at: iso(upload.received_at) }),
  };
}

async function ensureMultipart(
  env: Env,
  upload: UploadRow,
  series: DicomSeriesRow,
): Promise<DicomSeriesRow> {
  if (series.r2_multipart_id && series.part_size) return series;
  const key = `${upload.archive_prefix}${series.archive_relative_key}`;
  let multipart: R2MultipartUpload;
  try {
    multipart = await env.ARCHIVE.createMultipartUpload(key, {
      httpMetadata: { contentType: "application/zstd" },
      customMetadata: {
        upload_id: upload.id,
        series_archive_id: series.series_archive_id,
        sha256: series.expected_sha256,
        kind: "dicom_archive",
      },
    });
  } catch {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Unable to initialize the EPI archive upload",
    );
  }
  const updated = await env.DB.prepare(
    `UPDATE dicom_upload_series
     SET r2_multipart_id = ?1, part_size = ?2
     WHERE upload_id = ?3 AND series_archive_id = ?4
       AND r2_multipart_id IS NULL
     RETURNING *`,
  )
    .bind(
      multipart.uploadId,
      multipartPartSize(series.expected_size),
      upload.id,
      series.series_archive_id,
    )
    .first<DicomSeriesRow>();
  if (!updated) {
    await multipart.abort().catch(() => undefined);
    return getSeries(env, upload.id);
  }
  return updated;
}

async function credentialsResponse(
  env: Env,
  uploadInput: UploadRow,
): Promise<Record<string, unknown>> {
  const upload = await applyCurrentLicenseToWritableUpload(
    env,
    await expireIfNeeded(env, uploadInput),
  );
  requireCurrentClient(upload.client_version);
  if (upload.status === "committed") {
    return {
      ...(await statusResponse(env, upload)),
      multipart_objects: [],
    };
  }
  if (!["created", "uploading"].includes(upload.status)) {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Upload is no longer writable",
    );
  }
  const series = await ensureMultipart(env, upload, await getSeries(env, upload.id));
  const timestamp = nowSeconds();
  await env.DB.prepare(
    `UPDATE uploads
     SET status = 'uploading', updated_at = ?1
     WHERE id = ?2 AND status IN ('created', 'uploading')`,
  )
    .bind(timestamp, upload.id)
    .run();
  return {
    upload_id: upload.id,
    status: series.completed_at === null ? "uploading" : "checkpointed",
    format: "dicom-series-v1",
    object_prefix: upload.archive_prefix,
    multipart_objects:
      series.completed_at === null
        ? [
            {
              kind: "dicom_archive",
              series_archive_id: series.series_archive_id,
              key: `${upload.archive_prefix}${series.archive_relative_key}`,
              upload_id: series.r2_multipart_id,
              part_size: series.part_size,
            },
          ]
        : [],
  };
}

async function assertQuota(
  env: Env,
  device: DeviceContext,
  requestedBytes: number,
): Promise<void> {
  if (device.upload_quota_bytes === null) return;
  const usage = await env.DB.prepare(
    `SELECT COALESCE(SUM(total_bytes), 0) AS used_bytes
     FROM uploads
     WHERE project_id = ?1
       AND status IN ('created', 'uploading', 'committed')`,
  )
    .bind(device.project_id)
    .first<{ used_bytes: number }>();
  const usedBytes = Number(usage?.used_bytes ?? 0);
  if (usedBytes + requestedBytes > device.upload_quota_bytes) {
    throw new AppError(
      "QUOTA_EXCEEDED",
      413,
      "This project has reached its upload allowance",
      {
        quota_bytes: device.upload_quota_bytes,
        used_bytes: usedBytes,
        requested_bytes: requestedBytes,
      },
    );
  }
}

function alreadyReceived(
  reservation: ReservationRow,
): Record<string, unknown> {
  return {
    upload_id: reservation.upload_id,
    status: "already_received",
    format: "dicom-series-v1",
    series_count: 1,
    already_received_series: [
      {
        series_archive_id: reservation.bundle_id,
        receipt_upload_id: reservation.upload_id,
      },
    ],
  };
}

export async function createDicomUpload(
  request: Request,
  env: Env,
  input: CreateDicomUploadRequest,
): Promise<{ body: Record<string, unknown>; created: boolean }> {
  const device = await authenticateDevice(request, env);
  requireCurrentContract(input, device);
  const item = input.series[0];
  const bundleHash = await sha256Hex(canonicalJson(item));
  const requestHash = await sha256Hex(canonicalJson(input));

  let existing = await env.DB.prepare(
    `SELECT * FROM uploads WHERE device_id = ?1 AND request_hash = ?2 LIMIT 1`,
  )
    .bind(device.id, requestHash)
    .first<UploadRow>();
  if (existing) existing = await expireIfNeeded(env, existing);
  if (
    existing &&
    ["created", "uploading", "committed"].includes(existing.status)
  ) {
    return {
      body:
        existing.status === "committed"
          ? await statusResponse(env, existing)
          : await credentialsResponse(env, existing),
      created: false,
    };
  }
  if (existing?.status === "withdrawn") {
    throw new AppError(
      "DUPLICATE_BUNDLE",
      409,
      "This EPI archive was withdrawn and remains tombstoned",
      { reason: "withdrawn_tombstone" },
    );
  }
  if (existing?.status === "expired") {
    await env.DB.prepare(
      `UPDATE uploads SET request_hash = ?1 WHERE id = ?2 AND status = 'expired'`,
    )
      .bind(`${requestHash}:expired:${crypto.randomUUID()}`, existing.id)
      .run();
  }

  const reservation = await env.DB.prepare(
    `SELECT upload_id, bundle_id, series_id, bundle_hash, series_kind,
            archive_route, pixel_data_policy, withdrawn_at
     FROM received_series_reservations
     WHERE site_id = ?1 AND project_id = ?2 AND bundle_id = ?3
     LIMIT 1`,
  )
    .bind(device.site_id, device.project_id, item.series_archive_id)
    .first<ReservationRow>();
  if (reservation) {
    if (
      reservation.withdrawn_at === null &&
      reservation.series_id === item.series_id &&
      reservation.bundle_hash === bundleHash &&
      reservation.series_kind === "functional_epi" &&
      reservation.archive_route === "functional-epi-v1" &&
      reservation.pixel_data_policy === "scanner-native-not-defaced"
    ) {
      return { body: alreadyReceived(reservation), created: false };
    }
    throw new AppError(
      "DUPLICATE_BUNDLE",
      409,
      "The stable EPI identity already has a different archive receipt",
      {
        reason:
          reservation.withdrawn_at === null
            ? "identity_conflict"
            : "withdrawn_tombstone",
      },
    );
  }

  await assertQuota(env, device, item.archive.size);
  const uploadId = crypto.randomUUID();
  const timestamp = nowSeconds();
  const prefix =
    `dicom/v1/${device.site_id}/${device.project_id}/${uploadId}/`;
  const expiresAt = timestamp + uploadTtl(env);
  try {
    await env.DB.batch([
      env.DB.prepare(
        `INSERT INTO uploads
           (id, site_id, project_id, device_id, status, archive_prefix,
            request_hash, client_version, consent_policy_version,
            data_license_id, series_count, total_bytes,
            created_at, updated_at, expires_at,
            deidentification_policy_id, deidentification_policy_version)
         VALUES (?1, ?2, ?3, ?4, 'created', ?5, ?6, ?7, ?8, ?9,
                 1, ?10, ?11, ?11, ?12, ?13, ?14)`,
      ).bind(
        uploadId,
        device.site_id,
        device.project_id,
        device.id,
        prefix,
        requestHash,
        input.client_version,
        PUBLIC_CONSENT_POLICY_VERSION,
        DATA_LICENSE_ID,
        item.archive.size,
        timestamp,
        expiresAt,
        DICOM_DEIDENTIFICATION_POLICY_ID,
        DICOM_DEIDENTIFICATION_POLICY_VERSION,
      ),
      env.DB.prepare(
        `INSERT INTO dicom_upload_series
           (upload_id, series_archive_id, series_id, subject_id, session_id,
            protocol_group_id, bundle_hash, dicom_count,
            archive_relative_key, expected_size, expected_sha256,
            series_kind, archive_route, pixel_data_policy)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                 'functional_epi', 'functional-epi-v1',
                 'scanner-native-not-defaced')`,
      ).bind(
        uploadId,
        item.series_archive_id,
        item.series_id,
        item.subject_id,
        item.session_id,
        item.protocol_group_id,
        bundleHash,
        item.dicom_count,
        item.archive.relative_key,
        item.archive.size,
        item.archive.sha256,
      ),
      env.DB.prepare(
        `INSERT INTO audit_events
           (id, event_type, site_id, project_id, device_id, upload_id,
            subject_type, subject_id, detail_code, created_at)
         VALUES (?1, 'upload.created', ?2, ?3, ?4, ?5,
                 'upload', ?5, 'functional-epi', ?6)`,
      ).bind(
        crypto.randomUUID(),
        device.site_id,
        device.project_id,
        device.id,
        uploadId,
        timestamp,
      ),
    ]);
  } catch {
    const raced = await env.DB.prepare(
      `SELECT * FROM uploads WHERE device_id = ?1 AND request_hash = ?2 LIMIT 1`,
    )
      .bind(device.id, requestHash)
      .first<UploadRow>();
    if (raced) {
      return {
        body: await credentialsResponse(env, raced),
        created: false,
      };
    }
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Unable to create the EPI archive upload; retry the same folder",
    );
  }
  return {
    body: await credentialsResponse(
      env,
      await getUpload(env, uploadId, device.id),
    ),
    created: true,
  };
}

export async function refreshDicomUploadCredentials(
  request: Request,
  env: Env,
  uploadId: string,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  return credentialsResponse(
    env,
    await getUpload(env, uploadId, device.id),
  );
}

export async function createDicomUploadPartUrl(
  request: Request,
  env: Env,
  uploadId: string,
  input: SignPartRequest,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  const upload = await expireIfNeeded(
    env,
    await getUpload(env, uploadId, device.id),
  );
  requireCurrentClient(upload.client_version);
  if (!["created", "uploading"].includes(upload.status)) {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Upload is no longer writable",
    );
  }
  const series = await getSeries(env, upload.id);
  const key = `${upload.archive_prefix}${series.archive_relative_key}`;
  if (
    input.key !== key ||
    !series.r2_multipart_id ||
    !series.part_size
  ) {
    throw new AppError("OBJECT_MISSING", 404, "EPI archive was not found");
  }
  if (series.completed_at !== null) {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "EPI archive is already received",
    );
  }
  const partCount = Math.ceil(series.expected_size / series.part_size);
  const expectedSize =
    input.part_number === partCount
      ? series.expected_size - series.part_size * (partCount - 1)
      : series.part_size;
  if (
    input.part_number > partCount ||
    input.size !== expectedSize
  ) {
    throw new AppError("OBJECT_MISMATCH", 409, "Part size is incorrect");
  }
  return {
    ...(await presignUploadPart(env, {
      key,
      uploadId: series.r2_multipart_id,
      partNumber: input.part_number,
      size: input.size,
      sha256: input.sha256,
    })),
  };
}

function assertStoredObject(
  upload: UploadRow,
  series: DicomSeriesRow,
  head: R2Object,
): void {
  const metadata = head.customMetadata ?? {};
  if (
    head.size !== series.expected_size ||
    metadata.sha256 !== series.expected_sha256 ||
    metadata.upload_id !== upload.id ||
    metadata.series_archive_id !== series.series_archive_id ||
    metadata.kind !== "dicom_archive"
  ) {
    throw new AppError(
      "OBJECT_MISMATCH",
      409,
      "Stored EPI archive does not match its declaration",
    );
  }
}

async function checkpointObject(
  env: Env,
  upload: UploadRow,
  input: CompleteUploadRequest,
): Promise<DicomSeriesRow> {
  const series = await getSeries(env, upload.id);
  const key = `${upload.archive_prefix}${series.archive_relative_key}`;
  let head = await env.ARCHIVE.head(key);
  if (!head) {
    const object = input.objects[0];
    if (
      input.objects.length !== 1 ||
      !object ||
      object.key !== key ||
      object.size !== series.expected_size ||
      object.sha256 !== series.expected_sha256 ||
      !series.r2_multipart_id ||
      !series.part_size ||
      object.parts.length !==
        Math.ceil(series.expected_size / series.part_size)
    ) {
      throw new AppError(
        "OBJECT_MISMATCH",
        409,
        "Completion must describe the declared EPI archive",
      );
    }
    try {
      await env.ARCHIVE.resumeMultipartUpload(
        key,
        series.r2_multipart_id,
      ).complete(
        object.parts.map((part) => ({
          partNumber: part.part_number,
          etag: stripEtag(part.etag),
        })),
      );
    } catch {
      // The completion response can be lost after R2 commits the object.
    }
    head = await env.ARCHIVE.head(key);
  }
  if (!head) {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "EPI archive is temporarily unavailable after upload",
    );
  }
  assertStoredObject(upload, series, head);
  const timestamp = nowSeconds();
  await env.DB.batch([
    env.DB.prepare(
      `UPDATE dicom_upload_series
       SET completed_at = COALESCE(completed_at, ?1), etag = ?2
       WHERE upload_id = ?3 AND series_archive_id = ?4`,
    ).bind(timestamp, head.etag, upload.id, series.series_archive_id),
    env.DB.prepare(
      `UPDATE uploads
       SET status = 'uploading', provisional_expires_at = MAX(
             COALESCE(provisional_expires_at, 0), ?1
           ), updated_at = ?2
       WHERE id = ?3 AND status IN ('created', 'uploading')`,
    ).bind(
      timestamp + PROVISIONAL_RETENTION_SECONDS,
      timestamp,
      upload.id,
    ),
  ]);
  return { ...series, completed_at: timestamp, etag: head.etag };
}

async function claimReceipt(
  env: Env,
  upload: UploadRow,
): Promise<{ upload: UploadRow; token: string } | null> {
  const timestamp = nowSeconds();
  const token = crypto.randomUUID();
  const claimed = await env.DB.prepare(
    `UPDATE uploads
     SET receipt_token = ?1, receipt_expires_at = ?2, updated_at = ?3
     WHERE id = ?4 AND status IN ('created', 'uploading')
       AND COALESCE(provisional_expires_at, expires_at) > ?3
       AND (receipt_token IS NULL OR receipt_expires_at <= ?3)
     RETURNING *`,
  )
    .bind(token, timestamp + RECEIPT_LEASE_SECONDS, timestamp, upload.id)
    .first<UploadRow>();
  return claimed ? { upload: claimed, token } : null;
}

async function releaseReceipt(
  env: Env,
  uploadId: string,
  token: string,
): Promise<void> {
  await env.DB.prepare(
    `UPDATE uploads
     SET receipt_token = NULL, receipt_expires_at = NULL, updated_at = ?1
     WHERE id = ?2 AND receipt_token = ?3`,
  )
    .bind(nowSeconds(), uploadId, token)
    .run();
}

export async function checkpointDicomUpload(
  request: Request,
  env: Env,
  uploadId: string,
  input: CompleteUploadRequest,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  let upload = await applyCurrentLicenseToWritableUpload(
    env,
    await expireIfNeeded(
      env,
      await getUpload(env, uploadId, device.id),
    ),
  );
  if (upload.status === "committed") return statusResponse(env, upload);
  const claim = await claimReceipt(env, upload);
  if (!claim) {
    upload = await getUpload(env, uploadId, device.id);
    return statusResponse(env, upload);
  }
  try {
    await checkpointObject(env, claim.upload, input);
    await releaseReceipt(env, uploadId, claim.token);
    return statusResponse(
      env,
      await getUpload(env, uploadId, device.id),
    );
  } catch (error) {
    await releaseReceipt(env, uploadId, claim.token);
    throw error;
  }
}

export async function completeDicomUpload(
  request: Request,
  env: Env,
  uploadId: string,
  input: CompleteUploadRequest,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  let upload = await applyCurrentLicenseToWritableUpload(
    env,
    await expireIfNeeded(
      env,
      await getUpload(env, uploadId, device.id),
    ),
  );
  if (upload.status === "committed") return statusResponse(env, upload);
  const claim = await claimReceipt(env, upload);
  if (!claim) {
    upload = await getUpload(env, uploadId, device.id);
    return statusResponse(env, upload);
  }
  try {
    const series = await checkpointObject(env, claim.upload, input);
    const timestamp = nowSeconds();
    const publicationScheduledAt =
      timestamp + BETA_PUBLICATION_DELAY_SECONDS;
    try {
      await env.DB.batch([
        env.DB.prepare(
          `INSERT INTO received_series_reservations
             (upload_id, bundle_id, site_id, project_id, series_id,
              bundle_hash, received_at, series_kind,
              archive_route, pixel_data_policy)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                   'functional_epi', 'functional-epi-v1',
                   'scanner-native-not-defaced')`,
        ).bind(
          upload.id,
          series.series_archive_id,
          upload.site_id,
          upload.project_id,
          series.series_id,
          series.bundle_hash,
          timestamp,
        ),
        env.DB.prepare(
          `UPDATE uploads
           SET status = 'committed', received_at = ?1, updated_at = ?1,
               publication_scheduled_at = COALESCE(publication_scheduled_at, ?2),
               receipt_token = NULL,
               receipt_expires_at = NULL
           WHERE id = ?3 AND receipt_token = ?4`,
        ).bind(timestamp, publicationScheduledAt, upload.id, claim.token),
        env.DB.prepare(
          `INSERT INTO audit_events
             (id, event_type, site_id, project_id, device_id, upload_id,
              subject_type, subject_id, detail_code, created_at)
           VALUES (?1, 'upload.received', ?2, ?3, ?4, ?5,
                   'upload', ?5, 'functional-epi', ?6)`,
        ).bind(
          crypto.randomUUID(),
          upload.site_id,
          upload.project_id,
          upload.device_id,
          upload.id,
          timestamp,
        ),
        env.DB.prepare(
          `INSERT INTO audit_events
             (id, event_type, site_id, project_id, device_id, upload_id,
              subject_type, subject_id, detail_code, created_at)
           VALUES (?1, 'upload.publication_scheduled', ?2, ?3, ?4, ?5,
                   'upload', ?5, ?6, ?7)`,
        ).bind(
          crypto.randomUUID(),
          upload.site_id,
          upload.project_id,
          upload.device_id,
          upload.id,
          DATA_LICENSE_ID,
          timestamp,
        ),
      ]);
    } catch {
      const reservation = await env.DB.prepare(
        `SELECT upload_id, bundle_id, series_id, bundle_hash, series_kind,
                archive_route, pixel_data_policy, withdrawn_at
         FROM received_series_reservations
         WHERE site_id = ?1 AND project_id = ?2 AND bundle_id = ?3
         LIMIT 1`,
      )
        .bind(upload.site_id, upload.project_id, series.series_archive_id)
        .first<ReservationRow>();
      if (
        reservation &&
        reservation.withdrawn_at === null &&
        reservation.bundle_hash === series.bundle_hash &&
        reservation.series_id === series.series_id
      ) {
        await env.DB.prepare(
          `UPDATE uploads
           SET status = 'expired', receipt_token = NULL,
               receipt_expires_at = NULL,
               updated_at = ?1
           WHERE id = ?2`,
        )
          .bind(timestamp, upload.id)
          .run();
        return alreadyReceived(reservation);
      }
      throw new AppError(
        "STORAGE_UNAVAILABLE",
        502,
        "Unable to record the EPI archive receipt; retry the same folder",
      );
    }
    return statusResponse(
      env,
      await getUpload(env, uploadId, device.id),
    );
  } catch (error) {
    await releaseReceipt(env, uploadId, claim.token);
    throw error;
  }
}

interface StagedUploadRow {
  id: string;
  status: UploadStatus;
  site_id: string;
  project_id: string;
  device_id: string;
  archive_prefix: string;
  archive_relative_key: string;
  publication_scheduled_at: number | null;
  withdrawn_at: number | null;
}

export async function cancelStagedDicomUpload(
  env: Env,
  uploadId: string,
): Promise<Record<string, unknown>> {
  const row = await env.DB.prepare(
    `SELECT u.id, u.status, u.site_id, u.project_id, u.device_id,
            u.archive_prefix, d.archive_relative_key,
            u.publication_scheduled_at, u.withdrawn_at
     FROM uploads u
     JOIN dicom_upload_series d ON d.upload_id = u.id
     WHERE u.id = ?1
     LIMIT 1`,
  )
    .bind(uploadId)
    .first<StagedUploadRow>();
  if (!row) throw new AppError("NOT_FOUND", 404, "Upload was not found");

  const objectKey = `${row.archive_prefix}${row.archive_relative_key}`;
  if (row.status === "withdrawn") {
    await env.ARCHIVE.delete(objectKey);
    return {
      upload_id: row.id,
      publication_status: "cancelled",
      cancelled_at: iso(row.withdrawn_at),
    };
  }

  const timestamp = nowSeconds();
  if (
    row.status !== "committed" ||
    row.publication_scheduled_at === null ||
    row.publication_scheduled_at <= timestamp
  ) {
    throw new AppError(
      "CONFLICT",
      409,
      "Only an archive still inside its seven-day staging period can be cancelled",
    );
  }

  const results = await env.DB.batch([
    env.DB.prepare(
      `UPDATE uploads
       SET status = 'withdrawn', withdrawn_at = ?1, updated_at = ?1,
           data_license_id = NULL, data_license_granted_at = NULL
       WHERE id = ?2 AND status = 'committed'
         AND publication_scheduled_at > ?1`,
    ).bind(timestamp, row.id),
    env.DB.prepare(
      `UPDATE received_series_reservations
       SET withdrawn_at = ?1
       WHERE upload_id = ?2 AND withdrawn_at IS NULL
         AND EXISTS (
           SELECT 1 FROM uploads
           WHERE id = ?2 AND status = 'withdrawn' AND withdrawn_at = ?1
         )`,
    ).bind(timestamp, row.id),
    env.DB.prepare(
      `INSERT INTO audit_events
         (id, event_type, site_id, project_id, device_id, upload_id,
          subject_type, subject_id, detail_code, created_at)
       SELECT ?1, 'upload.cancelled_before_publication', ?2, ?3, ?4, ?5,
              'upload', ?5, 'contributor-request', ?6
       WHERE EXISTS (
         SELECT 1 FROM uploads
         WHERE id = ?5 AND status = 'withdrawn' AND withdrawn_at = ?6
       )`,
    ).bind(
      crypto.randomUUID(),
      row.site_id,
      row.project_id,
      row.device_id,
      row.id,
      timestamp,
    ),
  ]);
  if (results[0]?.meta.changes !== 1) {
    throw new AppError(
      "CONFLICT",
      409,
      "The archive is no longer inside its staging period",
    );
  }
  await env.ARCHIVE.delete(objectKey);
  return {
    upload_id: row.id,
    publication_status: "cancelled",
    cancelled_at: iso(timestamp),
  };
}

export async function getDicomUploadStatus(
  request: Request,
  env: Env,
  uploadId: string,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  return statusResponse(
    env,
    await expireIfNeeded(
      env,
      await getUpload(env, uploadId, device.id),
    ),
  );
}
