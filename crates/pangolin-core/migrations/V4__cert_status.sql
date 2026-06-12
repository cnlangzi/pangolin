-- V4: track ACME issuance lifecycle in the `certs` table (issue #45).
--
-- Until v3 the `certs` table only recorded *completed* certificate rows
-- (manual upload, or ACME success). Pending / in-flight / failed issuances
-- lived only in the in-memory event buffer (`events.rs`), so the admin UI
-- showed a false sense of completeness and there was no way to retry
-- without an SSH/restart.
--
-- This migration adds a per-row lifecycle status so the table becomes the
-- single source of truth for every cert the system has been asked to
-- manage:
--
--   status      'pending' | 'issuing' | 'issued' | 'failed' | 'skipped'
--   started_at  ISO-8601 timestamp of the last issuance attempt (NULL for
--               purely manual uploads that never went through ACME)
--   last_error  Free-text error message for `failed`/`skipped` rows.
--               NULL when the last attempt succeeded.
--
-- Backwards compatibility: `status` defaults to 'issued' so every
-- pre-existing row migrates cleanly and renders as a completed cert in
-- the UI — exactly the behaviour the row had before this migration.
--
-- An index on `status` keeps the dashboard summary endpoint
-- (`GET /api/certs/summary`) and the status-filtered list view
-- (`GET /certs?status=…`) cheap even on instances with many domains.

ALTER TABLE certs ADD COLUMN status TEXT NOT NULL DEFAULT 'issued';
ALTER TABLE certs ADD COLUMN started_at TEXT;
ALTER TABLE certs ADD COLUMN last_error TEXT;

CREATE INDEX IF NOT EXISTS idx_certs_status ON certs(status);
