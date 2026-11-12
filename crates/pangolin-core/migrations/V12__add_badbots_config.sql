-- V12: IP reputation configuration (issue #141)
--
-- One singleton table. The reputation check is configured **here** and
-- edited from the admin dashboard — not from `ngx.yml` and not from
-- the environment. The reason is that the only credential the feature
-- needs is a provider API key, and a key that lives in the
-- configuration file is a key that ends up in a backup, a config
-- management system and a shell history. The admin UI is already the
-- place where the operator edits global configuration, and the admin
-- UI is behind the operator's own authentication.
--
-- ## badbots_config
--
--   id                          always 1 — a CHECK constraint fails
--                               loudly on a second INSERT, the same
--                               convention the `system_config`
--                               singleton uses (V7).
--   enabled                     whether the reputation check runs at
--                               all. Default 1: the feature is on out
--                               of the box and needs no configuration.
--   evidence_policy             'strict' or 'lenient'. Default
--                               'lenient': the gateway asks the
--                               library for a second opinion about a
--                               client it has **already** decided to
--                               reject, so any report of doubt about
--                               the address is enough. 'strict' needs
--                               either a conviction-strength single
--                               observation or two corroborating
--                               families, which a single lookup
--                               cannot provide.
--   feed_refresh_interval_secs  how often the gateway asks the library
--                               to refresh the publishers' lists.
--                               Default 300. The library issues no
--                               request for a publisher whose own
--                               interval has not elapsed, so a tick
--                               that is far more frequent than the
--                               shortest publisher interval costs
--                               nothing.
--   abuseipdb_api_key           the AbuseIPDB key. NULL = the provider
--                               is not queried. A provider that is
--                               enabled without a key makes the
--                               library fail to build, so the gateway
--                               only enables the provider when the key
--                               is here.
--   crowdsec_api_key            the CrowdSec key. Same rule.
--
-- The three providers that need no credential (Spamhaus DROP,
-- DShield, the Blocklist.de service lists) are enabled by the gateway
-- itself and have no column here.
--
-- ## The keys are stored in the clear
--
-- An upstream key has to be sent to the upstream verbatim, so it
-- cannot be hashed the way `tun.token` is (V3) — that one is only
-- ever compared, never sent. The keys live in the database file
-- instead of the configuration file, and the admin UI never renders
-- them back: the form shows whether a key is configured and takes a
-- new one to replace it.
--
-- ## Bootstrap
--
-- The migration INSERTs the default row so existing deployments boot
-- with the feature on and the documented defaults, and so the reader
-- always finds a singleton — no separate seed step needed.

CREATE TABLE IF NOT EXISTS badbots_config (
    id                          INTEGER PRIMARY KEY CHECK (id = 1),
    enabled                     INTEGER NOT NULL DEFAULT 1,
    evidence_policy             TEXT    NOT NULL DEFAULT 'lenient'
        CHECK (evidence_policy IN ('strict', 'lenient')),
    feed_refresh_interval_secs  INTEGER NOT NULL DEFAULT 300,
    abuseipdb_api_key           TEXT,
    crowdsec_api_key            TEXT,
    updated_at                  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT OR IGNORE INTO badbots_config (id) VALUES (1);
