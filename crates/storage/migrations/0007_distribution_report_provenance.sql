ALTER TABLE quality_reports
    ADD COLUMN metadata_revision INTEGER NOT NULL DEFAULT 0 CHECK (metadata_revision >= 0);

ALTER TABLE quality_reports
    ADD COLUMN policy_digest TEXT NOT NULL DEFAULT '';

ALTER TABLE quality_reports
    ADD COLUMN metadata_digest TEXT NOT NULL DEFAULT '';

ALTER TABLE quality_reports
    ADD COLUMN package_digest TEXT NOT NULL DEFAULT '';
