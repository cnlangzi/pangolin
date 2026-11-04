-- V11: per-site known-bot rate-limit list (issue #131)
--
-- One table. The `site_rate_limits` table keeps its current columns —
-- the bot list is list-shaped per-site data, the same shape as
-- `site_origin_allow(site_name, cidr)` added by the provider
-- migration, so it gets its own table rather than a JSON / CSV
-- column.
--
-- ## site_rate_limit_bots
--
-- The registry ids of the known bots that are **not** exempt from a
-- site's rate limits. A bot in the list is counted against the site's
-- limits and answered `429` when it is over them; every other
-- verified bot stays exempt.
--
--   site_name     logical reference to sites.name
--   bot_name      a knownbots registry id (`googlebot`, `gptbot`, …)
--
-- A site with no row in this table exempts every verified bot — the
-- behaviour of the code before this migration. An empty list is
-- therefore the default and needs no row.
--
-- The id is the same string the JSONL bot log writes as `bot_name`
-- and the same string the `P-Known-Bot` gateway header carries. An id
-- that is not in the knownbots registry never matches a request: only
-- a bot the verifier knows can produce a verified bot.

CREATE TABLE IF NOT EXISTS site_rate_limit_bots (
    site_name TEXT NOT NULL,
    bot_name  TEXT NOT NULL,
    PRIMARY KEY (site_name, bot_name)
);
