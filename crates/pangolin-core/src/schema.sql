-- Pangolin SQLite schema. Mirrors README.md "数据模型" section.
-- All five tables use TEXT primary keys (natural keys, no surrogate ids).
-- No intermediate tables (tun_domains was removed; site.backend prefix
-- is the single source of truth for routing).

CREATE TABLE IF NOT EXISTS sites (
    name        TEXT PRIMARY KEY,            -- business name, e.g. 'customer-web'
    backend     TEXT NOT NULL,               -- '[tun_name:]url'
    enabled     INTEGER NOT NULL DEFAULT 1,
    host_mode   TEXT NOT NULL DEFAULT 'passthrough',  -- backend|passthrough|custom
    host_custom TEXT,                         -- custom Host value when host_mode=custom
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS domains (
    domain      TEXT PRIMARY KEY,            -- 'example.com' or '*.example.com'
    site_name   TEXT NOT NULL,               -- references sites.name (logical FK)
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_domains_site ON domains(site_name);

CREATE TABLE IF NOT EXISTS tun (
    name             TEXT PRIMARY KEY,       -- tun_name, e.g. 'office'
    enabled          INTEGER NOT NULL DEFAULT 1,
    online           INTEGER NOT NULL DEFAULT 0,
    registered_at    TEXT,
    last_seen_at     TEXT
);

CREATE TABLE IF NOT EXISTS tokens (
    token        TEXT PRIMARY KEY,            -- token string itself
    enabled      INTEGER NOT NULL DEFAULT 1,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at   TEXT                         -- NULL = never expires
);

CREATE TABLE IF NOT EXISTS certs (
    domain            TEXT PRIMARY KEY,           -- 1:1 with domain (may be a SAN, e.g. www.example.com)
    cert_file         TEXT NOT NULL,             -- path to blob file (key+cert combined)
    key_file          TEXT NOT NULL,             -- path to blob file (same as cert_file in blob layout)
    expires_at        TEXT,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    sans              TEXT NOT NULL DEFAULT '[]',        -- JSON array of SANs
    source            TEXT NOT NULL DEFAULT 'manual',     -- 'acme' | 'manual'
    acme_dns_provider TEXT,                              -- cloudflare | aliyun | tencent
    acme_account_id   TEXT,                              -- ACME account URL or id
    issued_at         INTEGER NOT NULL DEFAULT 0          -- Unix timestamp seconds
);
