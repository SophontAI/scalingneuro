PRAGMA foreign_keys = ON;

CREATE TABLE sites (
  id TEXT PRIMARY KEY,
  slug TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  pseudonym_key_ciphertext TEXT NOT NULL,
  pseudonym_key_version INTEGER NOT NULL DEFAULT 1 CHECK (pseudonym_key_version > 0),
  created_at INTEGER NOT NULL
);

CREATE TABLE projects (
  id TEXT PRIMARY KEY,
  site_id TEXT NOT NULL REFERENCES sites(id),
  slug TEXT NOT NULL,
  name TEXT NOT NULL,
  consent_policy_version TEXT NOT NULL,
  active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
  created_at INTEGER NOT NULL,
  UNIQUE (site_id, slug)
);

CREATE TABLE invites (
  id TEXT PRIMARY KEY,
  site_id TEXT NOT NULL REFERENCES sites(id),
  project_id TEXT NOT NULL REFERENCES projects(id),
  code_hash TEXT NOT NULL UNIQUE,
  max_uses INTEGER NOT NULL DEFAULT 1 CHECK (max_uses BETWEEN 1 AND 100),
  uses INTEGER NOT NULL DEFAULT 0 CHECK (uses >= 0),
  expires_at INTEGER NOT NULL,
  revoked_at INTEGER,
  created_at INTEGER NOT NULL
);

CREATE TABLE devices (
  id TEXT PRIMARY KEY,
  invite_id TEXT REFERENCES invites(id),
  site_id TEXT NOT NULL REFERENCES sites(id),
  project_id TEXT NOT NULL REFERENCES projects(id),
  token_hash TEXT NOT NULL UNIQUE,
  device_name TEXT NOT NULL,
  platform TEXT NOT NULL,
  client_version TEXT NOT NULL,
  accepted_consent_policy_version TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL,
  revoked_at INTEGER
);

-- Keep invite validation and consumption inside the device INSERT transaction.
-- D1's remote SQL parser rejects RAISE() nested inside CASE, so use a guard
-- trigger followed by a consumption trigger instead.
CREATE TRIGGER reject_unavailable_invite_before_device_insert
BEFORE INSERT ON devices
WHEN NEW.invite_id IS NOT NULL AND NOT EXISTS (
  SELECT 1
  FROM invites
  WHERE id = NEW.invite_id
    AND site_id = NEW.site_id
    AND project_id = NEW.project_id
    AND revoked_at IS NULL
    AND expires_at > CAST(strftime('%s', 'now') AS INTEGER)
    AND uses < max_uses
)
BEGIN
  SELECT RAISE(ABORT, 'invite_unavailable');
END;

CREATE TRIGGER consume_invite_after_device_insert
AFTER INSERT ON devices
WHEN NEW.invite_id IS NOT NULL
BEGIN
  UPDATE invites SET uses = uses + 1 WHERE id = NEW.invite_id;
END;

CREATE TABLE uploads (
  id TEXT PRIMARY KEY,
  site_id TEXT NOT NULL REFERENCES sites(id),
  project_id TEXT NOT NULL REFERENCES projects(id),
  device_id TEXT NOT NULL REFERENCES devices(id),
  status TEXT NOT NULL CHECK (status IN ('created', 'uploading', 'committed', 'expired', 'withdrawn')),
  archive_prefix TEXT NOT NULL UNIQUE,
  request_hash TEXT NOT NULL,
  client_version TEXT NOT NULL,
  consent_policy_version TEXT NOT NULL,
  series_count INTEGER NOT NULL CHECK (series_count > 0),
  total_bytes INTEGER NOT NULL CHECK (total_bytes > 0),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  last_credential_at INTEGER,
  committed_at INTEGER,
  withdrawn_at INTEGER,
  purged_at INTEGER,
  manifest_object_key TEXT,
  manifest_sha256 TEXT,
  UNIQUE (device_id, request_hash)
);

CREATE INDEX uploads_cleanup_idx ON uploads(status, expires_at);
CREATE INDEX uploads_project_idx ON uploads(site_id, project_id, created_at);

CREATE TABLE upload_bundles (
  upload_id TEXT NOT NULL REFERENCES uploads(id) ON DELETE CASCADE,
  bundle_id TEXT NOT NULL,
  series_id TEXT NOT NULL,
  subject_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  bundle_hash TEXT NOT NULL,
  nii_relative_key TEXT NOT NULL,
  nii_size INTEGER NOT NULL CHECK (nii_size > 0),
  nii_sha256 TEXT NOT NULL,
  nii_uncompressed_sha256 TEXT NOT NULL,
  metadata_relative_key TEXT NOT NULL,
  metadata_size INTEGER NOT NULL CHECK (metadata_size > 0),
  metadata_sha256 TEXT NOT NULL,
  PRIMARY KEY (upload_id, bundle_id),
  UNIQUE (upload_id, nii_relative_key),
  UNIQUE (upload_id, metadata_relative_key)
);

CREATE INDEX upload_bundles_series_idx ON upload_bundles(series_id, bundle_hash);

CREATE TABLE upload_objects (
  upload_id TEXT NOT NULL REFERENCES uploads(id) ON DELETE CASCADE,
  object_key TEXT NOT NULL,
  bundle_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('nii', 'metadata')),
  expected_size INTEGER NOT NULL CHECK (expected_size > 0),
  expected_sha256 TEXT NOT NULL,
  r2_multipart_id TEXT UNIQUE,
  part_size INTEGER,
  completed_at INTEGER,
  etag TEXT,
  PRIMARY KEY (upload_id, object_key),
  UNIQUE (upload_id, bundle_id, kind)
);

CREATE INDEX upload_objects_pending_idx ON upload_objects(upload_id, completed_at);

CREATE TABLE catalog_series (
  id TEXT PRIMARY KEY,
  upload_id TEXT NOT NULL REFERENCES uploads(id),
  bundle_id TEXT NOT NULL,
  site_id TEXT NOT NULL REFERENCES sites(id),
  project_id TEXT NOT NULL REFERENCES projects(id),
  series_id TEXT NOT NULL,
  subject_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  bundle_hash TEXT NOT NULL,
  nii_object_key TEXT NOT NULL,
  nii_size INTEGER NOT NULL,
  nii_sha256 TEXT NOT NULL,
  nii_uncompressed_sha256 TEXT NOT NULL,
  metadata_object_key TEXT NOT NULL,
  metadata_size INTEGER NOT NULL,
  metadata_sha256 TEXT NOT NULL,
  committed_at INTEGER NOT NULL,
  withdrawn_at INTEGER
);

CREATE UNIQUE INDEX catalog_series_dedup_idx
ON catalog_series(site_id, project_id, series_id, bundle_hash);

CREATE INDEX catalog_series_subject_idx
ON catalog_series(site_id, project_id, subject_id, session_id)
WHERE withdrawn_at IS NULL;

CREATE TABLE audit_events (
  id TEXT PRIMARY KEY,
  event_type TEXT NOT NULL,
  site_id TEXT,
  project_id TEXT,
  device_id TEXT,
  upload_id TEXT,
  subject_type TEXT,
  subject_id TEXT,
  detail_code TEXT,
  created_at INTEGER NOT NULL
);

CREATE INDEX audit_events_created_idx ON audit_events(created_at);
