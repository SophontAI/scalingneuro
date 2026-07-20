import { authenticateDevice, authenticateProcessor } from "./auth";
import { canonicalJson, sha256Hex, utf8Bytes } from "./crypto";
import { AppError } from "./errors";
import type { DeviceContext, Env, IngestFormat, UploadStatus } from "./env";
import {
  presignGetObject,
  presignPutObject,
  presignUploadPart,
  deleteObject,
  deletePrefix,
  uploadTtl,
} from "./r2";
import {
  clientVersionAtLeast,
  MINIMUM_ALL_MR_CLIENT_VERSION,
  PUBLIC_CONSENT_POLICY_VERSION,
} from "./service";
import {
  ACTIVE_METADATA_POLICY_ID,
  ACTIVE_METADATA_POLICY_VERSION,
} from "./sidecar";
import type {
  CompleteUploadRequest,
  CreateDicomUploadRequest,
  DicomProcessorValidation,
  ProcessorClaimRequest,
  ProcessorCompleteRequest,
  ProcessorFailRequest,
  ProcessorLeaseRequest,
  ProcessorOutputDescriptor,
  ProcessorOutputRequest,
  SignPartRequest,
} from "./validation";
import packageManifest from "../package.json";
import {
  REQUIRED_PROCESSOR_CONTROLLER_SHA256,
  REQUIRED_PROCESSOR_PIPELINE_VERSION,
  REQUIRED_PROCESSOR_VERSION,
} from "./processor-contract";

export const DICOM_DEIDENTIFICATION_POLICY_ID =
  "scaling-neuro.dicom-deidentification";
export const DICOM_DEIDENTIFICATION_POLICY_VERSION = "2.0.0";
const LEGACY_DICOM_DEIDENTIFICATION_POLICY_VERSION = "1.0.0";

const BASE_PART_SIZE = 64 * 1024 * 1024;
const PART_SIZE_GRANULARITY = 1024 * 1024;
const RECEIPT_LEASE_SECONDS = 10 * 60;
// A source folder can contain many terabytes even though each independently
// checkpointed series is capped at 64 GiB.  Once its multipart object has been
// completed and HEAD-verified, retain it for 90 days while the client finishes
// and rechecks the whole-folder identity.  It is still provisional: no receipt,
// reservation, processing job, or scientific catalog entry exists yet.
const PROVISIONAL_DICOM_RETENTION_SECONDS = 90 * 24 * 60 * 60;
const MAX_PROCESSING_ATTEMPTS = 5;
const MINIMUM_DICOM_CLIENT_VERSION = "0.3.0";
const PURGE_ELIGIBLE_DICOM_ERROR_CODES = [
  "DICOM_PRIVACY_AUDIT_FAILED",
  // The processor only reports this terminal code after five independently
  // downloaded copies of the immutable R2 object disagree with the intake
  // digest. A single transport disagreement is deliberately not purge proof.
  "STORED_OBJECT_SHA256_MISMATCH",
  "INVALID_DICOM_ARCHIVE",
  "ARCHIVE_DEIDENTIFICATION_POLICY_MISMATCH",
  "ARCHIVE_DEIDENTIFICATION_UNVERIFIED",
  "ARCHIVE_DICOM_COUNT_MISMATCH",
  "ARCHIVE_DICOM_SIZE_INVALID",
  "ARCHIVE_INSTANCE_MISMATCH",
  "ARCHIVE_INSTANCE_ORDER",
  "ARCHIVE_MANIFEST_DUPLICATE_KEY",
  "ARCHIVE_MANIFEST_JSON",
  "ARCHIVE_MANIFEST_MISSING",
  "ARCHIVE_MANIFEST_NOT_LAST",
  "ARCHIVE_MANIFEST_SCHEMA",
  "ARCHIVE_MANIFEST_TOO_LARGE",
  "ARCHIVE_MANUFACTURER_MISMATCH",
  "ARCHIVE_PROCESSING_ROUTE_MISMATCH",
  "ARCHIVE_SCANNER_METADATA_MISMATCH",
  "ARCHIVE_SERIES_MISMATCH",
  "ARCHIVE_SIEMENS_CSA_REQUIRED",
  "ARCHIVE_SIZE_INVALID",
  "ARCHIVE_SOP_UID_MISMATCH",
  "ARCHIVE_TAR_HEADER_INVALID",
  "ARCHIVE_TAR_PADDING_INVALID",
  "ARCHIVE_TRAILING_DATA",
  "ARCHIVE_TRUNCATED",
  "ARCHIVE_UNCOMPRESSED_LIMIT",
  "ARCHIVE_UNSUPPORTED_DICOM_FORM",
  "ARCHIVE_VENDOR_METADATA_MISMATCH",
  "ARCHIVE_ZSTD_INVALID",
] as const;
const PURGE_ELIGIBLE_DICOM_ERROR_CODE_SET = new Set<string>(
  PURGE_ELIGIBLE_DICOM_ERROR_CODES,
);

type DicomProcessingRoute = "functional-epi-v1" | "archive-verify-v1";

interface UploadRow {
  id: string;
  site_id: string;
  project_id: string;
  device_id: string;
  status: UploadStatus;
  ingest_format: IngestFormat;
  archive_prefix: string;
  request_hash: string;
  client_version: string;
  consent_policy_version: string;
  deidentification_policy_id: string | null;
  deidentification_policy_version: string | null;
  series_count: number;
  total_bytes: number;
  created_at: number;
  updated_at: number;
  expires_at: number;
  provisional_expires_at: number | null;
  received_at: number | null;
  committed_at: number | null;
  withdrawn_at: number | null;
  purged_at: number | null;
  receipt_reconciled_at: number | null;
  manifest_object_key: string | null;
  manifest_sha256: string | null;
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
  series_kind: string;
  processing_route: DicomProcessingRoute;
  effective_series_kind: string | null;
  effective_processing_route: DicomProcessingRoute | null;
  pixel_data_policy: "scanner-native-not-defaced";
  archive_relative_key: string;
  expected_size: number;
  expected_sha256: string;
  r2_multipart_id: string | null;
  part_size: number | null;
  completed_at: number | null;
  etag: string | null;
}

interface ProcessingJobRow {
  id: string;
  upload_id: string;
  bundle_id: string;
  input_format: IngestFormat;
  status: "queued" | "processing" | "processed" | "failed";
  attempt: number;
  next_attempt_at: number;
  processor_id: string | null;
  lease_token: string | null;
  lease_expires_at: number | null;
  created_at: number;
  updated_at: number;
  error_code: string | null;
  input_purged_at: number | null;
  completion_hash: string | null;
}

interface ProcessingOutputRow {
  job_id: string;
  kind: "nifti" | "sidecar" | "processing_manifest";
  object_key: string;
  expected_size: number;
  expected_sha256: string;
  content_type: string;
  uncompressed_sha256: string | null;
  completed_at: number | null;
  etag: string | null;
}

interface ReceiptReservationRow {
  upload_id: string;
  bundle_id: string;
  series_id: string;
  bundle_hash: string;
  series_kind: string;
  processing_route: DicomProcessingRoute;
  pixel_data_policy: "scanner-native-not-defaced";
  withdrawn_at: number | null;
}

interface ReleasedReservationRow {
  series_archive_id: string;
  bundle_hash: string;
  release_reason: string;
  withdrawn_at: number | null;
}

interface ReconciledSeriesRow {
  series_archive_id: string;
  existing_upload_id: string;
}

function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

function iso(seconds: number | null): string | null {
  return seconds === null ? null : new Date(seconds * 1000).toISOString();
}

function dicomUploadExpiresAt(upload: UploadRow): number {
  return upload.provisional_expires_at ?? upload.expires_at;
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

function requireSupportedClient(value: string): void {
  if (!clientVersionAtLeast(value, MINIMUM_DICOM_CLIENT_VERSION)) {
    throw new AppError(
      "CLIENT_UPDATE_REQUIRED",
      426,
      "This client is older than the active privacy contract; install the current release",
      { minimum_client_version: MINIMUM_DICOM_CLIENT_VERSION },
    );
  }
}

function processingRoute(
  item: CreateDicomUploadRequest["series"][number],
): DicomProcessingRoute {
  return item.processing_route ?? "functional-epi-v1";
}

function requireSupportedDicomContract(
  input: CreateDicomUploadRequest,
  device: DeviceContext,
): void {
  if (input.deidentification.policy_id !== DICOM_DEIDENTIFICATION_POLICY_ID) {
    throw new AppError(
      "CLIENT_UPDATE_REQUIRED",
      426,
      "The DICOM deidentification policy is not supported",
      {
        policy_id: DICOM_DEIDENTIFICATION_POLICY_ID,
        policy_version: DICOM_DEIDENTIFICATION_POLICY_VERSION,
      },
    );
  }
  if (
    input.deidentification.policy_version ===
    LEGACY_DICOM_DEIDENTIFICATION_POLICY_VERSION
  ) {
    requireSupportedClient(input.client_version);
    if (
      input.series.some(
        (item) =>
          item.series_kind !== undefined ||
          item.processing_route !== undefined ||
          item.pixel_data_policy !== undefined,
      )
    ) {
      throw new AppError(
        "INVALID_REQUEST",
        400,
        "Legacy DICOM uploads cannot declare all-MR routing fields",
      );
    }
    return;
  }
  if (
    input.deidentification.policy_version !==
    DICOM_DEIDENTIFICATION_POLICY_VERSION
  ) {
    throw new AppError(
      "CLIENT_UPDATE_REQUIRED",
      426,
      "The DICOM deidentification policy is not current",
      {
        policy_id: DICOM_DEIDENTIFICATION_POLICY_ID,
        policy_version: DICOM_DEIDENTIFICATION_POLICY_VERSION,
      },
    );
  }
  if (!clientVersionAtLeast(input.client_version, MINIMUM_ALL_MR_CLIENT_VERSION)) {
    throw new AppError(
      "CLIENT_UPDATE_REQUIRED",
      426,
      "This client is older than the all-MR privacy contract; install the current release",
      { minimum_client_version: MINIMUM_ALL_MR_CLIENT_VERSION },
    );
  }
  if (
    input.series.some(
      (item) =>
        item.series_kind === undefined ||
        item.processing_route === undefined ||
        item.pixel_data_policy === undefined ||
        ((item.series_kind === "functional_epi") !==
          (item.processing_route === "functional-epi-v1")),
    )
  ) {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      "All-MR DICOM uploads require explicit series routing and pixel-data policy",
    );
  }
  if (
    device.self_service &&
    device.current_consent_policy_version !== PUBLIC_CONSENT_POLICY_VERSION
  ) {
    throw new AppError(
      "CONSENT_POLICY_UPDATE_REQUIRED",
      409,
      "Review and accept the current public contribution policy",
      { consent_policy_version: PUBLIC_CONSENT_POLICY_VERSION },
    );
  }
}

async function getDicomUploadForDevice(
  env: Env,
  uploadId: string,
  deviceId: string,
): Promise<UploadRow> {
  const upload = await env.DB.prepare(
    `SELECT * FROM uploads
     WHERE id = ?1 AND device_id = ?2 AND ingest_format = 'dicom-series-v1'
     LIMIT 1`,
  )
    .bind(uploadId, deviceId)
    .first<UploadRow>();
  if (!upload) throw new AppError("NOT_FOUND", 404, "Upload was not found");
  return upload;
}

async function expireStaleDicomUpload(
  env: Env,
  upload: UploadRow,
): Promise<UploadRow> {
  const timestamp = nowSeconds();
  if (
    !["created", "uploading"].includes(upload.status) ||
    dicomUploadExpiresAt(upload) > timestamp
  ) {
    return upload;
  }
  const expired = await env.DB.prepare(
    `UPDATE uploads SET status = 'expired', updated_at = ?1,
                        receipt_token = NULL, receipt_expires_at = NULL
     WHERE id = ?2 AND status IN ('created', 'uploading')
       AND COALESCE(provisional_expires_at, expires_at) <= ?1
       AND (receipt_token IS NULL OR receipt_expires_at <= ?1)
     RETURNING *`,
  )
    .bind(timestamp, upload.id)
    .first<UploadRow>();
  return expired ?? upload;
}

async function dicomSeries(
  env: Env,
  uploadId: string,
): Promise<DicomSeriesRow[]> {
  return (
    await env.DB.prepare(
      `SELECT * FROM dicom_upload_series
       WHERE upload_id = ?1 ORDER BY series_archive_id`,
    )
      .bind(uploadId)
      .all<DicomSeriesRow>()
  ).results;
}

