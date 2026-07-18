-- Multipart completion and scientific validation are distinct durability
-- boundaries. Existing completed_at values remain valid R2 object receipts;
-- only the new Worker can establish atomic NIfTI/sidecar verification marks.
ALTER TABLE upload_objects ADD COLUMN verified_at INTEGER;

CREATE INDEX upload_objects_verified_idx
  ON upload_objects(upload_id, verified_at);
