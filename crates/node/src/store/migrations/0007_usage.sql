-- 0007 (N6a): per-service, per-container and per-process usage, and power,
-- as one-minute averages kept for a day.
CREATE TABLE usage_minute (
    kind     TEXT NOT NULL,
    name     TEXT NOT NULL,
    ts       INTEGER NOT NULL,
    cpu      REAL,
    mem      INTEGER,
    io_read  REAL,
    io_write REAL,
    net_rx   REAL,
    net_tx   REAL,
    watts    REAL,
    PRIMARY KEY (kind, name, ts)
) STRICT, WITHOUT ROWID;

CREATE INDEX usage_minute_ts ON usage_minute(ts);
