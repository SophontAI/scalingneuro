DROP INDEX IF EXISTS uploads_cluster_launch_idx;
DROP INDEX IF EXISTS processor_instances_readiness_idx;
DROP INDEX IF EXISTS processing_job_outputs_job_idx;
DROP INDEX IF EXISTS processing_jobs_claim_priority_idx;
DROP INDEX IF EXISTS processing_jobs_claim_idx;
DROP INDEX IF EXISTS processing_jobs_upload_idx;
DROP INDEX IF EXISTS released_series_reservations_identity_idx;

DROP TABLE IF EXISTS processor_instances;
DROP TABLE IF EXISTS processing_job_outputs;
DROP TABLE IF EXISTS released_series_reservations;
DROP TABLE IF EXISTS processing_jobs;

ALTER TABLE uploads DROP COLUMN cluster_launch_dispatched_at;
