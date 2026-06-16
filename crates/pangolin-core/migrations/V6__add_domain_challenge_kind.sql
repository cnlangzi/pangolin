-- V6: per-domain ACME challenge_kind (issue #55)
--
-- Each domain may explicitly pick one of three ACME challenge kinds:
--   'http-01'          HTTP-01 (file-based) — default fallback
--   'dns-01'           DNS-01  (TXT at _acme-challenge.<domain>)
--   'dns-persist-01'   dns-persist-01 (persistent TXT at _validation-persist.<domain>)
--   NULL               per-domain auto default: dns-01 if a DNS provider is
--                      linked, else http-01 (computed at plan_issuance time)
--
-- The column is intentionally nullable:
--   * Adding a NOT NULL default would force a one-way migration
--     (existing rows would be locked to that kind) and would surprise
--     operators who expect the auto default.
--   * The application reads the effective kind through
--     `domain.effective_challenge_kind(...)` and applies the
--     NULL -> auto-default rule uniformly, so the same code path covers
--     both explicit and auto modes.
--
-- Wildcard x http-01 is rejected at both save time
-- (`mutate::handle_create` / `mutate::handle_edit`) and at issue time
-- (`plan_issuance`) per RFC 8555 section 8.3 -- the server simply does
-- not offer an http-01 challenge for a wildcard identifier.
--
-- Backwards compatibility: this is an additive ALTER TABLE ADD COLUMN
-- with no DEFAULT, so every pre-existing row has challenge_kind = NULL
-- and recovers automatically under the auto-default rule. No data
-- backfill is required.

ALTER TABLE domains ADD COLUMN challenge_kind TEXT
    CHECK (challenge_kind IS NULL OR challenge_kind IN ('http-01', 'dns-01', 'dns-persist-01'));
