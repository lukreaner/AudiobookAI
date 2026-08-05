-- Durable, project-scoped production segments and take history.  The original
-- `segments` table is retained for backwards compatibility; it was never
-- populated by the conversion worker.  New job/usage references use the
-- explicitly named proof segment columns below.

CREATE TABLE proofing_projects (
    project_id TEXT PRIMARY KEY NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    source_conversion_job_id TEXT REFERENCES jobs(id) ON DELETE SET NULL,
    plan_revision INTEGER NOT NULL CHECK (plan_revision >= 0),
    plan_hash TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('ready', 'dirty', 'incomplete')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE production_segments (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    chapter_id TEXT REFERENCES chapters(id) ON DELETE CASCADE,
    paragraph_id TEXT REFERENCES paragraphs(id) ON DELETE CASCADE,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('epub_range', 'opening_credit', 'closing_credit')),
    stable_key TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    source_content_hash TEXT NOT NULL,
    byte_start INTEGER CHECK (byte_start IS NULL OR byte_start >= 0),
    byte_end INTEGER CHECK (byte_end IS NULL OR byte_end >= byte_start),
    speaker_key TEXT NOT NULL,
    expected_input_hash TEXT NOT NULL,
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    review_state TEXT NOT NULL CHECK (review_state IN ('unreviewed', 'flagged', 'approved', 'locked')),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE UNIQUE INDEX production_segments_active_key_idx
    ON production_segments(project_id, stable_key) WHERE active = 1;
CREATE INDEX production_segments_chapter_ordinal_idx
    ON production_segments(project_id, chapter_id, active, ordinal);
CREATE INDEX production_segments_review_idx
    ON production_segments(project_id, active, review_state);

CREATE TABLE segment_takes (
    id TEXT PRIMARY KEY NOT NULL,
    segment_id TEXT NOT NULL REFERENCES production_segments(id) ON DELETE CASCADE,
    artifact_id TEXT NOT NULL UNIQUE REFERENCES artifacts(id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    source_job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE RESTRICT,
    source_job_unit_id TEXT NOT NULL REFERENCES job_units(id) ON DELETE RESTRICT,
    semantic_input_hash TEXT NOT NULL,
    duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
    created_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    UNIQUE(segment_id, ordinal)
);

CREATE INDEX segment_takes_segment_created_idx
    ON segment_takes(segment_id, created_at DESC);
CREATE INDEX segment_takes_job_idx ON segment_takes(source_job_id);

CREATE TABLE segment_selections (
    segment_id TEXT PRIMARY KEY NOT NULL REFERENCES production_segments(id) ON DELETE CASCADE,
    take_id TEXT NOT NULL REFERENCES segment_takes(id) ON DELETE RESTRICT,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    selected_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE performance_overrides (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    segment_id TEXT NOT NULL UNIQUE REFERENCES production_segments(id) ON DELETE CASCADE,
    source_content_hash TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE proof_export_snapshots (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    job_id TEXT NOT NULL UNIQUE REFERENCES jobs(id) ON DELETE CASCADE,
    plan_revision INTEGER NOT NULL CHECK (plan_revision >= 0),
    plan_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

ALTER TABLE job_units
    ADD COLUMN proof_segment_id TEXT REFERENCES production_segments(id) ON DELETE CASCADE;

CREATE INDEX job_units_proof_segment_idx ON job_units(proof_segment_id);

ALTER TABLE usage_ledger
    ADD COLUMN proof_segment_id TEXT REFERENCES production_segments(id) ON DELETE SET NULL;

CREATE INDEX usage_proof_segment_idx ON usage_ledger(proof_segment_id);
