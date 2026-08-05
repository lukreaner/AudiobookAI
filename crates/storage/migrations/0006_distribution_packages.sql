CREATE TABLE distribution_metadata (
    project_id TEXT PRIMARY KEY NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE TABLE export_packages (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    target TEXT NOT NULL CHECK (target IN ('generic_m4b', 'acx', 'spotify_for_authors', 'google_play')),
    output_directory TEXT NOT NULL,
    created_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    UNIQUE(job_id, target)
);

CREATE INDEX export_packages_project_created_idx
    ON export_packages(project_id, created_at DESC);

CREATE TABLE quality_reports (
    id TEXT PRIMARY KEY NOT NULL,
    package_id TEXT NOT NULL REFERENCES export_packages(id) ON DELETE CASCADE,
    policy_version TEXT NOT NULL,
    technical_ready INTEGER NOT NULL CHECK (technical_ready IN (0, 1)),
    submission_ready INTEGER NOT NULL CHECK (submission_ready IN (0, 1)),
    generated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);

CREATE INDEX quality_reports_package_created_idx
    ON quality_reports(package_id, generated_at DESC);
