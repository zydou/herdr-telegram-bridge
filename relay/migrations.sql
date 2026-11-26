-- herdr-relay schema. Apply with: see the 部署 section of relay/README.md
-- (POST the file contents to the D1 query endpoint).

-- Audit of every device API call that reached Telegram, successes and failures
-- alike. idx_api_calls_created backs the cron's retention DELETE: without it
-- the delete scans the whole table every day, and scans count against the
-- row-read budget.
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
