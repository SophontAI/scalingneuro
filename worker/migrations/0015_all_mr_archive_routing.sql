-- Expand the privacy-cleared DICOM receipt from functional EPI only to all
-- supported MR series without silently sending non-functional series through
-- the functional converter. Existing 0.3.x rows retain their exact behavior.
ALTER TABLE dicom_upload_series ADD COLUMN series_kind TEXT NOT NULL
  DEFAULT 'functional_epi'
  CHECK (length(series_kind) BETWEEN 1 AND 64);
ALTER TABLE dicom_upload_series ADD COLUMN processing_route TEXT NOT NULL
  DEFAULT 'functional-epi-v1'
  CHECK (processing_route IN ('functional-epi-v1', 'archive-verify-v1'));
ALTER TABLE dicom_upload_series ADD COLUMN pixel_data_policy TEXT NOT NULL
  DEFAULT 'scanner-native-not-defaced'
  CHECK (pixel_data_policy = 'scanner-native-not-defaced');

-- Receipt reservations are the durable duplicate/tombstone boundary. Persist
-- the routing contract there as well so an exact replay cannot lose the
-- scientific disposition after the originating upload has completed.
ALTER TABLE received_series_reservations ADD COLUMN series_kind TEXT NOT NULL
  DEFAULT 'functional_epi'
  CHECK (length(series_kind) BETWEEN 1 AND 64);
ALTER TABLE received_series_reservations ADD COLUMN processing_route TEXT NOT NULL
  DEFAULT 'functional-epi-v1'
  CHECK (processing_route IN ('functional-epi-v1', 'archive-verify-v1'));
ALTER TABLE received_series_reservations ADD COLUMN pixel_data_policy TEXT NOT NULL
  DEFAULT 'scanner-native-not-defaced'
  CHECK (pixel_data_policy = 'scanner-native-not-defaced');

CREATE INDEX dicom_upload_series_route_idx
ON dicom_upload_series(upload_id, processing_route, series_archive_id);
