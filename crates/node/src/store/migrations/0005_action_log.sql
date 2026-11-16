-- 0005 (N4): the audit log of every action, dry or real, refused or done.
-- Append-only: rows are never updated. Each row's hash covers its fields
-- and the previous row's hash, so a removed or edited row breaks the chain.
CREATE TABLE action_log (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    at      INTEGER NOT NULL,
    source  TEXT NOT NULL,
    action  TEXT NOT NULL,
    target  TEXT NOT NULL DEFAULT '',
    dry_run INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    detail  TEXT NOT NULL DEFAULT '',
    prev    TEXT NOT NULL,
    hash    TEXT NOT NULL
) STRICT;

CREATE INDEX action_log_at ON action_log(at);
