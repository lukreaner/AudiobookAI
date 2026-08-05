CREATE TABLE application_settings (
    key TEXT PRIMARY KEY NOT NULL,
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload))
);
