-- 0002 (N1): events, host uptime and per-mount disk usage.

-- What happened on the host while the app may have been away: alerts
-- firing and clearing, outages worked out at start, boots. `seq` never
-- repeats (AUTOINCREMENT), so it works as a sync cursor even after pruning.
-- `data` is JSON: the alert, or the outage span. Kept 90 days / 20000 rows.
CREATE TABLE events (
    seq    INTEGER PRIMARY KEY AUTOINCREMENT,
    at     INTEGER NOT NULL,
    kind   TEXT NOT NULL,
    level  TEXT NOT NULL,
    title  TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    data   TEXT
) STRICT;

CREATE INDEX events_at ON events(at);

-- Seconds the host was up, down or asleep per hour (the app's uptime_hours
-- for this host). Kept 400 days.
CREATE TABLE uptime_hours (
    hour   INTEGER PRIMARY KEY,
    up     INTEGER NOT NULL DEFAULT 0,
    down   INTEGER NOT NULL DEFAULT 0,
    asleep INTEGER NOT NULL DEFAULT 0
) STRICT;

-- Latest usage of each mount per hour (the app's disk_usage for this host),
-- for fill-up forecasts. Kept 90 days.
CREATE TABLE disk_usage (
    mount TEXT NOT NULL,
    ts    INTEGER NOT NULL,
    used  INTEGER NOT NULL,
    size  INTEGER NOT NULL,
    PRIMARY KEY (mount, ts)
) STRICT, WITHOUT ROWID;
