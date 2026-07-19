-- Scanner-native, deidentified DICOM archives are received without asking the
-- edge control plane to read or convert scientific payloads. Existing rows
-- remain legacy NIfTI uploads and continue through the same public upload ID.
ALTER TABLE uploads ADD COLUMN ingest_format TEXT NOT NULL DEFAULT 'nifti-v1'
  CHECK (ingest_format IN ('nifti-v1', 'dicom-series-v1'));
ALTER TABLE uploads ADD COLUMN received_at INTEGER;
ALTER TABLE uploads ADD COLUMN receipt_token TEXT;
ALTER TABLE uploads ADD COLUMN receipt_expires_at INTEGER;
ALTER TABLE uploads ADD COLUMN deidentification_policy_id TEXT;
ALTER TABLE uploads ADD COLUMN deidentification_policy_version TEXT;

CREATE INDEX uploads_receipt_lease_idx
ON uploads(status, receipt_expires_at);

CREATE TABLE dicom_upload_series (
  upload_id TEXT NOT NULL REFERENCES uploads(id) ON DELETE CASCADE,
  series_archive_id TEXT NOT NULL,
  series_id TEXT NOT NULL,
  subject_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  protocol_group_id TEXT NOT NULL,
  bundle_hash TEXT NOT NULL,
  dicom_count INTEGER NOT NULL CHECK (dicom_count > 0),
  archive_relative_key TEXT NOT NULL,
  expected_size INTEGER NOT NULL CHECK (expected_size > 0),
  expected_sha256 TEXT NOT NULL,
  r2_multipart_id TEXT UNIQUE,
  part_size INTEGER,
  completed_at INTEGER,
  etag TEXT,
  PRIMARY KEY (upload_id, series_archive_id),
  UNIQUE (upload_id, series_id),
  UNIQUE (upload_id, archive_relative_key)
);

CREATE INDEX dicom_upload_series_receipt_idx
ON dicom_upload_series(upload_id, completed_at);

CREATE TABLE dicom_upload_reconciled_series (
  upload_id TEXT NOT NULL REFERENCES uploads(id) ON DELETE CASCADE,
  series_archive_id TEXT NOT NULL,
  existing_upload_id TEXT NOT NULL REFERENCES uploads(id),
  PRIMARY KEY (upload_id, series_archive_id)
);

CREATE TABLE processing_jobs (
  id TEXT PRIMARY KEY,
  upload_id TEXT NOT NULL REFERENCES uploads(id) ON DELETE CASCADE,
  bundle_id TEXT NOT NULL,
  input_format TEXT NOT NULL
    CHECK (input_format IN ('nifti-v1', 'dicom-series-v1')),
  status TEXT NOT NULL
    CHECK (status IN ('queued', 'processing', 'processed', 'failed')),
  attempt INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
  next_attempt_at INTEGER NOT NULL,
  processor_id TEXT,
  lease_token TEXT,
  lease_expires_at INTEGER,
  processor_version TEXT,
  converter_version TEXT,
  error_code TEXT,
  error_message TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  started_at INTEGER,
  processed_at INTEGER,
  failed_at INTEGER,
  input_purged_at INTEGER,
  UNIQUE (upload_id, bundle_id)
);

CREATE INDEX processing_jobs_claim_idx
ON processing_jobs(status, next_attempt_at, created_at);

CREATE INDEX processing_jobs_upload_idx
ON processing_jobs(upload_id, status);

CREATE TABLE processing_job_outputs (
  job_id TEXT NOT NULL REFERENCES processing_jobs(id) ON DELETE CASCADE,
  kind TEXT NOT NULL
    CHECK (kind IN ('nifti', 'sidecar', 'processing_manifest')),
  object_key TEXT NOT NULL UNIQUE,
  expected_size INTEGER NOT NULL CHECK (expected_size > 0),
  expected_sha256 TEXT NOT NULL,
  content_type TEXT NOT NULL,
  uncompressed_sha256 TEXT,
  completed_at INTEGER,
  etag TEXT,
  PRIMARY KEY (job_id, kind)
);