async function dicomReconciledSeries(
  env: Env,
  uploadId: string,
): Promise<ReconciledSeriesRow[]> {
  return (
    await env.DB.prepare(
      `SELECT series_archive_id, existing_upload_id
       FROM dicom_upload_reconciled_series WHERE upload_id = ?1
       ORDER BY series_archive_id`,
    )
      .bind(uploadId)
      .all<ReconciledSeriesRow>()
  ).results;
}

function alreadyReceivedSeries(rows: ReconciledSeriesRow[]): Array<{
  series_archive_id: string;
  receipt_upload_id: string;
}> {
  return rows.map((row) => ({
    series_archive_id: row.series_archive_id,
    receipt_upload_id: row.existing_upload_id,
  }));
}

async function reconciledReceiptResponse(
  env: Env,
  upload: UploadRow,
): Promise<Record<string, unknown> | null> {
  if (
    upload.status !== "expired" ||
    upload.receipt_reconciled_at === null
  ) {
    return null;
  }
  const reconciled = await dicomReconciledSeries(env, upload.id);
  if (reconciled.length === 0) {
    throw new AppError(
      "INTERNAL",
      500,
      "Reconciled DICOM receipt has no durable series records",
    );
  }
  return {
    upload_id: upload.id,
    status: "already_received",
    format: "dicom-series-v1",
    series_count: reconciled.length,
    total_bytes: upload.total_bytes,
    already_received_series: alreadyReceivedSeries(reconciled),
  };
}

async function bestEffortPurgeReconciledReceipt(
  env: Env,
  upload: UploadRow,
): Promise<void> {
  if (upload.purged_at !== null) return;
  try {
    await deletePrefix(env, upload.archive_prefix);
    await env.DB.prepare(
      `UPDATE uploads SET purged_at = ?1, updated_at = ?1
       WHERE id = ?2 AND status = 'expired'
         AND receipt_reconciled_at IS NOT NULL AND purged_at IS NULL`,
    )
      .bind(nowSeconds(), upload.id)
      .run();
  } catch {
    // The durable receipt is already terminal. Scheduled cleanup retries this
    // prefix without making the researcher wait or retransmit bytes.
    console.warn(
      JSON.stringify({
        event: "reconciled_dicom_cleanup_pending",
        upload_id: upload.id,
      }),
    );
  }
}

async function processingSummary(
  env: Env,
  upload: UploadRow,
): Promise<Record<string, unknown>> {
  const result = await env.DB.prepare(
    `SELECT
       SUM(CASE WHEN j.status = 'queued' THEN 1 ELSE 0 END) AS queued_series,
       SUM(CASE WHEN j.status = 'processing' THEN 1 ELSE 0 END) AS processing_series,
       SUM(CASE WHEN j.status = 'processed' THEN 1 ELSE 0 END) AS processed_series,
       SUM(CASE WHEN j.status = 'failed' THEN 1 ELSE 0 END) AS failed_series,
       SUM(CASE WHEN j.input_purged_at IS NOT NULL THEN 1 ELSE 0 END) AS purged_series,
       SUM(CASE WHEN EXISTS (
                      SELECT 1 FROM released_series_reservations released
                      WHERE released.processing_job_id = j.id
                    )
                THEN 1 ELSE 0 END) AS repairable_series,
       SUM(CASE WHEN COALESCE(d.effective_processing_route, d.processing_route)
                     = 'functional-epi-v1'
                THEN 1 ELSE 0 END) AS functional_epi_series,
       SUM(CASE WHEN COALESCE(d.effective_processing_route, d.processing_route)
                     = 'archive-verify-v1'
                THEN 1 ELSE 0 END) AS archive_only_series,
       SUM(CASE WHEN COALESCE(d.effective_processing_route, d.processing_route)
                     = 'archive-verify-v1'
                     AND j.status = 'processed'
                THEN 1 ELSE 0 END) AS archive_verified_series,
       MAX(j.updated_at) AS updated_at
     FROM dicom_upload_series d
     LEFT JOIN processing_jobs j
       ON j.upload_id = d.upload_id AND j.bundle_id = d.series_archive_id
     WHERE d.upload_id = ?1`,
  )
    .bind(upload.id)
    .first<{
      queued_series: number | null;
      processing_series: number | null;
      processed_series: number | null;
      failed_series: number | null;
      purged_series: number | null;
      repairable_series: number | null;
      functional_epi_series: number | null;
      archive_only_series: number | null;
      archive_verified_series: number | null;
      updated_at: number | null;
    }>();
  const queued = Number(result?.queued_series ?? 0);
  const processing = Number(result?.processing_series ?? 0);
  const processed = Number(result?.processed_series ?? 0);
  const failed = Number(result?.failed_series ?? 0);
  const purged = Number(result?.purged_series ?? 0);
  const status =
    failed > 0
      ? "failed"
      : processed === upload.series_count
      ? "processed"
      : processing > 0
        ? "processing"
        : queued > 0
          ? "queued"
          : "queued";
  return {
    status,
    queued_series: queued,
    processing_series: processing,
    processed_series: processed,
    failed_series: failed,
    purged_series: purged,
    repairable_series: Number(result?.repairable_series ?? 0),
    functional_epi_series: Number(result?.functional_epi_series ?? 0),
    archive_only_series: Number(result?.archive_only_series ?? 0),
    archive_verified_series: Number(result?.archive_verified_series ?? 0),
    total_series: upload.series_count,
    updated_at: iso(result?.updated_at ?? upload.updated_at),
  };
}

async function dicomStatusResponse(
  env: Env,
  upload: UploadRow,
): Promise<Record<string, unknown>> {
  const received = await env.DB.prepare(
    `SELECT COALESCE(SUM(CASE WHEN completed_at IS NOT NULL
                             THEN 1 ELSE 0 END), 0) AS received_series,
            COALESCE(SUM(CASE WHEN completed_at IS NOT NULL
                             THEN expected_size ELSE 0 END), 0) AS received_bytes
     FROM dicom_upload_series WHERE upload_id = ?1`,
  )
    .bind(upload.id)
    .first<{ received_series: number; received_bytes: number }>();
  const receivedSeries = Number(received?.received_series ?? 0);
  const responseStatus =
    upload.status === "uploading" &&
    receivedSeries === upload.series_count &&
    clientVersionAtLeast(upload.client_version, MINIMUM_ALL_MR_CLIENT_VERSION)
      ? "checkpointed"
      : upload.status;
  const response: Record<string, unknown> = {
    upload_id: upload.id,
    status: responseStatus,
    format: upload.ingest_format,
    object_prefix: upload.archive_prefix,
    series_count: upload.series_count,
    total_bytes: upload.total_bytes,
    consent_policy_version: upload.consent_policy_version,
    deidentification: {
      policy_id: upload.deidentification_policy_id,
      policy_version: upload.deidentification_policy_version,
    },
    receipt: {
      received_series: receivedSeries,
      received_bytes: Number(received?.received_bytes ?? 0),
      total_series: upload.series_count,
      total_bytes: upload.total_bytes,
    },
    created_at: iso(upload.created_at),
    updated_at: iso(upload.updated_at),
  };
  if (upload.received_at !== null)
    response.received_at = iso(upload.received_at);
  if (upload.withdrawn_at !== null)
    response.withdrawn_at = iso(upload.withdrawn_at);
  if (upload.status === "committed") {
    response.processing = await processingSummary(env, upload);
  }
  const reconciled = await dicomReconciledSeries(env, upload.id);
  if (reconciled.length > 0) {
    response.already_received_series = alreadyReceivedSeries(reconciled);
  }
  return response;
}

async function ensureDicomMultipartUploads(
  env: Env,
  upload: UploadRow,
): Promise<DicomSeriesRow[]> {
  let rows = await dicomSeries(env, upload.id);
  if (rows.length !== upload.series_count) {
    throw new AppError("INTERNAL", 500, "DICOM series catalog is incomplete");
  }
  const missing = rows.filter(
    (row) => row.r2_multipart_id === null || row.part_size === null,
  );
  for (let offset = 0; offset < missing.length; offset += 8) {
    await Promise.all(
      missing.slice(offset, offset + 8).map(async (row) => {
        const objectKey = `${upload.archive_prefix}${row.archive_relative_key}`;
        let multipart: R2MultipartUpload;
        try {
          multipart = await env.ARCHIVE.createMultipartUpload(objectKey, {
            httpMetadata: { contentType: "application/zstd" },
            customMetadata: {
              upload_id: upload.id,
              series_archive_id: row.series_archive_id,
              sha256: row.expected_sha256,
              kind: "dicom_archive",
            },
          });
        } catch {
          throw new AppError(
            "STORAGE_UNAVAILABLE",
            502,
            "Unable to initialize DICOM archive upload",
          );
        }
        const partSize = multipartPartSize(row.expected_size);
        try {
          const update = await env.DB.prepare(
            `UPDATE dicom_upload_series
             SET r2_multipart_id = ?1, part_size = ?2
             WHERE upload_id = ?3 AND series_archive_id = ?4
               AND r2_multipart_id IS NULL`,
          )
            .bind(
              multipart.uploadId,
              partSize,
              upload.id,
              row.series_archive_id,
            )
            .run();
          if ((update.meta.changes ?? 0) === 0) {
            await multipart.abort().catch(() => undefined);
          }
        } catch (error) {
          await multipart.abort().catch(() => undefined);
          if (error instanceof AppError) throw error;
          throw new AppError(
            "STORAGE_UNAVAILABLE",
            502,
            "Unable to persist DICOM multipart state",
          );
        }
      }),
    );
  }
  rows = await dicomSeries(env, upload.id);
  if (
    rows.length !== upload.series_count ||
    rows.some((row) => !row.r2_multipart_id || !row.part_size)
  ) {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "DICOM multipart initialization is incomplete",
    );
  }
  return rows;
}

async function dicomCredentialsResponse(
  env: Env,
  upload: UploadRow,
): Promise<Record<string, unknown>> {
  requireSupportedClient(upload.client_version);
  const reconciledReceipt = await reconciledReceiptResponse(env, upload);
  if (reconciledReceipt) {
    await bestEffortPurgeReconciledReceipt(env, upload);
    return reconciledReceipt;
  }
  const reconciled = await dicomReconciledSeries(env, upload.id);
  const alreadyReceived = alreadyReceivedSeries(reconciled);
  if (upload.status === "committed") {
    return {
      upload_id: upload.id,
      status: "committed",
      format: upload.ingest_format,
      object_prefix: upload.archive_prefix,
      multipart_objects: [],
      ...(alreadyReceived.length > 0
        ? { already_received_series: alreadyReceived }
        : {}),
    };
  }
  if (
    upload.status === "expired" ||
    upload.status === "withdrawn" ||
    dicomUploadExpiresAt(upload) <= nowSeconds()
  ) {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Upload is no longer writable",
    );
  }
  if (
    upload.receipt_token &&
    upload.receipt_expires_at &&
    upload.receipt_expires_at > nowSeconds()
  ) {
    throw new AppError("CONFLICT", 409, "Upload receipt is in progress");
  }
  const rows = await ensureDicomMultipartUploads(env, upload);
  const pendingRows = rows.filter((row) => row.completed_at === null);
  const timestamp = nowSeconds();
  await env.DB.prepare(
    `UPDATE uploads SET status = 'uploading', updated_at = ?1,
                        last_credential_at = ?1
     WHERE id = ?2 AND status IN ('created', 'uploading')`,
  )
    .bind(timestamp, upload.id)
    .run();
  return {
    upload_id: upload.id,
    status:
      pendingRows.length === 0 &&
      clientVersionAtLeast(upload.client_version, MINIMUM_ALL_MR_CLIENT_VERSION)
        ? "checkpointed"
        : "uploading",
    format: upload.ingest_format,
    object_prefix: upload.archive_prefix,
    multipart_objects: pendingRows.map((row) => ({
      kind: "dicom_archive",
      series_archive_id: row.series_archive_id,
      key: `${upload.archive_prefix}${row.archive_relative_key}`,
      upload_id: row.r2_multipart_id,
      part_size: row.part_size,
    })),
    ...(alreadyReceived.length > 0
      ? { already_received_series: alreadyReceived }
      : {}),
  };
}

async function rawBundleHash(
  item: CreateDicomUploadRequest["series"][number],
): Promise<string> {
  // Every scientific and organizational declaration participates in duplicate
  // identity. A reused archive ID must never silently change its session,
  // protocol grouping, instance count, archive size, format, or payload hash.
  return sha256Hex(canonicalJson(item));
}

