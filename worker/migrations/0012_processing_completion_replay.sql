-- The processor's completion POST can succeed in D1 while its HTTP response is
-- lost. Persist the canonical request identity so that exact retries return
-- the same terminal success and the processor can safely remove local cache.
ALTER TABLE processing_jobs ADD COLUMN completion_hash TEXT;
