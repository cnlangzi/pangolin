-- V2: merge the `tokens` table into `tun`.
--
-- Tokens were historically kept in a separate table so they could be
-- "decoupled" from tuns and reused across many tuns. In practice the
-- codebase never exercised that decoupling: a tun presents (token, name)
-- and both must match a row in their respective tables, which is
-- effectively 1:1. The two-table model forced a 401-vs-403 distinction
-- and required operators to register both halves before a tun could
-- come online.
--
-- The merged schema treats a tun as a single row carrying its own
-- auth credential. `token` is now a column on `tun`; `expires_at` keeps
-- the per-tun token expiry that used to live on `tokens`.
--
-- v2 auth model: the (name, token) pair is admin-provisioned via
-- `POST /api/tun` BEFORE the tun starts. The WS server runs a
-- single SQL lookup; no row → 401 → reject. There is no auto-register
-- path: presenting a valid token for an unknown name does NOT
-- create the row, because the admin is the sole source of truth
-- for "this tun exists".
--
-- This migration does NOT preserve existing data (per the v2 design
-- decision that legacy rows are out of scope). Operators upgrading must
-- re-add their tuns with `POST /api/tun {name, token, ...}`.

-- Step 1: drop the now-redundant tokens table.
DROP TABLE IF EXISTS tokens;

-- Step 2: add the credential columns to `tun`.
-- `token` is NOT NULL with a default of '' so existing rows (if any)
-- pass the NOT NULL constraint; new rows must always provide a real
-- token via the admin API.
ALTER TABLE tun ADD COLUMN token TEXT NOT NULL DEFAULT '';
ALTER TABLE tun ADD COLUMN expires_at TEXT;
