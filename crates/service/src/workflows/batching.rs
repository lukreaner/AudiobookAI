use super::{
    AppState, BTreeMap, DETECTION_BATCH_PARAGRAPHS, DETECTION_CONTEXT_OVERLAP,
    DETECTION_MAX_OUTPUT_TOKENS, DETECTION_MIN_OUTPUT_TOKENS, DETECTION_MIN_PARAGRAPH_TOKENS,
    DETECTION_PARAGRAPH_OVERHEAD_TOKENS, DETECTION_PROMPT_TOKEN_RESERVE,
    DETECTION_TOKEN_AWARE_SCHEMA_VERSION, DetectionJobConfig, DetectionParagraph, ParagraphId,
    ReasoningControl, ServiceError, UsageQuantities,
};

#[derive(Clone)]
pub(super) struct DetectionSourceParagraph {
    pub(super) id: ParagraphId,
    pub(super) text: String,
    pub(super) hash: String,
    pub(super) chapter_title: String,
    pub(super) chapter_id: audiobookai_core::ChapterId,
}

/// How source text is packed into detection requests.
///
/// Durable jobs recompute their batches on resume, so every schema version keeps its original
/// packing. Reservation estimates deliberately keep the byte-per-token upper bound regardless.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DetectionBatching {
    /// Conservative lower bound of UTF-8 bytes per model token. Two bytes per token holds for
    /// Latin, Cyrillic and CJK text with current tokenizers (typically three to four for German
    /// and English), while the original schema counted every byte as a token.
    pub(super) text_bytes_per_token: u64,
    /// JSON framing per paragraph. Since schema 6 the adapters send short request-local aliases
    /// (`p12`) instead of the durable fragment identifier, so its length is no longer counted.
    pub(super) paragraph_overhead_tokens: u64,
    pub(super) count_request_id: bool,
    pub(super) max_paragraphs: usize,
}

impl DetectionBatching {
    pub(super) const V5: Self = Self {
        text_bytes_per_token: 1,
        paragraph_overhead_tokens: DETECTION_PARAGRAPH_OVERHEAD_TOKENS,
        count_request_id: true,
        max_paragraphs: DETECTION_BATCH_PARAGRAPHS,
    };
    pub(super) const CURRENT: Self = Self {
        text_bytes_per_token: 2,
        paragraph_overhead_tokens: 32,
        count_request_id: false,
        max_paragraphs: 40,
    };

    pub(super) const fn for_schema(schema_version: u32) -> Self {
        if schema_version >= 6 {
            Self::CURRENT
        } else {
            Self::V5
        }
    }

