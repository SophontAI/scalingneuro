-- A completed R2 multipart object is a durable transfer checkpoint, but it is
-- not yet a scientific receipt.  Keep these provisional objects long enough
-- for very large folder-level uploads to finish their final source-stability
-- gate without depending on R2's seven-day multipart lifetime.
ALTER TABLE uploads ADD COLUMN provisional_expires_at INTEGER;

CREATE INDEX uploads_provisional_cleanup_idx
ON uploads(status, provisional_expires_at);
