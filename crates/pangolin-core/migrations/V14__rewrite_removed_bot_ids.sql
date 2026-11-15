-- V14: rewrite the per-site bot list for the crawlers the registry no
-- longer has (issue #151)
--
-- `site_rate_limit_bots` stores knownbots **registry ids**, and an id
-- the registry no longer has never matches a request (V11 says so) —
-- so a stale id leaves the row inert. Two of the three removed ids
-- are exactly that: nothing to preserve, so the rows are deleted.
--
--   google-extended         deleted — the registry has no such bot
--   cloudflare-alwaysonline deleted — the registry has no such bot
--
-- The third one carries the operator's intent and must be rewritten.
-- `applebot-extended` is gone from the registry, but a request that
-- carries `Applebot-Extended` still matches the marker `Applebot` (a
-- hyphen is a word boundary) and is now identified as `applebot`. A
-- site that deliberately listed `applebot-extended` to keep those
-- requests out of its exemptions would silently lose that intent, so
-- the row becomes a row for `applebot`.
--
-- The rewrite must not violate the primary key `(site_name,
-- bot_name)`: a site that lists **both** `applebot` and
-- `applebot-extended` must end up with one `applebot` row, not two.
-- The redundant `applebot-extended` row is therefore deleted first,
-- and only the rows that are still alone are renamed.

-- The rows with no successor: the crawlers are gone from the
-- registry and no request can match them any more.
DELETE FROM site_rate_limit_bots
 WHERE bot_name IN ('google-extended', 'cloudflare-alwaysonline');

-- The `applebot-extended` rows of the sites that already list
-- `applebot`: the rename would collide with the primary key.
DELETE FROM site_rate_limit_bots
 WHERE bot_name = 'applebot-extended'
   AND site_name IN (
       SELECT site_name FROM site_rate_limit_bots WHERE bot_name = 'applebot'
   );

-- The remaining `applebot-extended` rows become `applebot` rows.
UPDATE site_rate_limit_bots
   SET bot_name = 'applebot'
 WHERE bot_name = 'applebot-extended';
