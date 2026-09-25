use super::{
    AppState, BTreeMap, BTreeSet, Character, CharacterDetectionResult, CharacterId, CharacterView,
    DetectedCharacter, DetectionRunId, DetectionSourceParagraph, DialogueEvidenceView, FromStr,
    ParagraphId, ProjectId, ServiceError, Utc, Uuid,
};

pub(super) fn normalized_character_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[derive(Default)]
pub(super) struct CastGroup {
    pub(super) names: Vec<String>,
    pub(super) keys: BTreeSet<String>,
    pub(super) canonical_counts: BTreeMap<String, usize>,
    pub(super) confidence: f32,
    pub(super) merged_into: Option<usize>,
}

impl CastGroup {
    pub(super) fn has_canonical_key(&self, key: &str) -> bool {
        self.canonical_counts
            .keys()
            .any(|canonical| normalized_character_name(canonical) == key)
    }

    /// The name used most often as canonical, preferring the fuller name on ties.
    pub(super) fn canonical_name(&self) -> Option<String> {
        self.canonical_counts
            .iter()
            .max_by(|left, right| {
                left.1
                    .cmp(right.1)
                    .then_with(|| left.0.chars().count().cmp(&right.0.chars().count()))
                    .then_with(|| right.0.cmp(left.0))
            })
            .map(|(name, _)| name.clone())
    }

    pub(super) fn add_name(&mut self, name: String) {
        let key = normalized_character_name(&name);
        if !self
            .names
            .iter()
            .any(|existing| normalized_character_name(existing) == key)
        {
            self.names.push(name);
        }
        self.keys.insert(key);
    }
}

/// Union of per-batch character entries into one cast.
#[derive(Default)]
pub(super) struct CastMerger {
    pub(super) groups: Vec<CastGroup>,
}

impl CastMerger {
    pub(super) fn live(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.groups.len()).filter(|index| self.groups[*index].merged_into.is_none())
    }

    pub(super) fn root(&self, mut index: usize) -> usize {
        while let Some(next) = self.groups[index].merged_into {
            index = next;
        }
        index
    }

    pub(super) fn add_character(&mut self, character: &DetectedCharacter) {
        let canonical = character.canonical_name.trim();
        let canonical_key = normalized_character_name(canonical);
        if canonical_key.is_empty() {
            return;
        }
        let names = std::iter::once(canonical)
            .chain(character.aliases.iter().map(|alias| alias.trim()))
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let keys = names
            .iter()
            .map(|name| normalized_character_name(name))
            .collect::<BTreeSet<_>>();
        let matches = self
            .live()
            .filter(|index| {
                let group = &self.groups[*index];
                group.keys.contains(&canonical_key)
                    || group
                        .canonical_counts
                        .keys()
                        .any(|existing| keys.contains(&normalized_character_name(existing)))
            })
            .collect::<Vec<_>>();
        let target = matches.first().copied().unwrap_or_else(|| {
            self.groups.push(CastGroup::default());
            self.groups.len() - 1
        });
        for &other in matches.iter().skip(1) {
            self.absorb(target, other);
        }
        let group = &mut self.groups[target];
        for name in names {
            group.add_name(name);
        }
        *group
            .canonical_counts
            .entry(canonical.to_owned())
            .or_default() += 1;
        group.confidence = group.confidence.max(character.confidence);
    }

    pub(super) fn absorb(&mut self, target: usize, other: usize) {
        let absorbed = std::mem::replace(
            &mut self.groups[other],
            CastGroup {
                merged_into: Some(target),
                ..CastGroup::default()
            },
        );
        let group = &mut self.groups[target];
        for name in absorbed.names {
            group.add_name(name);
        }
        for (name, count) in absorbed.canonical_counts {
            *group.canonical_counts.entry(name).or_default() += count;
        }
        group.confidence = group.confidence.max(absorbed.confidence);
    }

    /// Resolves a dialogue speaker, preferring canonical names over aliases, and adds an
    /// undeclared speaker as a new character.
    pub(super) fn speaker(&mut self, name: &str, confidence: f32) -> Option<usize> {
        let key = normalized_character_name(name);
        if key.is_empty() {
            return None;
        }
        let existing = self
            .live()
            .find(|index| self.groups[*index].has_canonical_key(&key))
            .or_else(|| {
                self.live()
                    .find(|index| self.groups[*index].keys.contains(&key))
            });
        if let Some(index) = existing {
            return Some(self.root(index));
        }
        let mut group = CastGroup {
            confidence,
            ..CastGroup::default()
        };
        group.add_name(name.trim().to_owned());
        group.canonical_counts.insert(name.trim().to_owned(), 1);
        self.groups.push(group);
        Some(self.groups.len() - 1)
    }
}