CREATE INDEX processing_job_outputs_job_idx
ON processing_job_outputs(job_id, completed_at);

-- Receipt reservations close the interval between durable transfer and
-- scientific catalog publication. They prevent two workstations from queuing
-- the same stable series identity while catalog_series remains intentionally
-- processor-gated.
CREATE TABLE received_series_reservations (
  upload_id TEXT NOT NULL REFERENCES uploads(id) ON DELETE CASCADE,
  bundle_id TEXT NOT NULL,
  site_id TEXT NOT NULL REFERENCES sites(id),
  project_id TEXT NOT NULL REFERENCES projects(id),
  series_id TEXT NOT NULL,
  bundle_hash TEXT NOT NULL,
  input_format TEXT NOT NULL
    CHECK (input_format IN ('nifti-v1', 'dicom-series-v1')),
  received_at INTEGER NOT NULL,
  withdrawn_at INTEGER,
  PRIMARY KEY (upload_id, bundle_id)
);

CREATE UNIQUE INDEX received_series_reservation_identity_idx
ON received_series_reservations(site_id, project_id, bundle_id);

-- Seed reservations for already-published archives so new raw clients cannot
-- queue a duplicate while the catalog predates this migration.
INSERT OR IGNORE INTO received_series_reservations
  (upload_id, bundle_id, site_id, project_id, series_id, bundle_hash,
   input_format, received_at, withdrawn_at)
SELECT c.upload_id, c.bundle_id, c.site_id, c.project_id, c.series_id,
       c.bundle_hash, 'nifti-v1', c.committed_at, c.withdrawn_at
FROM catalog_series c;

-- SQL-safe recovery for legacy transfers whose authoritative object receipts
-- were fully checkpointed before deployment. Only the owner of every stable
-- reservation is committed and enqueued; duplicate jobs are impossible.
INSERT OR IGNORE INTO received_series_reservations
  (upload_id, bundle_id, site_id, project_id, series_id, bundle_hash,
   input_format, received_at)
SELECT b.upload_id, b.bundle_id, u.site_id, u.project_id, b.series_id,
       b.bundle_hash, 'nifti-v1', u.updated_at
FROM upload_bundles b
JOIN uploads u ON u.id = b.upload_id
WHERE u.status = 'uploading'
  AND (SELECT COUNT(*) FROM upload_objects o
       WHERE o.upload_id = u.id AND o.completed_at IS NOT NULL
         AND o.etag IS NOT NULL) = u.series_count * 2;

INSERT OR IGNORE INTO processing_jobs
  (id, upload_id, bundle_id, input_format, status, attempt,
   next_attempt_at, created_at, updated_at)
SELECT lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) ||
       '-4' || substr(lower(hex(randomblob(2))), 2) || '-a' ||
       substr(lower(hex(randomblob(2))), 2) || '-' ||
       lower(hex(randomblob(6))),
       b.upload_id, b.bundle_id, 'nifti-v1',
       'queued', 0, u.updated_at, u.updated_at, u.updated_at
FROM upload_bundles b
JOIN uploads u ON u.id = b.upload_id
JOIN received_series_reservations r
  ON r.upload_id = b.upload_id AND r.bundle_id = b.bundle_id
WHERE u.status = 'uploading'
  AND (SELECT COUNT(*) FROM upload_objects o
       WHERE o.upload_id = u.id AND o.completed_at IS NOT NULL
         AND o.etag IS NOT NULL) = u.series_count * 2;

UPDATE uploads
SET status = 'committed', received_at = updated_at, committed_at = updated_at,
    operation_token = NULL, operation_kind = NULL,
    operation_expires_at = NULL
WHERE status = 'uploading' AND ingest_format = 'nifti-v1'
  AND (SELECT COUNT(*) FROM received_series_reservations r
       WHERE r.upload_id = uploads.id) = series_count
  AND (SELECT COUNT(*) FROM processing_jobs j
       WHERE j.upload_id = uploads.id) = series_count;
