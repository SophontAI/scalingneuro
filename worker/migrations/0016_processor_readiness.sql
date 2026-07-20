CREATE TABLE processor_instances (
  processor_id TEXT PRIMARY KEY,
  processor_version TEXT NOT NULL,
  pipeline_version TEXT NOT NULL,
  controller_source_sha256 TEXT NOT NULL,
  claim_input_format TEXT NOT NULL,
  first_seen_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL
);

CREATE INDEX processor_instances_readiness_idx
  ON processor_instances (
    processor_version,
    pipeline_version,
    claim_input_format,
    last_seen_at
  );
