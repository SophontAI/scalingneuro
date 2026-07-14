-- Open self-service contribution keeps uploads authenticated while removing
-- the administrator-issued invite from the normal researcher path.
ALTER TABLE projects ADD COLUMN upload_quota_bytes INTEGER
  CHECK (upload_quota_bytes IS NULL OR upload_quota_bytes > 0);

CREATE TABLE contributor_registrations (
  id TEXT PRIMARY KEY,
  site_id TEXT NOT NULL UNIQUE REFERENCES sites(id),
  project_id TEXT NOT NULL UNIQUE REFERENCES projects(id),
  device_id TEXT NOT NULL UNIQUE REFERENCES devices(id),
  request_hash TEXT NOT NULL,
  email_hash TEXT NOT NULL,
  email_ciphertext TEXT NOT NULL,
  contact_name TEXT NOT NULL,
  institution_name TEXT NOT NULL,
  institution_ror_id TEXT,
  lab_name TEXT NOT NULL,
  contact_opt_in INTEGER NOT NULL DEFAULT 0 CHECK (contact_opt_in IN (0, 1)),
  created_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX contributor_registrations_email_idx
ON contributor_registrations(email_hash);

CREATE INDEX contributor_registrations_institution_idx
ON contributor_registrations(institution_name, lab_name, created_at);

CREATE TABLE contributor_registration_limits (
  requester_window_hash TEXT PRIMARY KEY,
  attempts INTEGER NOT NULL CHECK (attempts BETWEEN 1 AND 5),
  expires_at INTEGER NOT NULL
);

CREATE INDEX contributor_registration_limits_expiry_idx
ON contributor_registration_limits(expires_at);