export async function createDicomUpload(
  request: Request,
  env: Env,
  input: CreateDicomUploadRequest,
): Promise<{ body: Record<string, unknown>; created: boolean }> {
  const device = await authenticateDevice(request, env);
  requireSupportedDicomContract(input, device);
  const hashes = await Promise.all(input.series.map(rawBundleHash));
  const requestHash = await sha256Hex(canonicalJson(input));
  let existing = await env.DB.prepare(
    `SELECT * FROM uploads WHERE device_id = ?1 AND request_hash = ?2 LIMIT 1`,
  )
    .bind(device.id, requestHash)
    .first<UploadRow>();
  if (existing?.ingest_format === "dicom-series-v1") {
    existing = await expireStaleDicomUpload(env, existing);
  }
  if (
    existing &&
    existing.ingest_format === "dicom-series-v1" &&
    existing.status !== "withdrawn"
  ) {
    const reconciledReceipt = await reconciledReceiptResponse(env, existing);
    if (reconciledReceipt) {
      await bestEffortPurgeReconciledReceipt(env, existing);
      return { body: reconciledReceipt, created: false };
    }
    if (existing.status === "expired") {
      // A normal timeout is retired below so this exact folder can allocate a
      // fresh session. Only a durable race reconciliation replays as success.
    } else {
      return {
        body: await dicomCredentialsResponse(env, existing),
        created: false,
      };
    }
  }
  if (existing) {
    await env.DB.prepare(
      `UPDATE uploads SET request_hash = request_hash || ':retired:' || id
       WHERE id = ?1 AND status IN ('expired', 'withdrawn')`,
    )
      .bind(existing.id)
      .run();
  }

  const reservations: ReceiptReservationRow[] = [];
  for (let offset = 0; offset < input.series.length; offset += 40) {
    const chunk = input.series.slice(offset, offset + 40);
    const placeholders = chunk.map((_, index) => `?${index + 3}`).join(", ");
    const result = await env.DB.prepare(
      `SELECT upload_id, bundle_id, series_id, bundle_hash, series_kind,
              processing_route, pixel_data_policy, withdrawn_at
       FROM received_series_reservations
       WHERE site_id = ?1 AND project_id = ?2
         AND bundle_id IN (${placeholders})`,
    )
      .bind(
        device.site_id,
        device.project_id,
        ...chunk.map((item) => item.series_archive_id),
      )
      .all<ReceiptReservationRow>();
    reservations.push(...result.results);
  }
  const reservationById = new Map(
    reservations.map((row) => [row.bundle_id, row]),
  );
  const releasedReservations: ReleasedReservationRow[] = [];
  for (let offset = 0; offset < input.series.length; offset += 40) {
    const chunk = input.series.slice(offset, offset + 40);
    const placeholders = chunk.map((_, index) => `?${index + 3}`).join(", ");
    const result = await env.DB.prepare(
      `SELECT series_archive_id, bundle_hash, release_reason, withdrawn_at
       FROM released_series_reservations
       WHERE site_id = ?1 AND project_id = ?2
         AND series_archive_id IN (${placeholders})`,
    )
      .bind(
        device.site_id,
        device.project_id,
        ...chunk.map((item) => item.series_archive_id),
      )
      .all<ReleasedReservationRow>();
    releasedReservations.push(...result.results);
  }
  const releasedReservationById = new Map(
    releasedReservations.map((row) => [row.series_archive_id, row]),
  );
  const alreadyReceived: Array<{
    series_archive_id: string;
    receipt_upload_id: string;
  }> = [];
  const pendingSeries: CreateDicomUploadRequest["series"] = [];
  const pendingHashes: string[] = [];
  input.series.forEach((item, index) => {
    const reservation = reservationById.get(item.series_archive_id);
    if (!reservation) {
      const released = releasedReservationById.get(item.series_archive_id);
      if (released?.withdrawn_at !== null && released?.withdrawn_at !== undefined) {
        throw new AppError(
          "DUPLICATE_BUNDLE",
          409,
          "DICOM series was withdrawn and remains tombstoned",
          {
            reason: "withdrawn_tombstone",
            series_archive_id: item.series_archive_id,
          },
        );
      }
      if (
        released &&
        (released.release_reason !== "STORED_OBJECT_SHA256_MISMATCH" ||
          released.bundle_hash !== hashes[index])
      ) {
        throw new AppError(
          "DUPLICATE_BUNDLE",
          409,
          "DICOM integrity replacement conflicts with the released receipt",
          {
            reason: "identity_conflict",
            series_archive_id: item.series_archive_id,
          },
        );
      }
      pendingSeries.push(item);
      pendingHashes.push(hashes[index]!);
      return;
    }
    if (reservation.withdrawn_at !== null) {
      throw new AppError(
        "DUPLICATE_BUNDLE",
        409,
        "DICOM series was withdrawn and remains tombstoned",
        {
          reason: "withdrawn_tombstone",
          series_archive_id: item.series_archive_id,
        },
      );
    }
    if (
      reservation.series_id !== item.series_id ||
      reservation.bundle_hash !== hashes[index]
    ) {
      throw new AppError(
        "DUPLICATE_BUNDLE",
        409,
        "DICOM series identity conflicts with an existing receipt",
        {
          reason: "identity_conflict",
          series_archive_id: item.series_archive_id,
        },
      );
    }
    alreadyReceived.push({
      series_archive_id: item.series_archive_id,
      receipt_upload_id: reservation.upload_id,
    });
  });
  if (
    input.series.length !== 1 &&
    input.series.some((item) =>
      releasedReservationById.has(item.series_archive_id),
    )
  ) {
    throw new AppError(
      "DUPLICATE_BUNDLE",
      409,
      "A DICOM integrity replacement must use one exact series receipt",
      {
        reason: "identity_conflict",
        series_archive_id: input.series.find((item) =>
          releasedReservationById.has(item.series_archive_id),
        )!.series_archive_id,
      },
    );
  }
  if (pendingSeries.length === 0) {
    return {
      body: {
        upload_id: alreadyReceived[0]!.receipt_upload_id,
        status: "already_received",
        format: "dicom-series-v1",
        series_count: input.series.length,
        total_bytes: input.series.reduce(
          (sum, item) => sum + item.archive.size,
          0,
        ),
        already_received_series: alreadyReceived,
      },
      created: false,
    };
  }

  const totalBytes = pendingSeries.reduce(
    (sum, item) => sum + item.archive.size,
    0,
  );
  await assertQuota(env, device, totalBytes);
  const uploadId = crypto.randomUUID();
  const prefix = `dicom/v1/${device.site_id}/${device.project_id}/${uploadId}/`;
  const timestamp = nowSeconds();
  const statements: D1PreparedStatement[] = [
    env.DB.prepare(
      `INSERT INTO uploads
         (id, site_id, project_id, device_id, status, ingest_format,
          archive_prefix, request_hash, client_version,
          consent_policy_version, deidentification_policy_id,
          deidentification_policy_version, series_count, total_bytes,
          created_at, updated_at, expires_at)
       VALUES (?1, ?2, ?3, ?4, 'created', 'dicom-series-v1', ?5, ?6,
               ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13, ?14)`,
    ).bind(
      uploadId,
      device.site_id,
      device.project_id,
      device.id,
      prefix,
      requestHash,
      input.client_version,
      device.current_consent_policy_version,
      input.deidentification.policy_id,
      input.deidentification.policy_version,
      pendingSeries.length,
      totalBytes,
      timestamp,
      timestamp + uploadTtl(env),
    ),
  ];
  pendingSeries.forEach((item, index) => {
    statements.push(
      env.DB.prepare(
        `INSERT INTO dicom_upload_series
           (upload_id, series_archive_id, series_id, subject_id, session_id,
            protocol_group_id, bundle_hash, dicom_count, series_kind,
            processing_route, effective_series_kind,
            effective_processing_route, pixel_data_policy,
            archive_relative_key, expected_size, expected_sha256)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?9, ?10,
                 ?11, ?12, ?13, ?14)`,
      ).bind(
        uploadId,
        item.series_archive_id,
        item.series_id,
        item.subject_id,
        item.session_id,
        item.protocol_group_id,
        pendingHashes[index],
        item.dicom_count,
        item.series_kind ?? "functional_epi",
        processingRoute(item),
        item.pixel_data_policy ?? "scanner-native-not-defaced",
        item.archive.relative_key,
        item.archive.size,
        item.archive.sha256,
      ),
    );
  });
  for (const reconciled of alreadyReceived) {
    statements.push(
      env.DB.prepare(
        `INSERT INTO dicom_upload_reconciled_series
           (upload_id, series_archive_id, existing_upload_id)
         VALUES (?1, ?2, ?3)`,
      ).bind(
        uploadId,
        reconciled.series_archive_id,
        reconciled.receipt_upload_id,
      ),
    );
  }
  statements.push(
    env.DB.prepare(
      `INSERT INTO audit_events
         (id, event_type, site_id, project_id, device_id, upload_id,
          subject_type, subject_id, detail_code, created_at)
       VALUES (?1, 'upload.created', ?2, ?3, ?4, ?5, 'upload', ?5,
               'dicom-series-v1', ?6)`,
    ).bind(
      crypto.randomUUID(),
      device.site_id,
      device.project_id,
      device.id,
      uploadId,
      timestamp,
    ),
  );
  try {
    await env.DB.batch(statements);
  } catch {
    const raced = await env.DB.prepare(
      `SELECT * FROM uploads WHERE device_id = ?1 AND request_hash = ?2 LIMIT 1`,
    )
      .bind(device.id, requestHash)
      .first<UploadRow>();
    if (raced?.ingest_format === "dicom-series-v1") {
      return {
        body: await dicomCredentialsResponse(env, raced),
        created: false,
      };
    }
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Unable to persist the DICOM upload session; retry the same folder",
    );
  }
  const upload = await getDicomUploadForDevice(env, uploadId, device.id);
  return { body: await dicomCredentialsResponse(env, upload), created: true };
}

