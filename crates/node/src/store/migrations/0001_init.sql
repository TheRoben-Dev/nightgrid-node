-- 0001 (N0): node identity and metric history.
--
-- metric_points mirrors the app's table of the same name, without host_id
-- (a node only ever stores its own host). Tiers by `res`: 60 s minute points,
-- 900 s and 3600 s rollups kept as running averages. `n` is the number of
-- samples in a minute point and the number of minutes in a rollup.

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE metric_points (
    res        INTEGER NOT NULL,
    ts         INTEGER NOT NULL,
    n          INTEGER NOT NULL,
    cpu        REAL,
    mem        REAL,
    swap       REAL,
    load1      REAL,
    disk       REAL,
    net_rx     REAL,
    net_tx     REAL,
    disk_read  REAL,
    disk_write REAL,
    temp       REAL,
    gpu        REAL,
    gpu_mem    REAL,
    gpu_temp   REAL,
    gpu_power  REAL,
    PRIMARY KEY (res, ts)
) STRICT, WITHOUT ROWID;
