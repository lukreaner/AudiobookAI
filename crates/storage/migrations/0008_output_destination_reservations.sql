-- An export destination is a durable, exclusive job resource.  Reservations
-- are retained once promotion begins so a restart can distinguish its own
-- partially promoted output from another job's files. A failed/cancelled job
-- may release only a still-reserved row; a completed job releases a promoted
-- row because the finished path itself prevents reuse until the user removes
-- it. Promoting rows remain fail-closed.
CREATE TABLE output_destination_reservations (
    job_id TEXT PRIMARY KEY NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    destination_key TEXT NOT NULL COLLATE NOCASE UNIQUE,
    destination_path TEXT NOT NULL,
    layout TEXT NOT NULL CHECK (layout IN ('single_file', 'per_chapter')),
    state TEXT NOT NULL CHECK (state IN ('reserved', 'promoting', 'promoted')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    promoted_at TEXT
);

CREATE INDEX output_destination_reservations_project_idx
    ON output_destination_reservations(project_id, created_at DESC);
