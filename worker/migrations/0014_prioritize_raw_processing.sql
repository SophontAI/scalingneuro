-- A 0.3 release can create new raw-DICOM work while migration 0010 has queued
-- older legacy NIfTI validation. Keep the release/user-facing raw path ahead
-- of that one-time backlog, then retain FIFO ordering within each format.
CREATE INDEX processing_jobs_claim_priority_idx
ON processing_jobs(
  status,
  CASE input_format WHEN 'dicom-series-v1' THEN 0 ELSE 1 END,
  next_attempt_at,
  created_at,
  id
);
