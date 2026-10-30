-- V9: per-site rate limiting + IP rules (issue #117)
--
-- Two new tables. Neither replaces an existing one: the `sites` table
-- is untouched and the rate-limit policy is a self-contained record
-- owned by the rate-limit module.
--
-- ## site_rate_limits
--
-- One row per site (keyed by `sites.name`). A site with no row has
-- no limits — which is the same as a row with `enabled = 0`.
--
--   enabled            master switch for this site's limits. The
--                      whitelist / blacklist rules still apply when
--                      it is 0.
--   req_limit          requests per client IP per counting window.
--                      0 disables the request dimension.
--   req_window_secs    length of the request counting window.
--   page_limit         distinct pages (path + query) per client IP
--                      per counting window. 0 disables the page
--                      dimension.
--   page_window_secs   length of the page counting window.
--   ip_capacity        number of client-IP slots in each per-site
--                      ring (request ring + page ring).
--   throttle_capacity  number of client-IP slots in the penalty box.
--   throttle_secs      how long a throttled client stays in the
--                      penalty box before it gets a fresh window.
--
-- The counters themselves are **not** stored here — they are
-- in-memory per process and reset on restart (see
-- `docs/design/rate-limit.md`). Only the configuration is persisted.
--
-- ## ip_rules
--
-- Firewall-style allow / block entries. `scope` is either the literal
-- `'global'` (applies to every site) or a `sites.name` (applies to
-- that site only). A site's effective rule set is the union of the
-- global rules and its own.
--
-- `id` is the natural key `"<scope>|<kind>|<cidr>"` — the same triple
-- cannot be stored twice, so re-adding a rule is an upsert rather
-- than a duplicate. `cidr` holds either a bare address (`1.2.3.4`)
-- or a CIDR range (`10.0.0.0/8`).

CREATE TABLE IF NOT EXISTS site_rate_limits (
    site_name         TEXT    PRIMARY KEY,
    enabled           INTEGER NOT NULL DEFAULT 0,
    req_limit         INTEGER NOT NULL DEFAULT 0,
    req_window_secs   INTEGER NOT NULL DEFAULT 60,
    page_limit        INTEGER NOT NULL DEFAULT 0,
    page_window_secs  INTEGER NOT NULL DEFAULT 3600,
    ip_capacity       INTEGER NOT NULL DEFAULT 16384,
    throttle_capacity INTEGER NOT NULL DEFAULT 65536,
    throttle_secs     INTEGER NOT NULL DEFAULT 600,
    -- Raw HTML served as the body of this site's 429 responses.
    -- NULL → the built-in default page.
    custom_429_body   TEXT,
    updated_at        TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS ip_rules (
    id         TEXT PRIMARY KEY,
    scope      TEXT NOT NULL DEFAULT 'global',
    kind       TEXT NOT NULL CHECK (kind IN ('whitelist', 'blacklist')),
    cidr       TEXT NOT NULL,
    note       TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- The hot path resolves a site's rules by scope; keep the lookup
-- index on (scope) so the reload does not scan the whole table.
CREATE INDEX IF NOT EXISTS idx_ip_rules_scope ON ip_rules(scope);