async function assertQuota(
  env: Env,
  device: DeviceContext,
  requestedBytes: number,
): Promise<void> {
  if (device.upload_quota_bytes === null) return;
  const usage = await env.DB.prepare(
    `SELECT COALESCE(SUM(total_bytes), 0) AS used_bytes FROM uploads
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

export async function refreshDicomUploadCredentials(
  request: Request,
  env: Env,
  uploadId: string,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  return dicomCredentialsResponse(
    env,
    await getDicomUploadForDevice(env, uploadId, device.id),
  );
}

export async function createDicomUploadPartUrl(
  request: Request,
  env: Env,
  uploadId: string,
  input: SignPartRequest,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  const upload = await getDicomUploadForDevice(env, uploadId, device.id);
  requireSupportedClient(upload.client_version);
  const timestamp = nowSeconds();
  if (
    !["created", "uploading"].includes(upload.status) ||
    dicomUploadExpiresAt(upload) <= timestamp
  ) {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Upload is no longer writable",
    );
  }
  if (
    upload.receipt_token &&
    upload.receipt_expires_at &&
    upload.receipt_expires_at > timestamp
  ) {
    throw new AppError("CONFLICT", 409, "Upload receipt is in progress");
  }
  const row = await env.DB.prepare(
    `SELECT d.* FROM dicom_upload_series d
     JOIN uploads u ON u.id = d.upload_id
     WHERE d.upload_id = ?1 AND u.archive_prefix || d.archive_relative_key = ?2
     LIMIT 1`,
  )
    .bind(upload.id, input.key)
    .first<DicomSeriesRow>();
  if (!row?.r2_multipart_id || !row.part_size) {
    throw new AppError("OBJECT_MISSING", 404, "DICOM archive was not found");
  }
  if (row.completed_at !== null) {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "DICOM archive is already received",
    );
  }
  const partCount = Math.ceil(row.expected_size / row.part_size);
  if (input.part_number > partCount) {
    throw new AppError("OBJECT_MISMATCH", 409, "Part exceeds archive size");
  }
  const expectedSize =
    input.part_number === partCount
      ? row.expected_size - row.part_size * (partCount - 1)
      : row.part_size;
  if (input.size !== expectedSize) {
    throw new AppError("OBJECT_MISMATCH", 409, "Part size is incorrect");
  }
  return {
    ...(await presignUploadPart(env, {
      key: input.key,
      uploadId: row.r2_multipart_id,
      partNumber: input.part_number,
      size: input.size,
      sha256: input.sha256,
    })),
  };
}

function assertDicomHead(
  upload: UploadRow,
  row: DicomSeriesRow,
  head: R2Object,
): void {
  const metadata = head.customMetadata ?? {};
  if (
    head.size !== row.expected_size ||
    metadata.sha256 !== row.expected_sha256 ||
    (metadata.upload_id ?? metadata["upload-id"]) !== upload.id ||
    (metadata.series_archive_id ?? metadata["series-archive-id"]) !==
      row.series_archive_id ||
    metadata.kind !== "dicom_archive"
  ) {
    throw new AppError(
      "OBJECT_MISMATCH",
      409,
      "Stored DICOM archive metadata does not match its declaration",
      { series_archive_id: row.series_archive_id },
    );
  }
}

async function checkpointDicomObjects(
  env: Env,
  upload: UploadRow,
  input: CompleteUploadRequest,
): Promise<DicomSeriesRow[]> {
  const rows = await dicomSeries(env, upload.id);
  const reconciled = await dicomReconciledSeries(env, upload.id);
  const expectedKeys = new Set(
    rows.map((row) => `${upload.archive_prefix}${row.archive_relative_key}`),
  );
  const reconciledKeys = new Set(
    reconciled.map(
      (row) => `${upload.archive_prefix}${row.series_archive_id}/dicom.tar.zst`,
    ),
  );
  const declared = new Map(input.objects.map((object) => [object.key, object]));
  if (
    rows.length !== upload.series_count ||
    declared.size !== input.objects.length ||
    input.objects.some(
      (object) =>
        !expectedKeys.has(object.key) && !reconciledKeys.has(object.key),
    )
  ) {
    throw new AppError(
      "OBJECT_MISMATCH",
      409,
      "Completion must list each declared DICOM archive at most once",
    );
  }

  // Reconciliation is durable in D1 before this cleanup. If a duplicate
  // provisional object exists, remove it before retaining the unique objects.
  try {
    for (let offset = 0; offset < reconciled.length; offset += 8) {
      await Promise.all(
        reconciled.slice(offset, offset + 8).map((row) =>
          deleteObject(
            env,
            `${upload.archive_prefix}${row.series_archive_id}/dicom.tar.zst`,
          ),
        ),
      );
    }
  } catch {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Duplicate DICOM cleanup is pending; retry the same folder",
    );
  }

  const heads = new Map<string, R2Object>();
  for (let offset = 0; offset < rows.length; offset += 8) {
    await Promise.all(
      rows.slice(offset, offset + 8).map(async (row) => {
        const key = `${upload.archive_prefix}${row.archive_relative_key}`;
        const object = declared.get(key);
        if (
          object &&
          (object.size !== row.expected_size ||
            object.sha256 !== row.expected_sha256)
        ) {
          throw new AppError(
            "OBJECT_MISMATCH",
            409,
            "DICOM completion receipt is inconsistent",
            { series_archive_id: row.series_archive_id },
          );
        }

        let head = await env.ARCHIVE.head(key);
        if (!head && row.completed_at === null) {
          if (
            !object ||
            !row.r2_multipart_id ||
            !row.part_size ||
            object.parts.length !==
              Math.ceil(row.expected_size / row.part_size)
          ) {
            throw new AppError(
              "OBJECT_MISMATCH",
              409,
              "Completion must list every unfinished DICOM archive exactly once",
              { series_archive_id: row.series_archive_id },
            );
          }
          try {
            await env.ARCHIVE.resumeMultipartUpload(
              key,
              row.r2_multipart_id,
            ).complete(
              object.parts.map((part) => ({
                partNumber: part.part_number,
                etag: stripEtag(part.etag),
              })),
            );
          } catch {
            // The response may have been lost after R2 durably completed the
            // multipart object. Resolve that case using the authoritative HEAD.
          }
          head = await env.ARCHIVE.head(key);
        }
        if (!head) {
          throw new AppError(
            "STORAGE_UNAVAILABLE",
            502,
            "DICOM archive is temporarily unavailable after upload",
          );
        }
        assertDicomHead(upload, row, head);
        heads.set(row.series_archive_id, head);
      }),
    );
  }

  const checkpointedAt = nowSeconds();
  const statements = rows.map((row) =>
    env.DB.prepare(
      `UPDATE dicom_upload_series
       SET completed_at = COALESCE(completed_at, ?1), etag = ?2
       WHERE upload_id = ?3 AND series_archive_id = ?4`,
    ).bind(
      checkpointedAt,
      heads.get(row.series_archive_id)!.etag,
      upload.id,
      row.series_archive_id,
    ),
  );
  statements.push(
    env.DB.prepare(
      `UPDATE uploads
       SET provisional_expires_at = MAX(
             COALESCE(provisional_expires_at, 0), ?1
           ), updated_at = ?2
       WHERE id = ?3 AND status IN ('created', 'uploading')`,
    ).bind(
      checkpointedAt + PROVISIONAL_DICOM_RETENTION_SECONDS,
      checkpointedAt,
      upload.id,
    ),
  );
  await env.DB.batch(statements);
  return rows;
}

export async function checkpointDicomUpload(
  request: Request,
  env: Env,
  uploadId: string,
  input: CompleteUploadRequest,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  let upload = await getDicomUploadForDevice(env, uploadId, device.id);
  requireSupportedClient(upload.client_version);
  if (upload.status === "committed") return dicomStatusResponse(env, upload);
  if (
    upload.status === "expired" ||
    upload.status === "withdrawn" ||
    dicomUploadExpiresAt(upload) <= nowSeconds()
  ) {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Upload is no longer writable",
    );
  }
  const timestamp = nowSeconds();
  const token = crypto.randomUUID();
  const claimed = await env.DB.prepare(
    `UPDATE uploads SET receipt_token = ?1, receipt_expires_at = ?2,
                        updated_at = ?3
     WHERE id = ?4 AND ingest_format = 'dicom-series-v1'
       AND status IN ('created', 'uploading')
       AND COALESCE(provisional_expires_at, expires_at) > ?3
       AND (receipt_token IS NULL OR receipt_expires_at <= ?3)
     RETURNING *`,
  )
    .bind(token, timestamp + RECEIPT_LEASE_SECONDS, timestamp, upload.id)
    .first<UploadRow>();
  if (!claimed) {
    upload = await getDicomUploadForDevice(env, upload.id, device.id);
    return dicomStatusResponse(env, upload);
  }
  upload = claimed;
  try {
    await checkpointDicomObjects(env, upload, input);
    const released = await env.DB.prepare(
      `UPDATE uploads
       SET status = 'uploading', receipt_token = NULL,
           receipt_expires_at = NULL, updated_at = ?1
       WHERE id = ?2 AND receipt_token = ?3
       RETURNING *`,
    )
      .bind(nowSeconds(), upload.id, token)
      .first<UploadRow>();
    if (!released) {
      throw new AppError("CONFLICT", 409, "DICOM checkpoint lost its lease");
    }
    const response = await dicomStatusResponse(env, released);
    response.status = "checkpointed";
    return response;
  } catch (error) {
    await env.DB.prepare(
      `UPDATE uploads SET receipt_token = NULL, receipt_expires_at = NULL,
                          updated_at = ?1
       WHERE id = ?2 AND receipt_token = ?3`,
    )
      .bind(nowSeconds(), upload.id, token)
      .run();
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
  let upload = await getDicomUploadForDevice(env, uploadId, device.id);
  requireSupportedClient(upload.client_version);
  const replayedReceipt = await reconciledReceiptResponse(env, upload);
  if (replayedReceipt) {
    await bestEffortPurgeReconciledReceipt(env, upload);
    return replayedReceipt;
  }
  if (upload.status === "committed") return dicomStatusResponse(env, upload);
  if (
    upload.status === "expired" ||
    upload.status === "withdrawn" ||
    dicomUploadExpiresAt(upload) <= nowSeconds()
  ) {
    throw new AppError(
      "UPLOAD_NOT_WRITABLE",
      409,
      "Upload is no longer writable",
    );
  }
  const timestamp = nowSeconds();
  const token = crypto.randomUUID();
  const claimed = await env.DB.prepare(
    `UPDATE uploads SET receipt_token = ?1, receipt_expires_at = ?2,
                        updated_at = ?3
     WHERE id = ?4 AND ingest_format = 'dicom-series-v1'
       AND status IN ('created', 'uploading')
       AND COALESCE(provisional_expires_at, expires_at) > ?3
       AND (receipt_token IS NULL OR receipt_expires_at <= ?3)
     RETURNING *`,
  )
    .bind(token, timestamp + RECEIPT_LEASE_SECONDS, timestamp, upload.id)
    .first<UploadRow>();
  if (!claimed) {
    upload = await getDicomUploadForDevice(env, upload.id, device.id);
    return dicomStatusResponse(env, upload);
  }
  upload = claimed;
  try {
    const rows = await checkpointDicomObjects(env, upload, input);
    const receivedAt = nowSeconds();
    const statements: D1PreparedStatement[] = [];
    statements.push(
      env.DB.prepare(
        `INSERT INTO received_series_reservations
           (upload_id, bundle_id, site_id, project_id, series_id,
            bundle_hash, input_format, received_at, series_kind,
            processing_route, pixel_data_policy)
         SELECT d.upload_id, d.series_archive_id, u.site_id, u.project_id,
                d.series_id, d.bundle_hash, 'dicom-series-v1', ?1,
                d.series_kind, d.processing_route, d.pixel_data_policy
         FROM dicom_upload_series d
         JOIN uploads u ON u.id = d.upload_id
         JOIN devices dv ON dv.id = u.device_id
         JOIN projects p ON p.id = u.project_id
         WHERE d.upload_id = ?2 AND d.completed_at IS NOT NULL
           AND u.receipt_token = ?3
           AND dv.revoked_at IS NULL AND p.active = 1
           AND NOT EXISTS (
             SELECT 1 FROM released_series_reservations released
             WHERE released.site_id = u.site_id
               AND released.project_id = u.project_id
               AND released.series_archive_id = d.series_archive_id
               AND released.withdrawn_at IS NOT NULL
           )
           AND dv.accepted_consent_policy_version = p.consent_policy_version
           AND (
             p.consent_policy_version = u.consent_policy_version
             OR (
               u.consent_policy_version = 'open-epi-1.0.0'
               AND u.deidentification_policy_version = '1.0.0'
               AND p.consent_policy_version = 'open-mri-1.0.0'
             )
           )`,
      ).bind(receivedAt, upload.id, token),
      env.DB.prepare(
        `INSERT OR IGNORE INTO processing_jobs
           (id, upload_id, bundle_id, input_format, status, attempt,
            next_attempt_at, created_at, updated_at)
         SELECT lower(hex(randomblob(4))) || '-' ||
                lower(hex(randomblob(2))) || '-4' ||
                substr(lower(hex(randomblob(2))), 2) || '-a' ||
                substr(lower(hex(randomblob(2))), 2) || '-' ||
                lower(hex(randomblob(6))),
                d.upload_id, d.series_archive_id, 'dicom-series-v1',
                'queued', 0, ?1, ?1, ?1
         FROM dicom_upload_series d
         JOIN received_series_reservations r
           ON r.upload_id = d.upload_id AND r.bundle_id = d.series_archive_id
         JOIN uploads u ON u.id = d.upload_id
         WHERE d.upload_id = ?2 AND d.completed_at IS NOT NULL
           AND u.receipt_token = ?3`,
      ).bind(receivedAt, upload.id, token),
      env.DB.prepare(
        `UPDATE uploads
         SET status = 'committed', received_at = ?1, committed_at = ?1,
             updated_at = ?1, receipt_token = NULL, receipt_expires_at = NULL
         WHERE id = ?2 AND receipt_token = ?3
           AND ingest_format = 'dicom-series-v1'
           AND NOT EXISTS (
             SELECT 1 FROM dicom_upload_series d
             WHERE d.upload_id = uploads.id AND d.completed_at IS NOT NULL
               AND NOT EXISTS (
                 SELECT 1 FROM received_series_reservations r
                 WHERE r.upload_id = d.upload_id
                   AND r.bundle_id = d.series_archive_id
                   AND r.withdrawn_at IS NULL
               )
           )
           AND EXISTS (
             SELECT 1 FROM devices dv JOIN projects p ON p.id = uploads.project_id
             WHERE dv.id = uploads.device_id AND dv.revoked_at IS NULL
               AND p.active = 1
               AND dv.accepted_consent_policy_version = p.consent_policy_version
               AND (
                 p.consent_policy_version = uploads.consent_policy_version
                 OR (
                   uploads.consent_policy_version = 'open-epi-1.0.0'
                   AND uploads.deidentification_policy_version = '1.0.0'
                   AND p.consent_policy_version = 'open-mri-1.0.0'
                 )
               )
           )`,
      ).bind(receivedAt, upload.id, token),
      env.DB.prepare(
        `INSERT INTO audit_events
           (id, event_type, site_id, project_id, device_id, upload_id,
            subject_type, subject_id, detail_code, created_at)
         SELECT ?1, 'upload.received', ?2, ?3, ?4, ?5, 'upload', ?5,
                'dicom-series-v1', ?6
         WHERE EXISTS (
           SELECT 1 FROM uploads WHERE id = ?5 AND status = 'committed'
             AND received_at = ?6
         )`,
      ).bind(
        crypto.randomUUID(),
        upload.site_id,
        upload.project_id,
        upload.device_id,
        upload.id,
        receivedAt,
      ),
    );
    try {
      await env.DB.batch(statements);
    } catch {
      const placeholders = rows.map((_, index) => `?${index + 3}`).join(", ");
      const conflicts = await env.DB.prepare(
        `SELECT upload_id, bundle_id, series_id, bundle_hash, series_kind,
                processing_route, pixel_data_policy, withdrawn_at
         FROM received_series_reservations
         WHERE site_id = ?1 AND project_id = ?2
           AND bundle_id IN (${placeholders}) AND upload_id != ?${rows.length + 3}
         ORDER BY bundle_id`,
      )
        .bind(
          upload.site_id,
          upload.project_id,
          ...rows.map((row) => row.series_archive_id),
          upload.id,
        )
        .all<ReceiptReservationRow>();
      const conflictById = new Map(
        conflicts.results.map((row) => [row.bundle_id, row]),
      );
      const exactAll = rows.every((row) => {
        const conflict = conflictById.get(row.series_archive_id);
        return (
          conflict &&
          conflict.withdrawn_at === null &&
          conflict.series_id === row.series_id &&
          conflict.bundle_hash === row.bundle_hash
        );
      });
      if (exactAll && conflicts.results.length === rows.length) {
        const retiredAt = nowSeconds();
        const reconcileStatements = conflicts.results.map((row) =>
          env.DB.prepare(
            `INSERT OR IGNORE INTO dicom_upload_reconciled_series
               (upload_id, series_archive_id, existing_upload_id)
             VALUES (?1, ?2, ?3)`,
          ).bind(upload.id, row.bundle_id, row.upload_id),
        );
        reconcileStatements.push(
          env.DB.prepare(
            `UPDATE uploads
             SET status = 'expired', updated_at = ?1,
                 receipt_reconciled_at = ?1,
                 receipt_token = NULL, receipt_expires_at = NULL
             WHERE id = ?2 AND receipt_token = ?3`,
          ).bind(retiredAt, upload.id, token),
        );
        await env.DB.batch(reconcileStatements);
        upload = await getDicomUploadForDevice(env, upload.id, device.id);
        const response = await reconciledReceiptResponse(env, upload);
        if (!response) {
          throw new AppError(
            "STORAGE_UNAVAILABLE",
            502,
            "Concurrent DICOM receipt reconciliation is pending",
          );
        }
        return response;
      }
      const conflictingIdentity = conflicts.results.find((conflict) => {
        const row = rows.find(
          (candidate) => candidate.series_archive_id === conflict.bundle_id,
        );
        return (
          !row ||
          conflict.withdrawn_at !== null ||
          conflict.series_id !== row.series_id ||
          conflict.bundle_hash !== row.bundle_hash
        );
      });
      if (conflictingIdentity) {
        throw new AppError(
          "DUPLICATE_BUNDLE",
          409,
          conflictingIdentity.withdrawn_at !== null
            ? "A concurrently received series is withdrawn and tombstoned"
            : "A concurrently received series has a conflicting identity",
          {
            reason:
              conflictingIdentity.withdrawn_at !== null
                ? "withdrawn_tombstone"
                : "identity_conflict",
            series_archive_id: conflictingIdentity.bundle_id,
          },
        );
      }
      if (conflicts.results.length > 0) {
        const duplicateIds = new Set(
          conflicts.results.map((conflict) => conflict.bundle_id),
        );
        const uniqueRows = rows.filter(
          (row) => !duplicateIds.has(row.series_archive_id),
        );
        const reconciledAt = nowSeconds();
        const reconcileStatements: D1PreparedStatement[] = [];
        for (const conflict of conflicts.results) {
          reconcileStatements.push(
            env.DB.prepare(
              `INSERT OR IGNORE INTO dicom_upload_reconciled_series
                 (upload_id, series_archive_id, existing_upload_id)
               VALUES (?1, ?2, ?3)`,
            ).bind(upload.id, conflict.bundle_id, conflict.upload_id),
            env.DB.prepare(
              `DELETE FROM dicom_upload_series
               WHERE upload_id = ?1 AND series_archive_id = ?2`,
            ).bind(upload.id, conflict.bundle_id),
          );
        }
        reconcileStatements.push(
          env.DB.prepare(
            `UPDATE uploads
             SET series_count = ?1, total_bytes = ?2, updated_at = ?3,
                 receipt_token = NULL, receipt_expires_at = NULL
             WHERE id = ?4 AND receipt_token = ?5`,
          ).bind(
            uniqueRows.length,
            uniqueRows.reduce((sum, row) => sum + row.expected_size, 0),
            reconciledAt,
            upload.id,
            token,
          ),
        );
        await env.DB.batch(reconcileStatements);
        // End this invocation after the D1 checkpoint. Completing multipart
        // objects plus receipt-race discovery can already approach the
        // strictest Worker subrequest ceiling; the client's idempotent 502
        // retry continues cleanup and commits the unique rows in a fresh
        // invocation without retransmission.
        throw new AppError(
          "STORAGE_UNAVAILABLE",
          502,
          "Concurrent DICOM overlap was checkpointed; retry the same folder",
        );
      }
      throw new AppError(
        "STORAGE_UNAVAILABLE",
        502,
        "DICOM receipt could not be persisted; retry the same folder",
      );
    }
    upload = await getDicomUploadForDevice(env, upload.id, device.id);
    if (upload.status !== "committed") {
      throw new AppError("CONFLICT", 409, "DICOM receipt lost its lease");
    }
    return dicomStatusResponse(env, upload);
  } catch (error) {
    await env.DB.prepare(
      `UPDATE uploads SET receipt_token = NULL, receipt_expires_at = NULL,
                          updated_at = ?1
       WHERE id = ?2 AND receipt_token = ?3`,
    )
      .bind(nowSeconds(), upload.id, token)
      .run();
    throw error;
  }
}

export async function getDicomUploadStatus(
  request: Request,
  env: Env,
  uploadId: string,
): Promise<Record<string, unknown>> {
  const device = await authenticateDevice(request, env);
  const upload = await expireStaleDicomUpload(
    env,
    await getDicomUploadForDevice(env, uploadId, device.id),
  );
  return dicomStatusResponse(env, upload);
}

async function requireActiveJobLease(
  env: Env,
  jobId: string,
  leaseToken: string,
): Promise<ProcessingJobRow> {
  const job = await env.DB.prepare(
    `SELECT j.* FROM processing_jobs j
     JOIN uploads u ON u.id = j.upload_id
     WHERE j.id = ?1 AND j.status = 'processing' AND j.lease_token = ?2
       AND j.lease_expires_at > ?3 AND u.status = 'committed'
       AND u.withdrawn_at IS NULL LIMIT 1`,
  )
    .bind(jobId, leaseToken, nowSeconds())
    .first<ProcessingJobRow>();
  if (!job) {
    throw new AppError(
      "LEASE_LOST",
      409,
      "Processing job lease is no longer active",
    );
  }
  return job;
}

interface DicomJobContract {
  dicom_count: number;
  series_kind: string;
  processing_route: DicomProcessingRoute;
  pixel_data_policy: "scanner-native-not-defaced";
  deidentification_policy_version: string | null;
}

async function dicomJobContract(
  env: Env,
  job: ProcessingJobRow,
): Promise<DicomJobContract> {
  const contract = await env.DB.prepare(
    `SELECT d.dicom_count, d.series_kind, d.processing_route,
            d.pixel_data_policy, u.deidentification_policy_version
     FROM dicom_upload_series d JOIN uploads u ON u.id = d.upload_id
     WHERE d.upload_id = ?1 AND d.series_archive_id = ?2 LIMIT 1`,
  )
    .bind(job.upload_id, job.bundle_id)
    .first<DicomJobContract>();
  if (!contract) {
    throw new AppError("INTERNAL", 500, "DICOM job contract is missing");
  }
  return contract;
}

async function signedInput(
  env: Env,
  job: ProcessingJobRow,
): Promise<Record<string, unknown>> {
  if (job.input_format === "dicom-series-v1") {
    const row = await env.DB.prepare(
      `SELECT d.*, u.archive_prefix
       FROM dicom_upload_series d JOIN uploads u ON u.id = d.upload_id
       WHERE d.upload_id = ?1 AND d.series_archive_id = ?2 LIMIT 1`,
    )
      .bind(job.upload_id, job.bundle_id)
      .first<DicomSeriesRow & { archive_prefix: string }>();
    if (!row?.completed_at || !row.etag) {
      throw new AppError("INTERNAL", 500, "Received DICOM input is incomplete");
    }
    const key = `${row.archive_prefix}${row.archive_relative_key}`;
    const signed = await presignGetObject(env, key);
    return {
      series_id: row.series_id,
      series_archive_id: row.series_archive_id,
      series_kind: row.series_kind,
      processing_route: row.processing_route,
      pixel_data_policy: row.pixel_data_policy,
      input: {
        format: "dicom-tar-zstd",
        dicom_count: row.dicom_count,
        key,
        url: signed.url,
        ...(Object.keys(signed.headers).length > 0
          ? { headers: signed.headers }
          : {}),
        expires_at: signed.expires_at,
        size_bytes: row.expected_size,
        sha256: row.expected_sha256,
      },
    };
  }
  const bundle = await env.DB.prepare(
    `SELECT b.*, u.archive_prefix
     FROM upload_bundles b JOIN uploads u ON u.id = b.upload_id
     WHERE b.upload_id = ?1 AND b.bundle_id = ?2 LIMIT 1`,
  )
    .bind(job.upload_id, job.bundle_id)
    .first<{
      series_id: string;
      archive_prefix: string;
      nii_relative_key: string;
      nii_size: number;
      nii_sha256: string;
      nii_uncompressed_sha256: string;
      metadata_relative_key: string;
      metadata_size: number;
      metadata_sha256: string;
    }>();
  if (!bundle) throw new AppError("INTERNAL", 500, "NIfTI input is missing");
  const niftiKey = `${bundle.archive_prefix}${bundle.nii_relative_key}`;
  const sidecarKey = `${bundle.archive_prefix}${bundle.metadata_relative_key}`;
  const [nifti, sidecar] = await Promise.all([
    presignGetObject(env, niftiKey),
    presignGetObject(env, sidecarKey),
  ]);
  return {
    series_id: bundle.series_id,
    input: {
      nifti: {
        key: niftiKey,
        url: nifti.url,
        expires_at: nifti.expires_at,
        size_bytes: bundle.nii_size,
        sha256: bundle.nii_sha256,
        uncompressed_sha256: bundle.nii_uncompressed_sha256,
      },
      sidecar: {
        key: sidecarKey,
        url: sidecar.url,
        expires_at: sidecar.expires_at,
        size_bytes: bundle.metadata_size,
        sha256: bundle.metadata_sha256,
      },
    },
  };
}

async function processingJobClaimResponse(
  env: Env,
  job: ProcessingJobRow,
): Promise<Record<string, unknown>> {
  if (!job.lease_token || job.lease_expires_at === null) {
    throw new AppError("INTERNAL", 500, "Processing lease is incomplete");
  }
  const details = await signedInput(env, job);
  const upload = await env.DB.prepare(
    "SELECT archive_prefix, client_version FROM uploads WHERE id = ?1",
  )
    .bind(job.upload_id)
    .first<{ archive_prefix: string; client_version: string }>();
  if (!upload) {
    throw new AppError("INTERNAL", 500, "Processing upload is missing");
  }
  return {
    schema_version: "1.0.0",
    job_id: job.id,
    upload_id: job.upload_id,
    bundle_id: job.bundle_id,
    client_version: upload.client_version,
    input_format: job.input_format,
    attempt: job.attempt,
    lease_token: job.lease_token,
    lease_expires_at: iso(job.lease_expires_at),
    output_prefix: `${upload.archive_prefix}processed/${job.bundle_id}/`,
    ...details,
  };
}

export async function claimProcessingJob(
  request: Request,
  env: Env,
  input: ProcessorClaimRequest,
): Promise<Record<string, unknown> | null> {
  await authenticateProcessor(request, env);
  const timestamp = nowSeconds();
  const allMrProcessorCompatible =
    input.processor_version === REQUIRED_PROCESSOR_VERSION &&
    input.pipeline_version === REQUIRED_PROCESSOR_PIPELINE_VERSION &&
    input.controller_source_sha256 ===
      REQUIRED_PROCESSOR_CONTROLLER_SHA256;
  if (
    input.processor_version &&
    input.pipeline_version &&
    input.controller_source_sha256
  ) {
    await env.DB.prepare(
      `INSERT INTO processor_instances
         (processor_id, processor_version, pipeline_version,
          controller_source_sha256, claim_input_format, first_seen_at,
          last_seen_at)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
       ON CONFLICT(processor_id) DO UPDATE SET
         processor_version = excluded.processor_version,
         pipeline_version = excluded.pipeline_version,
         controller_source_sha256 = excluded.controller_source_sha256,
         claim_input_format = excluded.claim_input_format,
         last_seen_at = excluded.last_seen_at`,
    )
      .bind(
        input.processor_id,
        input.processor_version,
        input.pipeline_version,
        input.controller_source_sha256,
        input.claim_input_format ?? "all",
        timestamp,
      )
      .run();
  }
  // Privacy/archive/purpose rejects are first made terminal under their job
  // lease, then deleted. If storage or D1 was unavailable between those two
  // durable steps, a later claim finishes the idempotent purge before taking
  // more scientific work.
  await cleanupPendingRejectedDicomInputs(env, 4);
  await env.DB.batch([
    env.DB.prepare(
      `UPDATE processing_jobs
       SET status = 'failed', failed_at = ?1, updated_at = ?1,
           error_code = 'LEASE_EXHAUSTED',
           error_message = 'Processor lease expired too many times',
           processor_id = NULL, lease_token = NULL, lease_expires_at = NULL
       WHERE status = 'processing' AND lease_expires_at <= ?1
         AND attempt >= ?2`,
    ).bind(timestamp, MAX_PROCESSING_ATTEMPTS),
    env.DB.prepare(
      `UPDATE processing_jobs
       SET status = 'queued', next_attempt_at = ?1, updated_at = ?1,
           processor_id = NULL, lease_token = NULL, lease_expires_at = NULL
       WHERE status = 'processing' AND lease_expires_at <= ?1
         AND attempt < ?2`,
    ).bind(timestamp, MAX_PROCESSING_ATTEMPTS),
  ]);

  // Claim is a retrying POST. If D1 committed a lease but the response was
  // lost, the same processor identity must receive that exact lease again,
  // not consume a second queued job and strand the first until expiry. Extend
  // the existing lease without incrementing its attempt, then mint fresh
  // short-lived object capabilities for the replayed response.
  const replayed = await env.DB.prepare(
    `UPDATE processing_jobs
     SET lease_expires_at = ?1, updated_at = ?2
     WHERE id = (
       SELECT j.id FROM processing_jobs j
       JOIN uploads u ON u.id = j.upload_id
       WHERE j.status = 'processing' AND j.processor_id = ?3
         AND j.lease_token IS NOT NULL AND j.lease_expires_at > ?2
         AND u.status = 'committed' AND u.withdrawn_at IS NULL
         AND (?4 IS NULL OR j.input_format = ?4)
         AND (j.input_format != 'dicom-series-v1'
           OR u.deidentification_policy_version = ?5 OR ?6 = 1)
       ORDER BY j.started_at, j.id LIMIT 1
     )
     RETURNING *`,
  )
    .bind(
      timestamp + input.lease_seconds,
      timestamp,
      input.processor_id,
      input.claim_input_format ?? null,
      LEGACY_DICOM_DEIDENTIFICATION_POLICY_VERSION,
      allMrProcessorCompatible ? 1 : 0,
    )
    .first<ProcessingJobRow>();
  if (replayed) return processingJobClaimResponse(env, replayed);

  const leaseToken = crypto.randomUUID();
  const job = await env.DB.prepare(
    `UPDATE processing_jobs
     SET status = 'processing', attempt = attempt + 1,
         processor_id = ?1, lease_token = ?2, lease_expires_at = ?3,
         started_at = COALESCE(started_at, ?4), updated_at = ?4
     WHERE id = (
       SELECT j.id FROM processing_jobs j
       JOIN uploads u ON u.id = j.upload_id
       WHERE j.status = 'queued' AND j.next_attempt_at <= ?4
         AND u.status = 'committed' AND u.withdrawn_at IS NULL
         AND (?5 IS NULL OR j.input_format = ?5)
         AND (j.input_format != 'dicom-series-v1'
           OR u.deidentification_policy_version = ?6 OR ?7 = 1)
         AND NOT EXISTS (
           SELECT 1 FROM processing_jobs active
           JOIN uploads active_upload ON active_upload.id = active.upload_id
           WHERE active.status = 'processing' AND active.processor_id = ?1
             AND active.lease_token IS NOT NULL
             AND active.lease_expires_at > ?4
             AND active_upload.status = 'committed'
             AND active_upload.withdrawn_at IS NULL
         )
       ORDER BY CASE j.input_format
                  WHEN 'dicom-series-v1' THEN 0
                  ELSE 1
                END,
                j.next_attempt_at, j.created_at, j.id
       LIMIT 1
     )
     RETURNING *`,
  )
    .bind(
      input.processor_id,
      leaseToken,
      timestamp + input.lease_seconds,
      timestamp,
      input.claim_input_format ?? null,
      LEGACY_DICOM_DEIDENTIFICATION_POLICY_VERSION,
      allMrProcessorCompatible ? 1 : 0,
    )
    .first<ProcessingJobRow>();
  if (!job) return null;
  return processingJobClaimResponse(env, job);
}

export async function heartbeatProcessingJob(
  request: Request,
  env: Env,
  jobId: string,
  input: ProcessorLeaseRequest,
): Promise<Record<string, unknown>> {
  await authenticateProcessor(request, env);
  const job = await requireActiveJobLease(env, jobId, input.lease_token);
  const timestamp = nowSeconds();
  const expiresAt = timestamp + input.lease_seconds;
  const updated = await env.DB.batch([
    env.DB.prepare(
      `UPDATE processing_jobs SET lease_expires_at = ?1, updated_at = ?2
       WHERE id = ?3 AND status = 'processing' AND lease_token = ?4
         AND lease_expires_at > ?2`,
    ).bind(expiresAt, timestamp, jobId, input.lease_token),
    env.DB.prepare(
      `UPDATE processor_instances SET last_seen_at = ?1
       WHERE processor_id = ?2
         AND EXISTS (
           SELECT 1 FROM processing_jobs active
           JOIN uploads active_upload ON active_upload.id = active.upload_id
           WHERE active.id = ?3 AND active.status = 'processing'
             AND active.processor_id = ?2
             AND active.lease_token = ?4 AND active.lease_expires_at > ?1
             AND active_upload.status = 'committed'
             AND active_upload.withdrawn_at IS NULL
         )`,
    ).bind(timestamp, job.processor_id, jobId, input.lease_token),
  ]);
  if ((updated[0]?.meta.changes ?? 0) !== 1) {
    throw new AppError("LEASE_LOST", 409, "Processing job lease was lost");
  }
  return {
    job_id: jobId,
    status: "processing",
    lease_expires_at: iso(expiresAt),
  };
}

function outputFilename(kind: ProcessorOutputDescriptor["kind"]): string {
  if (kind === "nifti") return "bold.nii.gz";
  if (kind === "sidecar") return "bold.json";
  return "processing-manifest.json";
}

function descriptorMatches(
  row: ProcessingOutputRow,
  descriptor: ProcessorOutputDescriptor,
): boolean {
  return (
    row.kind === descriptor.kind &&
    row.expected_size === descriptor.size_bytes &&
    row.expected_sha256 === descriptor.sha256 &&
    row.content_type === descriptor.content_type &&
    row.uncompressed_sha256 === (descriptor.uncompressed_sha256 ?? null)
  );
}

export async function grantProcessingOutputs(
  request: Request,
  env: Env,
  jobId: string,
  input: ProcessorOutputRequest,
): Promise<Record<string, unknown>> {
  await authenticateProcessor(request, env);
  const job = await requireActiveJobLease(env, jobId, input.lease_token);
  if (job.input_format !== "dicom-series-v1") {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      "Legacy NIfTI validation jobs do not produce replacement outputs",
    );
  }
  const contract = await dicomJobContract(env, job);
  if (contract.processing_route !== "functional-epi-v1") {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      "Archive verification jobs do not produce derived outputs",
    );
  }
  const required = new Set(["nifti", "sidecar", "processing_manifest"]);
  if (
    input.outputs.length !== required.size ||
    input.outputs.some((output) => !required.delete(output.kind)) ||
    required.size !== 0
  ) {
    throw new AppError(
      "INVALID_REQUEST",
      400,
      "DICOM processing must declare NIfTI, sidecar, and processing manifest outputs",
    );
  }
  const upload = await env.DB.prepare(
    "SELECT archive_prefix FROM uploads WHERE id = ?1 LIMIT 1",
  )
    .bind(job.upload_id)
    .first<{ archive_prefix: string }>();
  if (!upload) throw new AppError("INTERNAL", 500, "Job upload is missing");
  const statements = input.outputs.map((output) => {
    const key = `${upload.archive_prefix}processed/${job.bundle_id}/${outputFilename(output.kind)}`;
    return env.DB.prepare(
      `INSERT OR IGNORE INTO processing_job_outputs
         (job_id, kind, object_key, expected_size, expected_sha256,
          content_type, uncompressed_sha256)
       SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
       WHERE EXISTS (
         SELECT 1 FROM processing_jobs j JOIN uploads u ON u.id = j.upload_id
         WHERE j.id = ?1 AND j.status = 'processing' AND j.lease_token = ?8
           AND j.lease_expires_at > ?9 AND u.status = 'committed'
           AND u.withdrawn_at IS NULL
       )`,
    ).bind(
      job.id,
      output.kind,
      key,
      output.size_bytes,
      output.sha256,
      output.content_type,
      output.uncompressed_sha256 ?? null,
      input.lease_token,
      nowSeconds(),
    );
  });
  await env.DB.batch(statements);
  await requireActiveJobLease(env, job.id, input.lease_token);
  const rows = (
    await env.DB.prepare(
      "SELECT * FROM processing_job_outputs WHERE job_id = ?1 ORDER BY kind",
    )
      .bind(job.id)
      .all<ProcessingOutputRow>()
  ).results;
  if (
    rows.length !== input.outputs.length ||
    input.outputs.some((output) => {
      const row = rows.find((item) => item.kind === output.kind);
      return !row || !descriptorMatches(row, output);
    })
  ) {
    throw new AppError(
      "CONFLICT",
      409,
      "Processing output declaration differs from its first allocation",
    );
  }
  const grants = await Promise.all(
    rows.map(async (row) => {
      const signed = await presignPutObject(env, {
        key: row.object_key,
        size: row.expected_size,
        sha256: row.expected_sha256,
        contentType: row.content_type,
        customMetadata: {
          job_id: job.id,
          kind: row.kind,
          sha256: row.expected_sha256,
        },
      });
      return { kind: row.kind, ...signed };
    }),
  );
  return { job_id: job.id, outputs: grants };
}

function assertOutputHead(
  job: ProcessingJobRow,
  row: ProcessingOutputRow,
  head: R2Object,
): void {
  const metadata = head.customMetadata ?? {};
  if (
    head.size !== row.expected_size ||
    metadata.sha256 !== row.expected_sha256 ||
    (metadata.job_id ?? metadata["job-id"]) !== job.id ||
    metadata.kind !== row.kind
  ) {
    throw new AppError(
      "OBJECT_MISMATCH",
      409,
      "Processed output metadata does not match its allocation",
      { kind: row.kind },
    );
  }
}

function allTrue(values: readonly boolean[]): boolean {
  return values.every(Boolean);
}

async function catalogStatementForJob(
  env: Env,
  job: ProcessingJobRow,
  outputs: ProcessingOutputRow[],
  timestamp: number,
  leaseToken: string,
): Promise<D1PreparedStatement> {
  if (job.input_format === "nifti-v1") {
    return env.DB.prepare(
      `INSERT INTO catalog_series
         (id, upload_id, bundle_id, site_id, project_id, series_id, subject_id,
          session_id, protocol_group_id, bundle_hash,
          nii_object_key, nii_size, nii_sha256, nii_uncompressed_sha256,
          metadata_object_key, metadata_size, metadata_sha256,
          metadata_policy_id, metadata_policy_version, committed_at)
       SELECT lower(hex(randomblob(16))), b.upload_id, b.bundle_id,
              u.site_id, u.project_id, b.series_id, b.subject_id, b.session_id,
              b.protocol_group_id, b.bundle_hash,
              u.archive_prefix || b.nii_relative_key, b.nii_size, b.nii_sha256,
              b.nii_uncompressed_sha256,
              u.archive_prefix || b.metadata_relative_key, b.metadata_size,
              b.metadata_sha256, ?3, ?4, ?5
       FROM upload_bundles b JOIN uploads u ON u.id = b.upload_id
       JOIN processing_jobs j ON j.upload_id = b.upload_id
                              AND j.bundle_id = b.bundle_id
       WHERE b.upload_id = ?1 AND b.bundle_id = ?2 AND j.id = ?6
         AND u.status = 'committed' AND u.withdrawn_at IS NULL
         AND j.status = 'processing' AND j.lease_token = ?7
         AND j.lease_expires_at > ?5`,
    ).bind(
      job.upload_id,
      job.bundle_id,
      ACTIVE_METADATA_POLICY_ID,
      ACTIVE_METADATA_POLICY_VERSION,
      timestamp,
      job.id,
      leaseToken,
    );
  }
  const nifti = outputs.find((row) => row.kind === "nifti");
  const sidecar = outputs.find((row) => row.kind === "sidecar");
  if (!nifti?.uncompressed_sha256 || !sidecar) {
    throw new AppError(
      "INTERNAL",
      500,
      "Processed output catalog is incomplete",
    );
  }
  return env.DB.prepare(
    `INSERT INTO catalog_series
       (id, upload_id, bundle_id, site_id, project_id, series_id, subject_id,
        session_id, protocol_group_id, bundle_hash,
        nii_object_key, nii_size, nii_sha256, nii_uncompressed_sha256,
        metadata_object_key, metadata_size, metadata_sha256,
        metadata_policy_id, metadata_policy_version, committed_at)
     SELECT lower(hex(randomblob(16))), d.upload_id, d.series_archive_id,
            u.site_id, u.project_id, d.series_id, d.subject_id, d.session_id,
            d.protocol_group_id, d.bundle_hash,
            ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
     FROM dicom_upload_series d JOIN uploads u ON u.id = d.upload_id
     JOIN processing_jobs j ON j.upload_id = d.upload_id
                            AND j.bundle_id = d.series_archive_id
     WHERE d.upload_id = ?1 AND d.series_archive_id = ?2 AND j.id = ?13
       AND u.status = 'committed' AND u.withdrawn_at IS NULL
       AND j.status = 'processing' AND j.lease_token = ?14
       AND j.lease_expires_at > ?12`,
  ).bind(
    job.upload_id,
    job.bundle_id,
    nifti.object_key,
    nifti.expected_size,
    nifti.expected_sha256,
    nifti.uncompressed_sha256,
    sidecar.object_key,
    sidecar.expected_size,
    sidecar.expected_sha256,
    ACTIVE_METADATA_POLICY_ID,
    ACTIVE_METADATA_POLICY_VERSION,
    timestamp,
    job.id,
    leaseToken,
  );
}

async function finalizeLegacyManifestIfReady(
  env: Env,
  job: ProcessingJobRow,
): Promise<void> {
  if (job.input_format !== "nifti-v1") return;
  const upload = await env.DB.prepare(
    `SELECT * FROM uploads WHERE id = ?1 AND status = 'committed' LIMIT 1`,
  )
    .bind(job.upload_id)
    .first<UploadRow>();
  if (!upload || upload.manifest_object_key) return;
  const pending = await env.DB.prepare(
    `SELECT COUNT(*) AS count FROM processing_jobs
     WHERE upload_id = ?1 AND status != 'processed'`,
  )
    .bind(upload.id)
    .first<number>("count");
  if (Number(pending ?? 0) !== 0) return;
  const bundles = (
    await env.DB.prepare(
      "SELECT * FROM upload_bundles WHERE upload_id = ?1 ORDER BY bundle_id",
    )
      .bind(upload.id)
      .all<{
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
      }>()
  ).results;
  const objects = (
    await env.DB.prepare(
      `SELECT object_key, bundle_id, kind, etag FROM upload_objects
       WHERE upload_id = ?1 AND completed_at IS NOT NULL AND etag IS NOT NULL`,
    )
      .bind(upload.id)
      .all<{
        object_key: string;
        bundle_id: string;
        kind: "nii" | "metadata";
        etag: string;
      }>()
  ).results;
  if (
    bundles.length !== upload.series_count ||
    objects.length !== bundles.length * 2
  ) {
    throw new AppError(
      "INTERNAL",
      500,
      "Legacy receipt manifest is incomplete",
    );
  }
  const manifestKey = `manifests/v1/${upload.site_id}/${upload.project_id}/${upload.id}.json`;
  const manifest = {
    schema_version: "scaling-neuro.archive-manifest.v1",
    upload_id: upload.id,
    site_id: upload.site_id,
    project_id: upload.project_id,
    consent_policy_version: upload.consent_policy_version,
    archive_prefix: upload.archive_prefix,
    client_version: upload.client_version,
    created_at: iso(upload.created_at),
    committed_at: iso(upload.committed_at),
    bundles: bundles.map((bundle) => {
      const nii = objects.find(
        (object) =>
          object.bundle_id === bundle.bundle_id && object.kind === "nii",
      );
      const metadata = objects.find(
        (object) =>
          object.bundle_id === bundle.bundle_id && object.kind === "metadata",
      );
      if (!nii || !metadata) {
        throw new AppError(
          "INTERNAL",
          500,
          "Legacy object receipt is incomplete",
        );
      }
      return {
        bundle_id: bundle.bundle_id,
        series_id: bundle.series_id,
        subject_id: bundle.subject_id,
        session_id: bundle.session_id,
        protocol_group_id: bundle.protocol_group_id,
        bundle_hash: bundle.bundle_hash,
        nii: {
          key: nii.object_key,
          size: bundle.nii_size,
          sha256: bundle.nii_sha256,
          uncompressed_sha256: bundle.nii_uncompressed_sha256,
          etag: nii.etag,
        },
        metadata: {
          key: metadata.object_key,
          size: bundle.metadata_size,
          sha256: bundle.metadata_sha256,
          etag: metadata.etag,
        },
      };
    }),
    control_plane: { service_version: packageManifest.version },
  };
  const payload = `${canonicalJson(manifest)}\n`;
  const manifestSha256 = await sha256Hex(payload);
  await env.ARCHIVE.put(manifestKey, utf8Bytes(payload), {
    httpMetadata: { contentType: "application/json; charset=utf-8" },
    customMetadata: { upload_id: upload.id, sha256: manifestSha256 },
  });
  await env.DB.prepare(
    `UPDATE uploads SET manifest_object_key = ?1, manifest_sha256 = ?2,
                        updated_at = ?3
     WHERE id = ?4 AND manifest_object_key IS NULL`,
  )
    .bind(manifestKey, manifestSha256, nowSeconds(), upload.id)
    .run();
}

export async function completeProcessingJob(
  request: Request,
  env: Env,
  jobId: string,
  input: ProcessorCompleteRequest,
): Promise<Record<string, unknown>> {
  await authenticateProcessor(request, env);
  const completionHash = await sha256Hex(canonicalJson(input));
  const replay = await env.DB.prepare(
    `SELECT id, upload_id, completion_hash FROM processing_jobs
     WHERE id = ?1 AND status = 'processed' LIMIT 1`,
  )
    .bind(jobId)
    .first<{
      id: string;
      upload_id: string;
      completion_hash: string | null;
    }>();
  if (replay) {
    if (replay.completion_hash !== completionHash) {
      throw new AppError(
        "CONFLICT",
        409,
        "Processing completion differs from the committed result",
      );
    }
    return {
      job_id: replay.id,
      upload_id: replay.upload_id,
      status: "processed",
    };
  }
  const job = await requireActiveJobLease(env, jobId, input.lease_token);
  let outputs: ProcessingOutputRow[] = [];
  let processingRouteForAudit: DicomProcessingRoute | null = null;
  let publishCatalog = job.input_format === "nifti-v1";
  let downgradeToArchiveVerification = false;
  if (job.input_format === "dicom-series-v1") {
    const validation = input.validation as DicomProcessorValidation;
    const contract = await dicomJobContract(env, job);
    processingRouteForAudit = contract.processing_route;
    const privacyAuditSucceeded =
      validation.dicom_privacy_audit_succeeded === true ||
      (contract.deidentification_policy_version ===
        LEGACY_DICOM_DEIDENTIFICATION_POLICY_VERSION &&
        validation.dicom_privacy_audit_succeeded === undefined);
    downgradeToArchiveVerification =
      contract.processing_route === "functional-epi-v1" &&
      validation.functional_epi_confirmed === false &&
      input.dcm2niix_version === undefined &&
      input.outputs.length === 0;
    if (
      !("archive_sha256_verified" in validation) ||
      !allTrue([
        validation.archive_sha256_verified,
        validation.dicom_parse_succeeded,
        privacyAuditSucceeded,
      ]) ||
      validation.dicom_count !== contract.dicom_count ||
      (contract.processing_route === "functional-epi-v1" &&
        !downgradeToArchiveVerification &&
        (!validation.functional_epi_confirmed || !input.dcm2niix_version)) ||
      (contract.processing_route === "archive-verify-v1" &&
        (validation.functional_epi_confirmed ||
          input.dcm2niix_version !== undefined))
    ) {
      throw new AppError(
        "OBJECT_MISMATCH",
        409,
        "DICOM scientific validation did not satisfy the processing contract",
      );
    }
    outputs = (
      await env.DB.prepare(
        "SELECT * FROM processing_job_outputs WHERE job_id = ?1 ORDER BY kind",
      )
        .bind(job.id)
        .all<ProcessingOutputRow>()
    ).results;
    if (contract.processing_route === "archive-verify-v1") {
      if (outputs.length !== 0 || input.outputs.length !== 0) {
        throw new AppError(
          "OBJECT_MISMATCH",
          409,
          "Archive verification jobs cannot publish derived outputs",
        );
      }
    } else if (downgradeToArchiveVerification) {
      if (outputs.length !== 0) {
        throw new AppError(
          "OBJECT_MISMATCH",
          409,
          "A functional-purpose downgrade cannot retain derived output allocations",
        );
      }
      processingRouteForAudit = "archive-verify-v1";
    } else {
      publishCatalog = true;
      if (
        outputs.length !== 3 ||
        input.outputs.length !== 3 ||
        input.outputs.some((descriptor) => {
          const row = outputs.find((item) => item.kind === descriptor.kind);
          return !row || !descriptorMatches(row, descriptor);
        })
      ) {
        throw new AppError(
          "OBJECT_MISMATCH",
          409,
          "Processed outputs differ from their allocation",
        );
      }
      for (const row of outputs) {
        const head = await env.ARCHIVE.head(row.object_key);
        if (!head) {
          throw new AppError(
            "OBJECT_MISSING",
            409,
            "A processed output has not reached storage",
            { kind: row.kind },
          );
        }
        assertOutputHead(job, row, head);
        row.etag = head.etag;
      }
    }
  } else {
    const validation = input.validation;
    if (
      "archive_sha256_verified" in validation ||
      !input.dcm2niix_version ||
      input.outputs.length !== 0 ||
      !allTrue([
        validation.nifti_sha256_verified,
        validation.nifti_uncompressed_sha256_verified,
        validation.sidecar_sha256_verified,
        validation.nifti_header_valid,
        validation.sidecar_valid,
        validation.nifti_sidecar_consistent,
      ])
    ) {
      throw new AppError(
        "OBJECT_MISMATCH",
        409,
        "Legacy NIfTI scientific validation did not satisfy the contract",
      );
    }
  }

  const timestamp = nowSeconds();
  const statements: D1PreparedStatement[] = outputs.map((row) =>
    env.DB.prepare(
      `UPDATE processing_job_outputs SET completed_at = ?1, etag = ?2
       WHERE job_id = ?3 AND kind = ?4
         AND EXISTS (
           SELECT 1 FROM processing_jobs j WHERE j.id = ?3
             AND j.status = 'processing' AND j.lease_token = ?5
             AND j.lease_expires_at > ?1
             AND EXISTS (
               SELECT 1 FROM uploads u WHERE u.id = j.upload_id
                 AND u.status = 'committed' AND u.withdrawn_at IS NULL
             )
         )`,
    ).bind(timestamp, row.etag, job.id, row.kind, input.lease_token),
  );
  if (downgradeToArchiveVerification) {
    statements.push(
      env.DB.prepare(
        `UPDATE dicom_upload_series
         SET effective_series_kind = 'other_mr',
             effective_processing_route = 'archive-verify-v1'
         WHERE upload_id = ?1 AND series_archive_id = ?2
           AND series_kind = 'functional_epi'
           AND processing_route = 'functional-epi-v1'
           AND EXISTS (
             SELECT 1 FROM processing_jobs j
             WHERE j.id = ?3 AND j.status = 'processing'
               AND j.lease_token = ?4 AND j.lease_expires_at > ?5
           )`,
      ).bind(
        job.upload_id,
        job.bundle_id,
        job.id,
        input.lease_token,
        timestamp,
      ),
    );
  }
  if (publishCatalog) {
    statements.push(
      await catalogStatementForJob(
        env,
        job,
        outputs,
        timestamp,
        input.lease_token,
      ),
    );
  }
  statements.push(
    env.DB.prepare(
      `UPDATE processing_jobs
       SET status = 'processed', processor_version = ?1,
           converter_version = ?2, processed_at = ?3, updated_at = ?3,
           processor_id = NULL, lease_token = NULL, lease_expires_at = NULL,
           error_code = NULL, error_message = NULL, completion_hash = ?4
       WHERE id = ?5 AND status = 'processing' AND lease_token = ?6
         AND lease_expires_at > ?3
         AND EXISTS (
           SELECT 1 FROM uploads u WHERE u.id = processing_jobs.upload_id
             AND u.status = 'committed' AND u.withdrawn_at IS NULL
         )`,
    ).bind(
      input.processor_version,
      input.dcm2niix_version ?? null,
      timestamp,
      completionHash,
      job.id,
      input.lease_token,
    ),
    env.DB.prepare(
      `INSERT INTO audit_events
         (id, event_type, upload_id, subject_type, subject_id, detail_code,
          created_at)
       SELECT ?1, 'processing.processed', ?2, 'processing_job', ?3, ?4, ?5
       WHERE EXISTS (
         SELECT 1 FROM processing_jobs j WHERE j.id = ?3
           AND j.status = 'processed' AND j.processed_at = ?5
       )`,
    ).bind(
      crypto.randomUUID(),
      job.upload_id,
      job.id,
      processingRouteForAudit ?? job.input_format,
      timestamp,
    ),
  );
  try {
    await env.DB.batch(statements);
  } catch {
    throw new AppError(
      "CONFLICT",
      409,
      "Processing result could not be committed",
    );
  }
  const completed = await env.DB.prepare(
    "SELECT status FROM processing_jobs WHERE id = ?1",
  )
    .bind(job.id)
    .first<{ status: string }>();
  if (completed?.status !== "processed") {
    throw new AppError("LEASE_LOST", 409, "Processing job lease was lost");
  }
  try {
    await finalizeLegacyManifestIfReady(env, job);
  } catch {
    console.warn(
      JSON.stringify({
        event: "legacy_manifest_finalize_pending",
        upload_id: job.upload_id,
      }),
    );
  }
  return { job_id: job.id, upload_id: job.upload_id, status: "processed" };
}

export async function failProcessingJob(
  request: Request,
  env: Env,
  jobId: string,
  input: ProcessorFailRequest,
): Promise<Record<string, unknown>> {
  await authenticateProcessor(request, env);
  const job = await requireActiveJobLease(env, jobId, input.lease_token);
  const timestamp = nowSeconds();
  const retry = input.retryable && job.attempt < MAX_PROCESSING_ATTEMPTS;
  // Each retry obtains a fresh signed GET and the processor streams and hashes
  // the object again. Only repeated full-download mismatches at the retry
  // ceiling establish that the immutable stored object itself is corrupt.
  const terminalErrorCode =
    !retry &&
    input.retryable &&
    input.error_code === "OBJECT_DOWNLOAD_INTEGRITY_MISMATCH" &&
    job.attempt >= MAX_PROCESSING_ATTEMPTS
      ? "STORED_OBJECT_SHA256_MISMATCH"
      : input.error_code;
  const terminalErrorMessage =
    terminalErrorCode === "STORED_OBJECT_SHA256_MISMATCH"
      ? terminalErrorCode
      : input.error_message;
  const nextAttemptAt = timestamp + Math.min(300, 5 * 2 ** job.attempt);
  const purgeInput =
    !retry &&
    job.input_format === "dicom-series-v1" &&
    shouldPurgeRejectedDicomInput(terminalErrorCode);
  if (purgeInput) {
    // Win the complete-vs-fail race in D1 before touching the source object.
    // Once terminal, no concurrent or stale processor can publish this job.
    const transition = await env.DB.batch([
      env.DB.prepare(
        `UPDATE processing_jobs
         SET status = 'failed', next_attempt_at = ?1, updated_at = ?1,
             failed_at = ?1, error_code = ?2, error_message = ?3,
             processor_id = NULL, lease_token = NULL, lease_expires_at = NULL
         WHERE id = ?4 AND status = 'processing' AND lease_token = ?5
           AND lease_expires_at > ?1`,
      ).bind(
        timestamp,
        terminalErrorCode,
        terminalErrorMessage,
        job.id,
        input.lease_token,
      ),
    ]);
    if ((transition[0]?.meta.changes ?? 0) !== 1) {
      throw new AppError("LEASE_LOST", 409, "Processing job lease was lost");
    }
    await purgeRejectedDicomInput(env, {
      ...job,
      status: "failed",
      error_code: terminalErrorCode,
    });
    return { job_id: job.id, status: "failed", input_status: "purged" };
  }
  const statements: D1PreparedStatement[] = [
    env.DB.prepare(
      `UPDATE processing_jobs
     SET status = ?1, next_attempt_at = ?2, updated_at = ?3,
         failed_at = CASE WHEN ?1 = 'failed' THEN ?3 ELSE NULL END,
         error_code = ?4, error_message = ?5,
         processor_id = NULL, lease_token = NULL, lease_expires_at = NULL
     WHERE id = ?6 AND status = 'processing' AND lease_token = ?7
       AND lease_expires_at > ?3`,
    ).bind(
      retry ? "queued" : "failed",
      retry ? nextAttemptAt : timestamp,
      timestamp,
      terminalErrorCode,
      terminalErrorMessage,
      job.id,
      input.lease_token,
    ),
  ];
  const results = await env.DB.batch(statements);
  if ((results[0]?.meta.changes ?? 0) !== 1) {
    throw new AppError("LEASE_LOST", 409, "Processing job lease was lost");
  }
  return {
    job_id: job.id,
    status: retry ? "queued" : "failed",
    ...(retry ? { next_attempt_at: iso(nextAttemptAt) } : {}),
  };
}

async function purgeRejectedDicomInput(
  env: Env,
  job: ProcessingJobRow,
): Promise<void> {
  if (!job.error_code || !shouldPurgeRejectedDicomInput(job.error_code)) {
    throw new AppError("INTERNAL", 500, "DICOM input is not purge-eligible");
  }
  const source = await env.DB.prepare(
    `SELECT u.archive_prefix, u.site_id, u.project_id,
            d.archive_relative_key
     FROM dicom_upload_series d
     JOIN uploads u ON u.id = d.upload_id
     WHERE d.upload_id = ?1 AND d.series_archive_id = ?2 LIMIT 1`,
  )
    .bind(job.upload_id, job.bundle_id)
    .first<{
      archive_prefix: string;
      site_id: string;
      project_id: string;
      archive_relative_key: string;
    }>();
  if (!source) {
    throw new AppError("INTERNAL", 500, "Rejected DICOM input is missing");
  }
  try {
    await deleteObject(
      env,
      `${source.archive_prefix}${source.archive_relative_key}`,
    );
  } catch {
    throw new AppError(
      "STORAGE_UNAVAILABLE",
      502,
      "Rejected DICOM input could not be purged yet",
    );
  }
  const purgedAt = nowSeconds();
  const priorIntegrityReleases =
    job.error_code === "STORED_OBJECT_SHA256_MISMATCH"
      ? Number(
          (await env.DB.prepare(
            `SELECT COUNT(*) AS count FROM released_series_reservations
             WHERE site_id = ?1 AND project_id = ?2
               AND series_archive_id = ?3`,
          )
            .bind(source.site_id, source.project_id, job.bundle_id)
            .first<number>("count")) ?? 0,
        )
      : 0;
  const releaseForOneIntegrityRetry =
    job.error_code === "STORED_OBJECT_SHA256_MISMATCH" &&
    priorIntegrityReleases === 0;
  const statements: D1PreparedStatement[] = [
    env.DB.prepare(
      `UPDATE processing_jobs
       SET input_purged_at = COALESCE(input_purged_at, ?1), updated_at = ?1
       WHERE id = ?2 AND status = 'failed' AND input_format = 'dicom-series-v1'
         AND error_code = ?3`,
    ).bind(purgedAt, job.id, job.error_code),
    env.DB.prepare(
      `INSERT INTO audit_events
         (id, event_type, upload_id, subject_type, subject_id,
          detail_code, created_at)
       SELECT ?1, 'processing.input_purged', ?2, 'processing_job', ?3,
              ?4, ?5
       WHERE EXISTS (
         SELECT 1 FROM processing_jobs
         WHERE id = ?3 AND input_purged_at IS NOT NULL
       ) AND NOT EXISTS (
         SELECT 1 FROM audit_events
         WHERE event_type = 'processing.input_purged'
           AND subject_type = 'processing_job' AND subject_id = ?3
       )`,
    ).bind(
      crypto.randomUUID(),
      job.upload_id,
      job.id,
      job.error_code,
      purgedAt,
    ),
  ];
  if (releaseForOneIntegrityRetry) {
    statements.push(
      env.DB.prepare(
        `INSERT INTO released_series_reservations
           (id, processing_job_id, upload_id, site_id, project_id,
            series_archive_id, bundle_hash, release_reason, released_at)
         SELECT ?1, ?2, r.upload_id, r.site_id, r.project_id, r.bundle_id,
                r.bundle_hash, ?3, ?4
         FROM received_series_reservations r
         WHERE r.upload_id = ?5 AND r.bundle_id = ?6
           AND r.withdrawn_at IS NULL
           AND NOT EXISTS (
             SELECT 1 FROM released_series_reservations
             WHERE processing_job_id = ?2
           )`,
      ).bind(
        crypto.randomUUID(),
        job.id,
        job.error_code,
        purgedAt,
        job.upload_id,
        job.bundle_id,
      ),
      env.DB.prepare(
        `DELETE FROM received_series_reservations
         WHERE upload_id = ?1 AND bundle_id = ?2
           AND EXISTS (
             SELECT 1 FROM released_series_reservations
             WHERE processing_job_id = ?3
           )`,
      ).bind(job.upload_id, job.bundle_id, job.id),
      env.DB.prepare(
        `UPDATE uploads
         SET request_hash = request_hash || ':integrity-retired:' || ?1,
             updated_at = ?2
         WHERE id = ?3 AND status = 'committed'
           AND EXISTS (
             SELECT 1 FROM released_series_reservations
             WHERE processing_job_id = ?1
           )`,
      ).bind(job.id, purgedAt, job.upload_id),
      env.DB.prepare(
        `INSERT INTO audit_events
           (id, event_type, upload_id, subject_type, subject_id,
            detail_code, created_at)
         SELECT ?1, 'processing.integrity_replacement_released', ?2,
                'processing_job', ?3, ?4, ?5
         WHERE EXISTS (
           SELECT 1 FROM released_series_reservations
           WHERE processing_job_id = ?3
         ) AND NOT EXISTS (
           SELECT 1 FROM audit_events
           WHERE event_type = 'processing.integrity_replacement_released'
             AND subject_type = 'processing_job' AND subject_id = ?3
         )`,
      ).bind(
        crypto.randomUUID(),
        job.upload_id,
        job.id,
        job.error_code,
        purgedAt,
      ),
    );
  } else {
    statements.push(
      env.DB.prepare(
        `UPDATE received_series_reservations
         SET withdrawn_at = COALESCE(withdrawn_at, ?1)
         WHERE upload_id = ?2 AND bundle_id = ?3`,
      ).bind(purgedAt, job.upload_id, job.bundle_id),
    );
  }
  await env.DB.batch(statements);
}

export async function cleanupPendingRejectedDicomInputs(
  env: Env,
  limit: number,
): Promise<void> {
  const errorPlaceholders = PURGE_ELIGIBLE_DICOM_ERROR_CODES.map(
    (_code, index) => `?${index + 1}`,
  ).join(", ");
  const limitPlaceholder = `?${PURGE_ELIGIBLE_DICOM_ERROR_CODES.length + 1}`;
  const candidates = (
    await env.DB.prepare(
      `SELECT * FROM processing_jobs
       WHERE status = 'failed' AND input_format = 'dicom-series-v1'
         AND input_purged_at IS NULL AND error_code IS NOT NULL
         AND error_code IN (${errorPlaceholders})
       ORDER BY failed_at, id LIMIT ${limitPlaceholder}`,
    )
      .bind(...PURGE_ELIGIBLE_DICOM_ERROR_CODES, limit)
      .all<ProcessingJobRow>()
  ).results;
  for (const job of candidates) {
    try {
      await purgeRejectedDicomInput(env, job);
    } catch {
      // One unavailable object must not globally starve unrelated jobs. Both
      // subsequent claims and the scheduled cleanup path retry each terminal
      // input independently.
      console.warn(
        JSON.stringify({
          event: "rejected_dicom_cleanup_pending",
          job_id: job.id,
          upload_id: job.upload_id,
        }),
      );
    }
  }
}

function shouldPurgeRejectedDicomInput(code: string): boolean {
  return PURGE_ELIGIBLE_DICOM_ERROR_CODE_SET.has(code);
}
