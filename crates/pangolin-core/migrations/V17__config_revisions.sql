-- V17: the revision counters of the two global singletons (issue #273)
--
-- `system_config` and `badbots_config` have **two** writers: the
-- node's own admin UI and the dashboard, which pushes the desired
-- state down the outbound channel. Both write the same singleton
-- row, so the node needs to be able to say whether the row still is
-- what the dashboard last pushed — otherwise the dashboard cannot
-- tell a clean node from a drifted one and has to push blindly.
--
--   revision        the node's own version of the row. It goes up by
--                   one when the **content** really changes; a write
--                   that changes nothing leaves it alone.
--   synced_revision the value `revision` had when the dashboard's
--                   last write landed. The two are equal if and only
--                   if the row is still exactly what the dashboard
--                   pushed.
--
-- `synced_revision = 0` means "never synchronised". A node that
-- upgrades carries that value, so the two numbers differ and the
-- dashboard pushes its desired state once after the upgrade — which
-- is what an upgraded node needs anyway, because the write semantics
-- of the API changed with it (see `docs/apis/dash.md`, v3).
--
-- The counters are **not** content: the write layer compares the
-- content fields alone when it decides whether the revision moves.

ALTER TABLE system_config ADD COLUMN revision        INTEGER NOT NULL DEFAULT 1;
ALTER TABLE system_config ADD COLUMN synced_revision INTEGER NOT NULL DEFAULT 0;

ALTER TABLE badbots_config ADD COLUMN revision        INTEGER NOT NULL DEFAULT 1;
ALTER TABLE badbots_config ADD COLUMN synced_revision INTEGER NOT NULL DEFAULT 0;
