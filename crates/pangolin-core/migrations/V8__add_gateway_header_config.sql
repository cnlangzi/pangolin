-- V8: unified P-* gateway header configuration (issue #115)
--
-- Adds the four knobs the gateway-header contract needs and drops the
-- old `trusted_headers` list:
--
--   pgl_headers_enabled  -- master switch for the four `P-*` headers
--   client_ip_header     -- first-level header carrying the visitor IP
--   client_country_header-- first-level header carrying the country code
--   client_region_header -- first-level header carrying the region code
--
-- The three `*_header` columns apply to `frontend_mode='custom_lb'`
-- only: in `cloudflare` mode the built-in CF-* names are used and in
-- `direct` mode the TCP peer is the visitor. Each column is
-- independent — an unset column yields an empty value, never a
-- fallback to another column.
--
-- ## Why the one-time data move
--
-- `trusted_headers` was an ordered list of header names consulted in
-- `custom_lb` mode; the new model names exactly one header per
-- value. Dropping the column without a copy would silently blank the
-- client IP of every existing `custom_lb` deployment, so the first
-- entry of the old list is carried over once, here. `X-Real-IP` is
-- the seed when the old list was empty — it is the historical
-- default for this mode (and, unlike `X-Forwarded-For`, it is set by
-- a load balancer rather than by pangolin itself).
--
-- The copy is guarded by `json_valid` so a hand-edited row with a
-- malformed value cannot abort the migration.

ALTER TABLE system_config ADD COLUMN pgl_headers_enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE system_config ADD COLUMN client_ip_header TEXT;
ALTER TABLE system_config ADD COLUMN client_country_header TEXT;
ALTER TABLE system_config ADD COLUMN client_region_header TEXT;

UPDATE system_config
   SET client_ip_header = COALESCE(
           CASE WHEN json_valid(trusted_headers)
                THEN json_extract(trusted_headers, '$[0]')
                END,
           'X-Real-IP')
 WHERE id = 1;

ALTER TABLE system_config DROP COLUMN trusted_headers;
