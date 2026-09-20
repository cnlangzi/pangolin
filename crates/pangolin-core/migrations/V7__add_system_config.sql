-- V7: system-level configuration (fix-client-ip)
--
-- A single-row table holding the operator's declaration of what's in
-- front of pangolin (a reverse proxy, CDN, or load balancer) so the
-- access log / bot log can record the *real* visitor IP instead of the
-- TCP peer addr (which is just the hop closest to us).
--
-- ## Why a singleton (id INTEGER PRIMARY KEY CHECK (id = 1)) and not
-- a TEXT PK like the other tables?
--
-- system_config is by definition global, not a collection. A CHECK
-- constraint fails loudly on any second INSERT, exactly matching our
-- intent. The refinery-managed history table itself is a singleton
-- with a surrogate PK; this is precedent enough to override the
-- "natural TEXT PK" convention used by sites / domains / tun / certs /
-- dns_providers.
--
-- ## frontend_mode values
--
--   'direct'     -- TCP peer is the visitor. Use session.client_addr(),
--                    stripped of port. `trusted_headers` is ignored.
--   'cloudflare' -- CF is in front. Use the CF-Connecting-IP header,
--                    falling back to the peer if the header is missing.
--                    `trusted_headers` is ignored.
--   'custom_lb'  -- Operator-configured ordered list of header names;
--                    first present + non-empty value wins. The list
--                    lives in trusted_headers (JSON-encoded TEXT). The
--                    admin POST handler validates that the list is
--                    non-empty when this mode is selected; if it is
--                    ever read as empty the resolver expands it to
--                    `["X-Real-IP"]` as a built-in safety net.
--
-- trusted_headers is consulted only when frontend_mode='custom_lb'.
-- The default value `[]` (empty array) matches the 'direct' mode that
-- the row is seeded with: in direct mode the column is irrelevant,
-- and an empty literal makes it obvious that no headers are trusted.
--
-- ## Bootstrap
--
-- The migration INSERTs the default row so existing deployments boot
-- with sensible defaults and the resolver always finds a singleton —
-- no separate seed step needed. The CHECK on id prevents drift if a
-- future migration accidentally tries to insert a second row.

CREATE TABLE IF NOT EXISTS system_config (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    frontend_mode   TEXT    NOT NULL DEFAULT 'direct'
        CHECK (frontend_mode IN ('direct', 'cloudflare', 'custom_lb')),
    trusted_headers TEXT    NOT NULL DEFAULT '[]',
    updated_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT OR IGNORE INTO system_config (id, frontend_mode, trusted_headers)
VALUES (1, 'direct', '[]');