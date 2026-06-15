-- V5: explicit error classification + per-row retry schedule + order persistence
-- (issue: cert renewal hammering Let's Encrypt even on permanent failures).
--
-- Until V4, the only signal a renewal failure carried was the free-text
-- `last_error` string. The renewal loop (a fixed 6h ticker) treated every
-- failure the same way: re-run `ensure_one` for the row. The visible
-- symptoms on the operator dashboard were:
--
--   1. Permanent errors (rejectedIdentifier, invalid, unauthorized, caa)
--      are retried forever, even though no number of retries will ever
--      succeed without operator intervention (fix the domain, the DNS,
--      or the CA policy).
--   2. Rate-limit errors (HTTP 429, ACME `rateLimited`) are retried on
--      the same 6h schedule, contributing to the rate-limit counter
--      that the server has explicitly told us to back off from.
--   3. The server returns a `Retry-After` hint or a "retry after
--      2026-06-15 19:28:55 UTC" detail string, but the loop ignores it.
--
-- V5 adds the four columns that close all three gaps:
--
--   next_retry_at  Earliest UTC timestamp at which the renewal loop
--                  should attempt this row again. The loop consults
--                  this column instead of relying on a fixed ticker.
--                  NULL means "no scheduled retry" (e.g. fresh Pending
--                  row, or successfully issued).
--
--   error_class    Serialized CertErrorClass (one of "transient",
--                  "permanent", or "rate_limited:<rfc3339>"). Drives
--                  both the UI badge (rate-limited is a distinct color
--                  from permanent) and the renewal loop's retry policy
--                  (Permanent rows are not retried; RateLimited rows
--                  are retried only after `next_retry_at`; Transient
--                  rows use the backoff schedule).
--
--   attempt_count  Monotonic counter of how many times this row has
--                  gone through `ensure_one` in the current failure
--                  streak. Reset to 0 on a successful Issued transition.
--                  Used to pick the right slot in the backoff schedule.
--
--   order_url      instant-acme Order URL, set after the order is
--                  created and persisted across restarts so the loop
--                  can resume polling an in-flight order instead of
--                  opening a fresh one. Cleared on Issued or on
--                  recover_stuck_issuing_rows.
--
-- The index on `next_retry_at` is what the new per-row loop uses to
-- find "the next row that's due" in O(log n).

ALTER TABLE certs ADD COLUMN next_retry_at TEXT;
ALTER TABLE certs ADD COLUMN error_class TEXT;
ALTER TABLE certs ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE certs ADD COLUMN order_url TEXT;

CREATE INDEX IF NOT EXISTS idx_certs_next_retry ON certs(next_retry_at);