/// Merges the per-batch character lists of one detection run into one cast.
///
/// Batches are detected independently, so the same person can appear as "Harry Potter" in one
/// batch and as "Harry" in the next. Two entries are merged when one entry's canonical name is a
/// name of the other. Two entries that merely share an alias (such as "Dad") stay separate, since
/// such aliases are frequently shared by different people. Every dialogue passage is rewritten to
/// its merged canonical name, speakers the model forgot to declare are added, and characters
/// without any attributed dialogue are dropped because they cannot receive a voice line.
pub(super) fn canonicalize_detection_result(
    mut result: CharacterDetectionResult,
) -> CharacterDetectionResult {
    let mut cast = CastMerger::default();
    for character in &result.characters {
        cast.add_character(character);
    }
    let speakers = result
        .dialogue
        .iter()
        .map(|dialogue| cast.speaker(&dialogue.character, dialogue.confidence))
        .collect::<Vec<_>>();
    let canonical_names = cast
        .groups
        .iter()
        .map(CastGroup::canonical_name)
        .collect::<Vec<_>>();
    let mut speaking = BTreeSet::new();
    result.dialogue = std::mem::take(&mut result.dialogue)
        .into_iter()
        .zip(speakers)
        .filter_map(|(mut span, group)| {
            let group = group?;
            span.character = canonical_names[group].clone()?;
            speaking.insert(group);
            Some(span)
        })
        .collect();
    result.characters = cast
        .live()
        .filter(|index| speaking.contains(index) || cast.groups[*index].keys.contains("narrator"))
        .filter_map(|index| {
            let canonical_name = canonical_names[index].clone()?;
            let canonical_key = normalized_character_name(&canonical_name);
            Some(DetectedCharacter {
                aliases: cast.groups[index]
                    .names
                    .iter()
                    .filter(|name| normalized_character_name(name) != canonical_key)
                    .cloned()
                    .collect(),
                canonical_name,
                confidence: cast.groups[index].confidence,
            })
        })
        .collect();
    result
}

pub(super) fn merge_characters(
    result: &CharacterDetectionResult,
    _paragraphs: &[DetectionSourceParagraph],
    project_id: Uuid,
    run_id: DetectionRunId,
    previous_characters: &BTreeMap<String, Character>,
) -> Vec<Character> {
    let mut merged = BTreeMap::<String, (String, Vec<String>, f32)>::new();
    for character in &result.characters {
        let key = character.canonical_name.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        let entry = merged.entry(key).or_insert_with(|| {
            (
                character.canonical_name.trim().to_owned(),
                Vec::new(),
                character.confidence,
            )
        });
        entry.2 = entry.2.max(character.confidence);
        for alias in &character.aliases {
            if !entry
                .1
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(alias))
            {
                entry.1.push(alias.clone());
            }
        }
    }
    merged
        .entry("narrator".to_owned())
        .or_insert(("Narrator".to_owned(), Vec::new(), 1.0));
    let now = Utc::now();
    let mut output = merged
        .into_values()
        .map(|(detected_name, detected_aliases, confidence)| {
            let previous = previous_characters.get(&detected_name.to_lowercase());
            let preserve_identity = previous.is_some_and(|character| character.manually_created);
            Character {
                id: previous.map_or_else(CharacterId::new, |character| character.id),
                project_id: ProjectId::from_uuid(project_id),
                role: if detected_name.eq_ignore_ascii_case("narrator") {
                    audiobookai_core::CharacterRole::Narrator
                } else {
                    audiobookai_core::CharacterRole::Character
                },
                canonical_name: previous
                    .filter(|_| preserve_identity)
                    .map_or(detected_name, |character| character.canonical_name.clone()),
                aliases: previous
                    .filter(|_| preserve_identity)
                    .map_or(detected_aliases, |character| character.aliases.clone()),
                description: previous.and_then(|character| character.description.clone()),
                confidence: Some(confidence),
                detection_run_id: Some(run_id),
                manually_created: preserve_identity,
                created_at: previous.map_or(now, |character| character.created_at),
                updated_at: now,
            }
        })
        .collect::<Vec<_>>();
    let mut retained_ids = output
        .iter()
        .map(|character| character.id)
        .collect::<BTreeSet<_>>();
    for previous in previous_characters.values() {
        if previous.manually_created && retained_ids.insert(previous.id) {
            output.push(previous.clone());
        }
    }
    output
}

