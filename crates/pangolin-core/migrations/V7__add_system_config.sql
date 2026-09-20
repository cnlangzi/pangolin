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
--                    stripped of port.
--   'cloudflare' -- CF is in front. Use the CF-Connecting-IP header,
--                    falling back to the peer if the header is missing.
--   'custom_lb'  -- Operator-configured ordered list of header names;
--                    first present + non-empty value wins. The list
--                    lives in trusted_headers (JSON-encoded TEXT). When
--                    frontend_mode='custom_lb' the list must be non-empty;
--                    the resolver falls back to the peer if none match.
--
-- trusted_headers is consulted only when frontend_mode='custom_lb'.
-- The default value `["X-Real-IP"]` is the resolver's fallback when
-- the list is empty (the row can never be empty in practice because
-- the migration seeds it with this default).
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
    trusted_headers TEXT    NOT NULL DEFAULT '["X-Real-IP"]',
    updated_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT OR IGNORE INTO system_config (id, frontend_mode, trusted_headers)
VALUES (1, 'direct', '["X-Real-IP"]');