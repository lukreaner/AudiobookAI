-- AudiobookAI authoritative schema. Domain payloads are retained as JSON for
-- lossless API round-tripping while relational columns enforce ownership,
-- ordering, lifecycle, and accounting invariants.

CREATE TABLE books (
    id TEXT PRIMARY KEY NOT NULL,
    managed_epub_path TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    imported_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE UNIQUE INDEX books_managed_path_idx ON books(managed_epub_path);
CREATE INDEX books_source_hash_idx ON books(source_hash);

CREATE TABLE projects (
    id TEXT PRIMARY KEY NOT NULL,
    book_id TEXT NOT NULL REFERENCES books(id) ON DELETE RESTRICT,
    name TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE INDEX projects_book_idx ON projects(book_id);
CREATE INDEX projects_updated_idx ON projects(updated_at DESC);

CREATE TABLE chapters (
    id TEXT PRIMARY KEY NOT NULL,
    book_id TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    selected INTEGER NOT NULL CHECK (selected IN (0, 1)),
    text_hash TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    UNIQUE(book_id, ordinal)
);

CREATE TABLE paragraphs (
    id TEXT PRIMARY KEY NOT NULL,
    chapter_id TEXT NOT NULL REFERENCES chapters(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    content_hash TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    UNIQUE(chapter_id, ordinal)
);

CREATE TABLE providers (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    family TEXT NOT NULL,
    role TEXT NOT NULL,
    deployment TEXT NOT NULL,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE INDEX providers_family_idx ON providers(family);

CREATE TABLE capability_snapshots (
    id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model TEXT,
    endpoint_fingerprint TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    expires_at TEXT,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE INDEX capability_provider_observed_idx
    ON capability_snapshots(provider_id, observed_at DESC);

CREATE TABLE characters (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    canonical_name TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE INDEX characters_project_idx ON characters(project_id);

CREATE TABLE character_aliases (
    character_id TEXT NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    alias TEXT NOT NULL,
    normalized_alias TEXT NOT NULL,
    PRIMARY KEY(character_id, normalized_alias)
);

CREATE TABLE detection_runs (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE RESTRICT,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    completed_at TEXT,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE dialogue_spans (
    detection_run_id TEXT NOT NULL REFERENCES detection_runs(id) ON DELETE CASCADE,
    paragraph_id TEXT NOT NULL REFERENCES paragraphs(id) ON DELETE CASCADE,
    character_id TEXT NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    byte_start INTEGER NOT NULL CHECK (byte_start >= 0),
    byte_end INTEGER NOT NULL CHECK (byte_end >= byte_start),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    PRIMARY KEY(detection_run_id, paragraph_id, byte_start, byte_end)
);

CREATE TABLE speaker_overrides (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    paragraph_id TEXT NOT NULL REFERENCES paragraphs(id) ON DELETE CASCADE,
    source_content_hash TEXT NOT NULL,
    byte_start INTEGER NOT NULL CHECK (byte_start >= 0),
    byte_end INTEGER NOT NULL CHECK (byte_end >= byte_start),
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE segments (
    id TEXT PRIMARY KEY NOT NULL,
    chapter_id TEXT NOT NULL REFERENCES chapters(id) ON DELETE CASCADE,
    paragraph_id TEXT NOT NULL REFERENCES paragraphs(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    text_hash TEXT NOT NULL,
    state TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    UNIQUE(chapter_id, ordinal)
);

CREATE TABLE artifacts (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT REFERENCES projects(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    path TEXT NOT NULL,
    cache_key TEXT,
    pinned_by_job_id TEXT,
    created_at TEXT NOT NULL,
    last_accessed_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE UNIQUE INDEX artifacts_cache_key_idx
    ON artifacts(cache_key) WHERE cache_key IS NOT NULL;
CREATE INDEX artifacts_lru_idx ON artifacts(last_accessed_at);
CREATE INDEX artifacts_pin_idx ON artifacts(pinned_by_job_id);

CREATE TABLE voice_profiles (
    id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE RESTRICT,
    name TEXT NOT NULL,
    origin TEXT NOT NULL,
    ownership TEXT NOT NULL,
    provider_voice_id TEXT,
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE voice_assignments (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE RESTRICT,
    voice_profile_id TEXT NOT NULL REFERENCES voice_profiles(id) ON DELETE RESTRICT,
    speaker_key TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    UNIQUE(project_id, speaker_key)
);

CREATE TABLE dictionaries (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT REFERENCES projects(id) ON DELETE CASCADE,
    scope TEXT NOT NULL,
    name TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE dictionary_rules (
    id TEXT PRIMARY KEY NOT NULL,
    dictionary_id TEXT NOT NULL REFERENCES dictionaries(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    kind TEXT NOT NULL,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    UNIQUE(dictionary_id, ordinal)
);

CREATE TABLE export_profiles (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    format TEXT NOT NULL,
    layout TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE budgets (
    id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT REFERENCES providers(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    scope TEXT NOT NULL,
    period TEXT NOT NULL,
    metric TEXT NOT NULL,
    currency TEXT,
    limit_value INTEGER NOT NULL CHECK (limit_value >= 0),
    used_value INTEGER NOT NULL CHECK (used_value >= 0),
    hard INTEGER NOT NULL CHECK (hard IN (0, 1)),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    period_started_at TEXT NOT NULL,
    period_ends_at TEXT,
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE INDEX budgets_provider_idx ON budgets(provider_id);

CREATE TABLE jobs (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    export_profile_id TEXT REFERENCES export_profiles(id) ON DELETE SET NULL,
    reservation_id TEXT,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE INDEX jobs_project_created_idx ON jobs(project_id, created_at DESC);
CREATE INDEX jobs_state_idx ON jobs(state);

CREATE TABLE job_units (
    id TEXT PRIMARY KEY NOT NULL,
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    chapter_id TEXT REFERENCES chapters(id) ON DELETE CASCADE,
    segment_id TEXT REFERENCES segments(id) ON DELETE CASCADE,
    provider_id TEXT REFERENCES providers(id) ON DELETE RESTRICT,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    next_attempt_at TEXT,
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE INDEX job_units_ready_idx ON job_units(job_id, state, next_attempt_at);

CREATE TABLE job_unit_dependencies (
    job_unit_id TEXT NOT NULL REFERENCES job_units(id) ON DELETE CASCADE,
    depends_on_id TEXT NOT NULL REFERENCES job_units(id) ON DELETE CASCADE,
    PRIMARY KEY(job_unit_id, depends_on_id),
    CHECK(job_unit_id <> depends_on_id)
);

CREATE TABLE job_attempts (
    id TEXT PRIMARY KEY NOT NULL,
    job_unit_id TEXT NOT NULL REFERENCES job_units(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    started_at TEXT NOT NULL,
    finished_at TEXT,
    failure_class TEXT,
    uncertain_charge INTEGER NOT NULL CHECK (uncertain_charge IN (0, 1)),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    UNIQUE(job_unit_id, ordinal)
);

CREATE TABLE budget_reservations (
    id TEXT PRIMARY KEY NOT NULL,
    job_id TEXT NOT NULL UNIQUE REFERENCES jobs(id) ON DELETE CASCADE,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT,
    reconciled_at TEXT,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE budget_allocations (
    reservation_id TEXT NOT NULL REFERENCES budget_reservations(id) ON DELETE CASCADE,
    budget_id TEXT NOT NULL REFERENCES budgets(id) ON DELETE RESTRICT,
    reserved_amount INTEGER NOT NULL CHECK (reserved_amount >= 0),
    actual_amount INTEGER CHECK (actual_amount IS NULL OR actual_amount >= 0),
    PRIMARY KEY(reservation_id, budget_id)
);

CREATE INDEX budget_allocations_active_idx ON budget_allocations(budget_id, reservation_id);

CREATE TABLE rate_cards (
    id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model TEXT,
    workload TEXT NOT NULL,
    currency TEXT NOT NULL,
    effective_at TEXT NOT NULL,
    expires_at TEXT,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE usage_ledger (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    occurred_at TEXT NOT NULL,
    workload TEXT NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
    job_id TEXT REFERENCES jobs(id) ON DELETE SET NULL,
    attempt_id TEXT REFERENCES job_attempts(id) ON DELETE SET NULL,
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE RESTRICT,
    characters INTEGER,
    audio_milliseconds INTEGER,
    input_tokens INTEGER,
    output_tokens INTEGER,
    provider_credits INTEGER,
    cost_micros INTEGER,
    currency TEXT,
    uncertain_charge INTEGER NOT NULL CHECK (uncertain_charge IN (0, 1)),
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE INDEX usage_project_time_idx ON usage_ledger(project_id, occurred_at);
CREATE INDEX usage_provider_time_idx ON usage_ledger(provider_id, occurred_at);
CREATE INDEX usage_job_idx ON usage_ledger(job_id);

CREATE TRIGGER usage_ledger_no_update
BEFORE UPDATE ON usage_ledger
BEGIN
    SELECT RAISE(ABORT, 'usage ledger is append-only');
END;

CREATE TRIGGER usage_ledger_no_delete
BEFORE DELETE ON usage_ledger
BEGIN
    SELECT RAISE(ABORT, 'usage ledger is append-only');
END;

CREATE TABLE secret_references (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    label TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_used_at TEXT,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE encrypted_secrets (
    secret_id TEXT PRIMARY KEY NOT NULL REFERENCES secret_references(id) ON DELETE CASCADE,
    schema_version INTEGER NOT NULL,
    algorithm TEXT NOT NULL,
    key_source TEXT NOT NULL,
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    associated_data BLOB NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE auth_sessions (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    token_hash BLOB NOT NULL,
    csrf_hash BLOB,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    revoked_at TEXT,
    peer_address TEXT
);

CREATE INDEX auth_sessions_expiry_idx ON auth_sessions(expires_at);

CREATE TABLE api_tokens (
    id TEXT PRIMARY KEY NOT NULL,
    label TEXT NOT NULL,
    token_hash BLOB NOT NULL,
    created_at TEXT NOT NULL,
    last_used_at TEXT,
    expires_at TEXT,
    revoked_at TEXT
);

CREATE TABLE idempotency_keys (
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    response_status INTEGER,
    response_body BLOB,
    response_content_type TEXT,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    PRIMARY KEY(scope, key)
);

CREATE INDEX idempotency_expiry_idx ON idempotency_keys(expires_at);

