-- herdr-relay schema (v4). Apply with: see the 部署 section of relay/README.md
-- (POST the file contents to the D1 query endpoint).

-- v4: the relay is outbound-only. The whole inbound pipeline is gone — the
-- webhook queue, the door-drop log and the topic bindings all served reply
-- routing, which the plugin no longer has.
DROP TABLE IF EXISTS updates;
DROP TABLE IF EXISTS drops;
DROP TABLE IF EXISTS meta;

-- Audit of every device API call that reached Telegram, successes and failures
-- alike. idx_api_calls_created backs the cron's retention DELETE: without it
-- that delete scans the whole table every day (~20k rows, and DELETEs count
-- against the row-read budget), which is how the daily quota got burned through
-- in diagnosis.
CREATE TABLE IF NOT EXISTS api_calls (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    device      TEXT NOT NULL,
    method      TEXT NOT NULL,
    params      TEXT,               -- the request params, JSON-encoded
    ok          INTEGER NOT NULL,   -- 1 = Telegram accepted the call
    error_code  INTEGER,            -- Telegram's error_code when ok = 0
    description TEXT,               -- Telegram's description when ok = 0
    duration_ms INTEGER,            -- Worker → Telegram round trip
    created_at  INTEGER NOT NULL    -- unix seconds; aged out after 30d
);
CREATE INDEX IF NOT EXISTS idx_api_calls_created ON api_calls (created_at);
