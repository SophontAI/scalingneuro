ALTER TABLE devices ADD COLUMN enrollment_id TEXT;

-- Existing devices predate client-generated enrollment operations and remain
-- NULL. New operations are globally unique so a replay cannot be rebound to
-- another invite, project, or device.
CREATE UNIQUE INDEX devices_enrollment_id_unique_idx
ON devices(enrollment_id)
WHERE enrollment_id IS NOT NULL;
