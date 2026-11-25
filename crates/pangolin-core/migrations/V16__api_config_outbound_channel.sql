-- V16: the outbound dashboard channel (issue #262)
--
-- The node no longer exposes a management port. It dials **out** to
-- the dashboard, keeps an SSE subscription open and posts the result
-- of every command back — so the table changes shape:
--
--   * `token_hash` is gone. The node signs its requests with
--     **HMAC-SHA256** under the token, so it needs the token itself:
--     a digest cannot produce a signature. The old column held a
--     one-way hash, so it cannot be turned back into the token — an
--     upgraded node has to be given a new token (see the note below).
--   * `dashboard_port` and `dashboard_ip` are gone. The dashboard's
--     address is not learned from an inbound call any more; the node
--     dials a configured base address.
--   * `enabled` is gone. There is no switch: the channel is as good
--     as the token, and a node without a token simply does not dial.
--   * `dash_api` is new: the dashboard's API base address the node
--     dials. An empty value means "use the built-in default".
--
-- The row keeps its `instance_id` and its `updated_at`: the id the
-- dashboard knows this node by does not change with the transport.
--
-- UPGRADE NOTE: the old `token_hash` was a one-way hash of the
-- bearer token, so the token cannot be recovered from it. After this
-- migration the node has **no token** and does not dial the dashboard
-- until the dashboard hands the operator a fresh one and the operator
-- pastes it into the "Dashboard" card on the settings page.

CREATE TABLE api_config_v16 (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    instance_id TEXT    NOT NULL,
    token       TEXT    NOT NULL DEFAULT '',
    dash_api    TEXT    NOT NULL DEFAULT '',
    updated_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO api_config_v16 (id, instance_id, token, dash_api, updated_at)
    SELECT id, instance_id, '', '', updated_at FROM api_config WHERE id = 1;

DROP TABLE api_config;

ALTER TABLE api_config_v16 RENAME TO api_config;
