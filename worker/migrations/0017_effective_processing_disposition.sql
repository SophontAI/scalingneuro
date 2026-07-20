-- Keep the uploader's immutable/archive-bound declaration separate from the
-- processor's independently verified effective disposition.
ALTER TABLE dicom_upload_series ADD COLUMN effective_series_kind TEXT;
ALTER TABLE dicom_upload_series ADD COLUMN effective_processing_route TEXT;

UPDATE dicom_upload_series
SET effective_series_kind = series_kind,
    effective_processing_route = processing_route;

CREATE INDEX dicom_upload_series_effective_route_idx
ON dicom_upload_series(
  upload_id,
  effective_processing_route,
  series_archive_id
);