    pub(super) fn fragment_tokens(self, fragment: &DetectionFragment) -> u64 {
        u64::try_from(fragment.text.len())
            .unwrap_or(u64::MAX)
            .div_ceil(self.text_bytes_per_token)
            .saturating_add(if self.count_request_id {
                u64::try_from(fragment.request_id.len()).unwrap_or(u64::MAX)
            } else {
                0
            })
            .saturating_add(self.paragraph_overhead_tokens)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DetectionContextBudget {
    pub(super) context_window: u64,
    pub(super) paragraph_budget: u64,
    pub(super) max_output: u32,
    pub(super) safety_margin: u64,
}

impl DetectionContextBudget {
    pub(super) fn new(context_window_tokens: u64) -> Result<Self, ServiceError> {
        let safety_tokens = (context_window_tokens / 16).clamp(128, 1_024);
        let fixed = DETECTION_PROMPT_TOKEN_RESERVE.saturating_add(safety_tokens);
        let available = context_window_tokens.checked_sub(fixed).ok_or_else(|| {
            ServiceError::Conflict(
                "the configured model context window is too small for character detection"
                    .to_owned(),
            )
        })?;
        if available < DETECTION_MIN_OUTPUT_TOKENS.saturating_add(DETECTION_MIN_PARAGRAPH_TOKENS) {
            return Err(ServiceError::Conflict(
                "the configured model context window is too small for character detection"
                    .to_owned(),
            ));
        }
        let desired_output = (context_window_tokens / 4)
            .clamp(DETECTION_MIN_OUTPUT_TOKENS, DETECTION_MAX_OUTPUT_TOKENS);
        let output_tokens = desired_output.min(available - DETECTION_MIN_PARAGRAPH_TOKENS);
        let max_output_tokens = u32::try_from(output_tokens).map_err(|_| {
            ServiceError::Conflict("the configured model output limit is too large".to_owned())
        })?;
        Ok(Self {
            context_window: context_window_tokens,
            paragraph_budget: available - output_tokens,
            max_output: max_output_tokens,
            safety_margin: safety_tokens,
        })
    }
}

#[derive(Clone, Debug)]
pub(super) struct DetectionFragment {
    pub(super) request_id: String,
    pub(super) source_id: String,
    pub(super) source_byte_start: usize,
    pub(super) text: String,
}

#[derive(Clone, Debug)]
pub(super) struct DetectionBatchParagraph {
    pub(super) fragment: DetectionFragment,
    pub(super) context_only: bool,
}

#[derive(Clone, Debug)]
pub(super) struct DetectionBatch {
    pub(super) paragraphs: Vec<DetectionBatchParagraph>,
    pub(super) max_output_tokens: u32,
}

impl DetectionBatch {
    pub(super) fn request_paragraphs(&self) -> Vec<DetectionParagraph> {
        self.paragraphs
            .iter()
            .map(|paragraph| DetectionParagraph {
                id: paragraph.fragment.request_id.clone(),
                text: paragraph.fragment.text.clone(),
                context_only: paragraph.context_only,
            })
            .collect()
    }

    pub(super) fn core_paragraphs(&self) -> Vec<DetectionFragment> {
        self.paragraphs
            .iter()
            .filter(|paragraph| !paragraph.context_only)
            .map(|paragraph| paragraph.fragment.clone())
            .collect()
    }

    pub(super) fn split_for_context_retry(&self) -> Result<Vec<Self>, ServiceError> {
        let fallback_output = if self.max_output_tokens > 256 {
            (self.max_output_tokens / 2).max(256)
        } else {
            self.max_output_tokens
        };
        if let Some(children) = self.split_core(fallback_output)? {
            return Ok(children);
        }
        if self.max_output_tokens > 256 {
            return Ok(vec![Self {
                paragraphs: self.paragraphs.clone(),
                max_output_tokens: fallback_output,
            }]);
        }
        Err(ServiceError::Conflict(
            "the provider context window remains too small after adaptive batching".to_owned(),
        ))
    }

    pub(super) fn split_for_output_retry(&self) -> Result<Vec<Self>, ServiceError> {
        self.split_core(self.max_output_tokens)?.ok_or_else(|| {
            ServiceError::Conflict(
                "the provider output remains incomplete after adaptive batching".to_owned(),
            )
        })
    }

    pub(super) fn split_core(
        &self,
        max_output_tokens: u32,
    ) -> Result<Option<Vec<Self>>, ServiceError> {
        let core = self.core_paragraphs();
        if core.len() > 1 {
            let middle = core.len().div_ceil(2);
            return Ok(Some(
                [&core[..middle], &core[middle..]]
                    .into_iter()
                    .filter(|paragraphs| !paragraphs.is_empty())
                    .map(|paragraphs| Self {
                        paragraphs: paragraphs
                            .iter()
                            .cloned()
                            .map(|fragment| DetectionBatchParagraph {
                                fragment,
                                context_only: false,
                            })
                            .collect(),
                        max_output_tokens,
                    })
                    .collect(),
            ));
        }
        let Some(fragment) = core.first() else {
            return Err(ServiceError::Conflict(
                "character-detection adaptive batching has no source text".to_owned(),
            ));
        };
        if fragment.text.chars().count() > 1 {
            let end = preferred_fragment_end(&fragment.text, 0, fragment.text.len() / 2);
            if end > 0 && end < fragment.text.len() {
                let fragments = [
                    detection_fragment_slice(fragment, 0, end),
                    detection_fragment_slice(fragment, end, fragment.text.len()),
                ];
                return Ok(Some(
                    fragments
                        .into_iter()
                        .map(|fragment| Self {
                            paragraphs: vec![DetectionBatchParagraph {
                                fragment,
                                context_only: false,
                            }],
                            max_output_tokens,
                        })
                        .collect(),
                ));
            }
        }
        Ok(None)
    }
}

pub(super) async fn selected_paragraphs(
    state: &AppState,
    project: &audiobookai_core::Project,
) -> Result<Vec<DetectionSourceParagraph>, ServiceError> {
    let repository = state.database.repositories().projects;
    let chapters = repository
        .list_chapters(project.book_id)
        .await
        .map_err(|error| ServiceError::Storage(error.to_string()))?;
    let mut output = Vec::new();
    for chapter in chapters.into_iter().filter(|chapter| chapter.selected) {
        let paragraphs = repository
            .list_paragraphs(chapter.id)
            .await
            .map_err(|error| ServiceError::Storage(error.to_string()))?;
        output.extend(
            paragraphs
                .into_iter()
                .map(|paragraph| DetectionSourceParagraph {
                    id: paragraph.id,
                    text: paragraph.text,
                    hash: paragraph.content_hash,
                    chapter_title: chapter.title.clone(),
                    chapter_id: chapter.id,
                }),
        );
    }
    Ok(output)
}

pub(super) fn legacy_paragraph_batches(
    paragraphs: &[DetectionSourceParagraph],
) -> Vec<DetectionBatch> {
    (0..paragraphs.len())
        .step_by(DETECTION_BATCH_PARAGRAPHS)
        .map(|start| {
            let end = (start + DETECTION_BATCH_PARAGRAPHS).min(paragraphs.len());
            let context_start = start.saturating_sub(DETECTION_CONTEXT_OVERLAP);
            let context_end = (end + DETECTION_CONTEXT_OVERLAP).min(paragraphs.len());
            DetectionBatch {
                paragraphs: paragraphs[context_start..context_end]
                    .iter()
                    .enumerate()
                    .map(|(offset, paragraph)| {
                        let absolute = context_start + offset;
                        let source_id = paragraph.id.to_string();
                        DetectionBatchParagraph {
                            fragment: DetectionFragment {
                                request_id: source_id.clone(),
                                source_id,
                                source_byte_start: 0,
                                text: paragraph.text.clone(),
                            },
                            context_only: absolute < start || absolute >= end,
                        }
                    })
                    .collect(),
                max_output_tokens: 4_096,
            }
        })
        .collect()
}

pub(super) fn paragraph_batches(
    paragraphs: &[DetectionSourceParagraph],
    budget: DetectionContextBudget,
    batching: DetectionBatching,
) -> Result<Vec<DetectionBatch>, ServiceError> {
    let mut fragments = Vec::new();
    for paragraph in paragraphs {
        fragments.extend(fragment_source_paragraph(
            paragraph,
            budget.paragraph_budget,
            batching,
        )?);
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < fragments.len() {
        let mut end = start;
        let mut tokens = 0_u64;
        while end < fragments.len() && end - start < batching.max_paragraphs {
            let next = batching.fragment_tokens(&fragments[end]);
            if end > start && tokens.saturating_add(next) > budget.paragraph_budget {
                break;
            }
            if next > budget.paragraph_budget {
                return Err(ServiceError::Conflict(
                    "a paragraph fragment exceeds the character-detection token budget".to_owned(),
                ));
            }
            tokens = tokens.saturating_add(next);
            end += 1;
        }
        if end == start {
            return Err(ServiceError::Conflict(
                "the character-detection token budget produced an empty batch".to_owned(),
            ));
        }
        ranges.push((start, end, tokens));
        start = end;
    }

    Ok(ranges
        .into_iter()
        .map(|(start, end, mut tokens)| {
            let mut indexes = BTreeMap::new();
            for index in start..end {
                indexes.insert(index, false);
            }
            let before_start = start.saturating_sub(DETECTION_CONTEXT_OVERLAP);
            for index in (before_start..start)
                .rev()
                .chain(end..(end + DETECTION_CONTEXT_OVERLAP).min(fragments.len()))
            {
                let next = batching.fragment_tokens(&fragments[index]);
                if tokens.saturating_add(next) <= budget.paragraph_budget {
                    tokens = tokens.saturating_add(next);
                    indexes.insert(index, true);
                }
            }
            DetectionBatch {
                paragraphs: indexes
                    .into_iter()
                    .map(|(index, context_only)| DetectionBatchParagraph {
                        fragment: fragments[index].clone(),
                        context_only,
                    })
                    .collect(),
                max_output_tokens: budget.max_output,
            }
        })
        .collect())
}

pub(super) fn detection_batches_for_config(
    paragraphs: &[DetectionSourceParagraph],
    config: &DetectionJobConfig,
) -> Result<Vec<DetectionBatch>, ServiceError> {
    if config.schema_version < DETECTION_TOKEN_AWARE_SCHEMA_VERSION {
        return Ok(legacy_paragraph_batches(paragraphs));
    }
    let context_window_tokens = config.context_window_tokens.ok_or_else(|| {
        ServiceError::Conflict(
            "the durable detection job is missing its context-window budget".to_owned(),
        )
    })?;
    let budget = DetectionContextBudget::new(context_window_tokens)?;
    if config.max_output_tokens != Some(budget.max_output) {
        return Err(ServiceError::Conflict(
            "the durable detection job has an inconsistent output-token budget".to_owned(),
        ));
    }
    paragraph_batches(
        paragraphs,
        budget,
        DetectionBatching::for_schema(config.schema_version),
    )
}

pub(super) fn fragment_source_paragraph(
    paragraph: &DetectionSourceParagraph,
    paragraph_tokens: u64,
    batching: DetectionBatching,
) -> Result<Vec<DetectionFragment>, ServiceError> {
    let source_id = paragraph.id.to_string();
    let unsplit = DetectionFragment {
        request_id: source_id.clone(),
        source_id: source_id.clone(),
        source_byte_start: 0,
        text: paragraph.text.clone(),
    };
    if batching.fragment_tokens(&unsplit) <= paragraph_tokens {
        return Ok(vec![unsplit]);
    }
    let fixed = DETECTION_PARAGRAPH_OVERHEAD_TOKENS
        .saturating_add(u64::try_from(source_id.len()).unwrap_or(u64::MAX))
        .saturating_add(21);
    let max_text_bytes = paragraph_tokens
        .checked_sub(fixed)
        .map(|value| value.saturating_mul(batching.text_bytes_per_token))
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            ServiceError::Conflict(
                "the configured context window cannot hold a paragraph fragment".to_owned(),
            )
        })?;
    let mut fragments = Vec::new();
    let mut start = 0;
    while start < paragraph.text.len() {
        let end = preferred_fragment_end(&paragraph.text, start, max_text_bytes);
        if end <= start {
            return Err(ServiceError::Conflict(
                "a paragraph could not be split at a UTF-8 boundary".to_owned(),
            ));
        }
        let fragment = DetectionFragment {
            request_id: format!("{source_id}@{start}"),
            source_id: source_id.clone(),
            source_byte_start: start,
            text: paragraph.text[start..end].to_owned(),
        };
        if batching.fragment_tokens(&fragment) > paragraph_tokens {
            return Err(ServiceError::Conflict(
                "a paragraph fragment exceeds the configured context window".to_owned(),
            ));
        }
        fragments.push(fragment);
        start = end;
    }
    Ok(fragments)
}

pub(super) fn preferred_fragment_end(text: &str, start: usize, max_bytes: usize) -> usize {
    if start >= text.len() {
        return text.len();
    }
    let mut hard_end = start.saturating_add(max_bytes).min(text.len());
    while hard_end > start && !text.is_char_boundary(hard_end) {
        hard_end -= 1;
    }
    if hard_end == text.len() || hard_end == start {
        return hard_end;
    }
    let minimum = max_bytes / 2;
    text[start..hard_end]
        .char_indices()
        .rev()
        .find_map(|(offset, character)| {
            (character.is_whitespace() && offset >= minimum)
                .then_some(start + offset + character.len_utf8())
        })
        .unwrap_or(hard_end)
}

pub(super) fn detection_fragment_slice(
    fragment: &DetectionFragment,
    start: usize,
    end: usize,
) -> DetectionFragment {
    let source_byte_start = fragment.source_byte_start.saturating_add(start);
    DetectionFragment {
        request_id: format!("{}@{source_byte_start}", fragment.source_id),
        source_id: fragment.source_id.clone(),
        source_byte_start,
        text: fragment.text[start..end].to_owned(),
    }
}

pub(super) fn detection_fragment_tokens(fragment: &DetectionFragment) -> u64 {
    u64::try_from(fragment.text.len())
        .unwrap_or(u64::MAX)
        .saturating_add(u64::try_from(fragment.request_id.len()).unwrap_or(u64::MAX))
        .saturating_add(DETECTION_PARAGRAPH_OVERHEAD_TOKENS)
}

pub(super) fn detection_request_estimate(
    batch: &DetectionBatch,
    reasoning: &ReasoningControl,
) -> UsageQuantities {
    let characters = batch.paragraphs.iter().fold(0_u64, |total, paragraph| {
        total.saturating_add(
            u64::try_from(paragraph.fragment.text.chars().count()).unwrap_or(u64::MAX),
        )
    });
    // A byte-per-token upper estimate plus stable schema/prompt and paragraph-ID overhead is
    // deliberately conservative across tokenizers without persisting the source text.
    let input_tokens = batch
        .paragraphs
        .iter()
        .fold(DETECTION_PROMPT_TOKEN_RESERVE, |total, paragraph| {
            total.saturating_add(detection_fragment_tokens(&paragraph.fragment))
        });
    let reasoning_tokens = match reasoning {
        ReasoningControl::Disabled => 0,
        ReasoningControl::TokenBudget { tokens } => u64::from(*tokens),
        ReasoningControl::Effort { effort } => match effort {
            audiobookai_providers::ReasoningEffort::Minimal => 2_048,
            audiobookai_providers::ReasoningEffort::Low => 4_096,
            audiobookai_providers::ReasoningEffort::Medium => 8_192,
            audiobookai_providers::ReasoningEffort::High => 16_384,
        },
        ReasoningControl::Inherit | ReasoningControl::Adaptive => 16_384,
    };
    UsageQuantities {
        characters: Some(characters),
        input_tokens: Some(input_tokens),
        output_tokens: Some(u64::from(batch.max_output_tokens)),
        reasoning_tokens: Some(reasoning_tokens),
        ..UsageQuantities::default()
    }
}
