-- Character review is edited as one optimistic project-scoped aggregate.  The
-- relational reference columns mirror existing JSON payloads so merges and
-- deletes can be enforced transactionally rather than by scanning opaque JSON.

ALTER TABLE projects
    ADD COLUMN character_revision INTEGER NOT NULL DEFAULT 0 CHECK (character_revision >= 0);

ALTER TABLE characters
    ADD COLUMN role TEXT NOT NULL DEFAULT 'character'
        CHECK (role IN ('narrator', 'character'));

UPDATE characters
SET role = 'narrator'
WHERE lower(trim(canonical_name)) = 'narrator';

ALTER TABLE speaker_overrides
    ADD COLUMN speaker_character_id TEXT REFERENCES characters(id) ON DELETE RESTRICT;

UPDATE speaker_overrides
SET speaker_character_id = json_extract(payload, '$.speaker.id')
WHERE json_extract(payload, '$.speaker.kind') = 'character';

-- Earlier releases selected the newest duplicate at read time. Retain exactly
-- that row before making the range identity a database invariant.
DELETE FROM speaker_overrides
WHERE id IN (
    SELECT id
    FROM (
        SELECT id,
               row_number() OVER (
                   PARTITION BY project_id, paragraph_id, source_content_hash, byte_start, byte_end
                   ORDER BY updated_at DESC, id DESC
               ) AS duplicate_rank
        FROM speaker_overrides
    )
    WHERE duplicate_rank > 1
);

CREATE UNIQUE INDEX speaker_overrides_exact_range_idx
    ON speaker_overrides(project_id, paragraph_id, source_content_hash, byte_start, byte_end);
CREATE INDEX speaker_overrides_character_idx
    ON speaker_overrides(speaker_character_id);

ALTER TABLE voice_assignments
    ADD COLUMN character_id TEXT REFERENCES characters(id) ON DELETE RESTRICT;

UPDATE voice_assignments
SET character_id = substr(speaker_key, length('character:') + 1)
WHERE speaker_key LIKE 'character:%';

UPDATE voice_assignments
SET character_id = (
    SELECT characters.id
    FROM characters
    WHERE characters.project_id = voice_assignments.project_id
      AND characters.role = 'narrator'
    ORDER BY characters.updated_at DESC, characters.id DESC
    LIMIT 1
)
WHERE speaker_key = 'narrator';

CREATE UNIQUE INDEX voice_assignments_project_character_idx
    ON voice_assignments(project_id, character_id)
    WHERE character_id IS NOT NULL;

ALTER TABLE dictionary_rules
    ADD COLUMN character_id TEXT REFERENCES characters(id) ON DELETE RESTRICT;

UPDATE dictionary_rules
SET character_id = json_extract(payload, '$.character_id')
WHERE json_extract(payload, '$.character_id') IS NOT NULL;

CREATE INDEX dictionary_rules_character_idx ON dictionary_rules(character_id);
CREATE INDEX jobs_project_kind_state_created_idx
    ON jobs(project_id, kind, state, created_at DESC);
