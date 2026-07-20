-- A repeatedly hash-mismatched stored object may be replaced once without
-- erasing the failed receipt's audit lineage. Intrinsic privacy/archive
-- violations continue to use permanent withdrawn tombstones.
CREATE TABLE released_series_reservations (
  id TEXT PRIMARY KEY,
  processing_job_id TEXT NOT NULL UNIQUE,
  upload_id TEXT NOT NULL,
  site_id TEXT NOT NULL,
  project_id TEXT NOT NULL,
  series_archive_id TEXT NOT NULL,
  bundle_hash TEXT NOT NULL,
  release_reason TEXT NOT NULL,
  released_at INTEGER NOT NULL,
  withdrawn_at INTEGER
);

CREATE UNIQUE INDEX released_series_reservations_identity_idx
ON released_series_reservations(
  site_id,
  project_id,
  series_archive_id
);
