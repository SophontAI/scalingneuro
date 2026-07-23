ALTER TABLE dicom_upload_series
  RENAME COLUMN processing_route TO archive_route;

ALTER TABLE dicom_upload_series
  RENAME COLUMN effective_processing_route TO effective_archive_route;

ALTER TABLE received_series_reservations
  RENAME COLUMN processing_route TO archive_route;
