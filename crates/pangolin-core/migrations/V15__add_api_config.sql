-- V15: the dashboard-facing management API's runtime state (issue #185)
--
-- The API's listen address is configuration (`api.listen` in
-- `ngx.yml`), but everything about *who* is allowed to call it is
-- runtime state an operator must be able to change without a
-- restart: the bearer token, the instance id the dashboard knows
-- this node by, and whether the API is enabled at all.
--
-- The table is a **singleton** — `id` is pinned to 1 by a CHECK
-- constraint, so there is exactly one row per gateway and a second
-- INSERT cannot create a rival configuration.
--
-- `token_hash` holds the SHA-256 hex digest of the bearer token,
-- never the token itself (the same convention `tun.token_hash`
-- uses since V3): a database dump must not hand the dashboard's
-- credential to whoever reads it.
--
-- `dashboard_ip` is learned by the node from the source address of
-- the inbound API connection; `dashboard_port` is operator-supplied.
-- Neither belongs in `ngx.yml` — the operator configures the API's
-- own port, not the dashboard's.

CREATE TABLE IF NOT EXISTS api_config (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    instance_id     TEXT    NOT NULL,
    token_hash      TEXT    NOT NULL,
    dashboard_port  INTEGER NOT NULL DEFAULT 0,
    dashboard_ip    TEXT,
    enabled         INTEGER NOT NULL DEFAULT 0,
    updated_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
