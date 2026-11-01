-- V10: per-site provider configuration + origin guard (issue #122)
--
-- ## system_config
--
-- `frontend_mode` is renamed to `provider`: the value no longer only
-- says "how is the client IP resolved" but "which front CDN / load
-- balancer is in front of this deployment" — the same value that
-- names the source of the published source-IP ranges. The two halves
-- of that answer must never disagree, so they live under one name.
--
-- The CHECK constraint travels with the rename (SQLite rewrites the
-- column reference inside it), so the allowed values are unchanged:
-- `direct`, `cloudflare`, `custom_lb`. Adding a provider value later
-- needs a separate migration that rebuilds the table — SQLite cannot
-- alter a CHECK constraint in place.
--
-- `origin_guard_enabled` is the global default of the origin guard:
-- whether a site that does not override it refuses requests whose TCP
-- peer is outside its allowlist.

ALTER TABLE system_config RENAME COLUMN frontend_mode TO provider;

ALTER TABLE system_config ADD COLUMN origin_guard_enabled INTEGER NOT NULL DEFAULT 0;

-- ## site_providers
--
-- The per-site override of the global provider settings. Every column
-- is nullable and `NULL` means "inherit the global value", so a row
-- only has to carry the fields the site actually changes. The absence
-- of a row is equivalent to a row whose columns are all `NULL`.
--
--   provider              the front CDN / load balancer in front of
--                         the site. NULL → inherit.
--   client_ip_header      the header the load balancer sets for the
--                         visitor's IP (`custom_lb` only). NULL →
--                         inherit.
--   country_header        the header for the visitor's country.
--   region_header         the header for the visitor's region.
--   origin_guard_enabled  the origin-guard switch. NULL → inherit,
--                         0 → off, 1 → on.

CREATE TABLE IF NOT EXISTS site_providers (
    site_name            TEXT    PRIMARY KEY,
    provider             TEXT,
    client_ip_header     TEXT,
    country_header       TEXT,
    region_header        TEXT,
    origin_guard_enabled INTEGER,
    updated_at           TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CHECK (provider IS NULL OR provider IN ('direct', 'cloudflare', 'custom_lb')),
    CHECK (origin_guard_enabled IS NULL OR origin_guard_enabled IN (0, 1))
);

-- ## site_origin_allow
--
-- The manual source-IP ranges of a site, unioned with the ranges its
-- provider publishes. `cidr` holds either a bare address (`1.2.3.4`)
-- or a CIDR range (`10.0.0.0/8`), in its canonical text form.

CREATE TABLE IF NOT EXISTS site_origin_allow (
    site_name TEXT NOT NULL,
    cidr      TEXT NOT NULL,
    PRIMARY KEY (site_name, cidr)
);

-- ## provider_ip_lists
--
-- The published source-IP ranges of a provider, one row per
-- `(provider, url)` so each URL keeps its own `ETag` /
-- `Last-Modified` validator — one URL changing does not re-download
-- the others.
--
--   cidrs         the canonical text form of the ranges, one per line.
--   status        `ok` after a successful fetch, `error` after a
--                 failed one. A failed fetch keeps the stored ranges
--                 in force.
--   last_error    the error text of the last failed fetch.

CREATE TABLE IF NOT EXISTS provider_ip_lists (
    provider      TEXT NOT NULL,
    url           TEXT NOT NULL,
    cidrs         TEXT NOT NULL,
    fetched_at    TEXT NOT NULL,
    etag          TEXT,
    last_modified TEXT,
    status        TEXT NOT NULL,
    last_error    TEXT,
    PRIMARY KEY (provider, url)
);

-- ## site_rate_limits.custom_403_body
--
-- The per-site custom 403 page, next to the existing custom 429 body.
-- The 403 and 429 pages are siblings: the site form already carries
-- the 429 textarea, and the proxy already holds the site's rate-limit
-- record when it answers a rejection.

ALTER TABLE site_rate_limits ADD COLUMN custom_403_body TEXT;
