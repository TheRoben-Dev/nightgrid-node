-- 0006 (N4c): changes waiting for confirmation, and recent finished ones.
-- The snapshots themselves are kept by the root helper, not here.
CREATE TABLE changes (
    id        TEXT PRIMARY KEY,
    created   INTEGER NOT NULL,
    deadline  INTEGER NOT NULL,
    paths     TEXT NOT NULL,
    on_revert TEXT NOT NULL DEFAULT '',
    note      TEXT NOT NULL DEFAULT '',
    state     TEXT NOT NULL,
    detail    TEXT NOT NULL DEFAULT '',
    finished  INTEGER
) STRICT;

CREATE INDEX changes_pending ON changes(state, deadline);
