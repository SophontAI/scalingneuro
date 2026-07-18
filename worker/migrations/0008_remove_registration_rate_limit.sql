-- Clear counters created by the retired open-beta registration limiter. Keep
-- the now-unused table during this deployment so the preceding Worker version
-- remains safe between migration application and the new Worker going live.
DELETE FROM contributor_registration_limits;