pub(super) async fn load_previous_characters(
    state: &AppState,
    project_id: Uuid,
) -> Result<BTreeMap<String, Character>, ServiceError> {
    let payloads = sqlx::query_as::<_, (String, String)>(
        "SELECT role, payload FROM characters WHERE project_id = ? ORDER BY updated_at DESC",
    )
    .bind(project_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let mut mapped = BTreeMap::new();
    for (role, payload) in payloads {
        let mut character: Character = serde_json::from_str(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        character.role = if role == "narrator" {
            audiobookai_core::CharacterRole::Narrator
        } else {
            audiobookai_core::CharacterRole::Character
        };
        for name in std::iter::once(&character.canonical_name).chain(character.aliases.iter()) {
            mapped
                .entry(name.to_lowercase())
                .or_insert_with(|| character.clone());
        }
    }
    Ok(mapped)
}

pub(super) fn character_views(
    characters: &[Character],
    result: &CharacterDetectionResult,
    paragraphs: &[DetectionSourceParagraph],
    previous_assignments: &BTreeMap<String, Option<crate::models::VoiceAssignmentView>>,
) -> Vec<CharacterView> {
    characters
        .iter()
        .map(|character| {
            let names = std::iter::once(character.canonical_name.as_str())
                .chain(character.aliases.iter().map(String::as_str))
                .collect::<Vec<_>>();
            let evidence = result
                .dialogue
                .iter()
                .filter(|dialogue| {
                    names
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&dialogue.character))
                })
                .filter_map(|dialogue| {
                    let paragraph_id = ParagraphId::from_str(&dialogue.paragraph_id).ok()?;
                    let paragraph = paragraphs
                        .iter()
                        .find(|paragraph| paragraph.id == paragraph_id)?;
                    let start = usize::try_from(dialogue.start).ok()?;
                    let end = usize::try_from(dialogue.end).ok()?;
                    Some(DialogueEvidenceView {
                        id: Uuid::new_v4(),
                        paragraph_id: paragraph.id.as_uuid(),
                        chapter_id: paragraph.chapter_id.as_uuid(),
                        chapter_title: paragraph.chapter_title.clone(),
                        excerpt: paragraph
                            .text
                            .get(start..end)
                            .unwrap_or(paragraph.text.as_str())
                            .chars()
                            .take(240)
                            .collect(),
                        confidence: dialogue.confidence,
                        start_offset: start,
                        end_offset: end,
                        speaker_override: None,
                    })
                })
                .collect::<Vec<_>>();
            CharacterView {
                id: character.id.as_uuid(),
                role: character.role,
                canonical_name: character.canonical_name.clone(),
                aliases: character.aliases.clone(),
                confidence: character.confidence.unwrap_or_default(),
                dialogue_count: evidence.len(),
                voice_assignment: previous_assignments
                    .get(&character.canonical_name.to_lowercase())
                    .cloned()
                    .flatten(),
                evidence,
            }
        })
        .collect()
}

pub(super) async fn apply_persisted_overrides(
    state: &AppState,
    project_id: Uuid,
    paragraphs: &[DetectionSourceParagraph],
    views: &mut [CharacterView],
) -> Result<(), ServiceError> {
    use audiobookai_core::{Speaker, SpeakerOverride};

    let payloads = sqlx::query_scalar::<_, String>(
        "SELECT payload FROM speaker_overrides WHERE project_id = ? ORDER BY updated_at",
    )
    .bind(project_id.to_string())
    .fetch_all(state.database.pool())
    .await
    .map_err(|error| ServiceError::Storage(error.to_string()))?;
    for payload in payloads {
        let record: SpeakerOverride = serde_json::from_str(&payload)
            .map_err(|error| ServiceError::Internal(error.to_string()))?;
        let Some(paragraph) = paragraphs
            .iter()
            .find(|paragraph| paragraph.id == record.paragraph_id)
        else {
            continue;
        };
        if paragraph.hash != record.source_content_hash {
            continue;
        }
        let speaker_name = match record.speaker {
            Speaker::Narrator => "Narrator".to_owned(),
            Speaker::Character(character_id) => views
                .iter()
                .find(|character| character.id == character_id.as_uuid())
                .map_or_else(
                    || character_id.to_string(),
                    |character| character.canonical_name.clone(),
                ),
            Speaker::Named(name) => name,
        };
        for evidence in views
            .iter_mut()
            .flat_map(|character| &mut character.evidence)
        {
            if evidence.paragraph_id == record.paragraph_id.as_uuid()
                && evidence.start_offset == usize::try_from(record.byte_start).unwrap_or(usize::MAX)
                && evidence.end_offset == usize::try_from(record.byte_end).unwrap_or(usize::MAX)
            {
                evidence.speaker_override = Some(speaker_name.clone());
            }
        }
    }
    Ok(())
}
