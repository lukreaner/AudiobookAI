//! Per-model discovery of the temperature and reasoning options a model accepts.
//!
//! Options differ per model and change with every model generation, so they are read from the
//! provider whenever it exposes them instead of being hard-coded. When a provider reveals nothing
//! about a model, only provider defaults are offered, because those are always accepted.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::{
    GenerationControlsSource, ModelGenerationControls, ParameterSupport, ReasoningEffort,
    ReasoningMode,
};

/// Effort levels in ascending order; unknown future levels are appended after these.
const KNOWN_EFFORT_ORDER: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
const ANTHROPIC_MIN_THINKING_BUDGET: u32 = 1_024;

fn ordered_efforts(levels: impl IntoIterator<Item = String>) -> Vec<ReasoningEffort> {
    let mut levels = levels
        .into_iter()
        .filter_map(|level| ReasoningEffort::new(level).ok())
        .collect::<Vec<_>>();
    levels.sort_by_key(|level| {
        (
            KNOWN_EFFORT_ORDER
                .iter()
                .position(|known| *known == level.as_str())
                .unwrap_or(KNOWN_EFFORT_ORDER.len()),
            level.as_str().to_owned(),
        )
    });
    levels.dedup();
    levels
}

fn supported(value: &Value, pointer: &str) -> Option<bool> {
    value.pointer(pointer).and_then(Value::as_bool)
}

/// Reads `GET /v1/models/{id}` from the Anthropic Models API.
///
/// Sampling parameters were removed together with fixed thinking budgets (Opus 4.7 and later
/// reject both), so temperature is offered only for models that either do not think at all or
/// still accept `enabled` thinking, unless the model reports sampling support explicitly.
pub(super) fn anthropic_model_controls(model: &Value) -> ModelGenerationControls {
    let capabilities = model.get("capabilities").unwrap_or(&Value::Null);
    let thinks = supported(capabilities, "/thinking/supported").unwrap_or(false);
    let adaptive = supported(capabilities, "/thinking/types/adaptive/supported").unwrap_or(false);
    let budget = supported(capabilities, "/thinking/types/enabled/supported").unwrap_or(false);
    // Some models run thinking permanently; disabling is offered only when positively reported.
    let disable = supported(capabilities, "/thinking/types/disabled/supported").unwrap_or(false);
    let efforts = if supported(capabilities, "/effort/supported").unwrap_or(false) {
        ordered_efforts(
            capabilities
                .get("effort")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter(|(_, entry)| entry.get("supported").and_then(Value::as_bool) == Some(true))
                .map(|(level, _)| level.clone()),
        )
    } else {
        Vec::new()
    };
    let mut reasoning = BTreeSet::new();
    if disable {
        reasoning.insert(ReasoningMode::Disabled);
    }
    if adaptive {
        reasoning.insert(ReasoningMode::Adaptive);
    }
    if budget {
        reasoning.insert(ReasoningMode::TokenBudget);
    }
    if !efforts.is_empty() {
        reasoning.insert(ReasoningMode::Effort);
    }
    let sampling = supported(capabilities, "/sampling/temperature/supported")
        .or_else(|| supported(capabilities, "/temperature/supported"))
        .unwrap_or(!thinks || budget);
    ModelGenerationControls {
        temperature: if sampling {
            ParameterSupport::Value
        } else {
            ParameterSupport::Unsupported
        },
        max_temperature: sampling.then_some(1.0),
        reasoning,
        efforts,
        min_token_budget: budget.then_some(ANTHROPIC_MIN_THINKING_BUDGET),
        max_token_budget: None,
        source: GenerationControlsSource::ModelApi,
    }
}

/// Reads `GET v1beta/models/{id}` from the Gemini API.
///
/// The model resource reports thinking support and the temperature ceiling, but not which
/// thinking controls `generateContent` accepts for the model, so thinking stays at its default.
pub(super) fn gemini_model_controls(model: &Value) -> ModelGenerationControls {
    let max_temperature = model
        .get("maxTemperature")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        // Bounded to (0, 2] above, so the narrowing conversion cannot overflow.
        .map(|value| {
            #[allow(clippy::cast_possible_truncation)]
            let value = value.min(2.0) as f32;
            value
        });
    ModelGenerationControls {
        temperature: ParameterSupport::Value,
        max_temperature: max_temperature.or(Some(2.0)),
        reasoning: BTreeSet::new(),
        efforts: Vec::new(),
        min_token_budget: None,
        max_token_budget: None,
        source: GenerationControlsSource::ModelApi,
    }
}

/// Reads `POST /api/show` from Ollama.
///
/// Thinking models accept `think: false`; GPT-OSS instead takes the levels `low`, `medium`,
/// and `high` and cannot switch thinking off.
pub(super) fn ollama_model_controls(model_id: &str, show: &Value) -> ModelGenerationControls {
    let thinks = show
        .get("capabilities")
        .and_then(Value::as_array)
        .is_some_and(|capabilities| {
            capabilities
                .iter()
                .any(|capability| capability.as_str() == Some("thinking"))
        });
    let family = show
        .pointer("/details/family")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let gpt_oss = family.replace(['-', '_'], "") == "gptoss"
        || model_id.to_ascii_lowercase().starts_with("gpt-oss");
    let mut reasoning = BTreeSet::new();
    let mut efforts = Vec::new();
    if thinks && gpt_oss {
        reasoning.insert(ReasoningMode::Effort);
        efforts = ordered_efforts(["low", "medium", "high"].map(ToOwned::to_owned));
    } else if thinks {
        reasoning.insert(ReasoningMode::Disabled);
    }
    ModelGenerationControls {
        temperature: ParameterSupport::Value,
        max_temperature: Some(2.0),
        reasoning,
        efforts,
        min_token_budget: None,
        max_token_budget: None,
        source: GenerationControlsSource::ModelApi,
    }
}

/// What an `OpenAI` validation probe revealed about one parameter.
#[derive(Debug, PartialEq)]
pub(super) enum ProbeFinding {
    /// The model rejects the parameter entirely.
    Unsupported,
    /// The model accepts the parameter; for enumerations, these values.
    Supported(Vec<String>),
    /// The response could not be interpreted, so nothing may be offered.
    Unknown,
}

fn error_text(status: u16, body: &[u8]) -> Option<(String, String)> {
    if (200..300).contains(&status) {
        return None;
    }
    let value = serde_json::from_slice::<Value>(body).ok()?;
    let error = value.get("error")?;
    Some((
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    ))
}

fn is_unsupported_parameter(message: &str, code: &str) -> bool {
    code == "unsupported_parameter" || message.contains("Unsupported parameter")
}

/// Interprets the response to a request with a deliberately invalid `reasoning.effort`.
///
/// `OpenAI` validates the enumeration before any generation and lists the accepted values, for
/// example `Supported values are: 'none', 'low', 'medium', and 'high'.`
pub(super) fn openai_effort_probe(status: u16, body: &[u8]) -> ProbeFinding {
    let Some((message, code)) = error_text(status, body) else {
        return ProbeFinding::Unknown;
    };
    if is_unsupported_parameter(&message, &code) {
        return ProbeFinding::Unsupported;
    }
    let Some((_, listed)) = message.split_once("Supported values are") else {
        return ProbeFinding::Unknown;
    };
    let values = listed
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if values.is_empty() {
        ProbeFinding::Unknown
    } else {
        ProbeFinding::Supported(values)
    }
}

/// Interprets the response to a request with an out-of-range `temperature`.
pub(super) fn openai_temperature_probe(status: u16, body: &[u8]) -> ProbeFinding {
    let Some((message, code)) = error_text(status, body) else {
        return ProbeFinding::Unknown;
    };
    if is_unsupported_parameter(&message, &code) {
        ProbeFinding::Unsupported
    } else if message.contains("temperature")
        && ["maximum", "less than or equal", "Invalid"]
            .iter()
            .any(|marker| message.contains(marker))
    {
        ProbeFinding::Supported(Vec::new())
    } else {
        ProbeFinding::Unknown
    }
}

/// Combines both `OpenAI` probes. An inconclusive probe offers nothing for its parameter.
pub(super) fn openai_model_controls(
    effort: &ProbeFinding,
    temperature: &ProbeFinding,
) -> ModelGenerationControls {
    let mut reasoning = BTreeSet::new();
    let mut efforts = Vec::new();
    if let ProbeFinding::Supported(values) = effort {
        if values.iter().any(|value| value == "none") {
            reasoning.insert(ReasoningMode::Disabled);
        }
        efforts = ordered_efforts(values.iter().filter(|value| *value != "none").cloned());
        if !efforts.is_empty() {
            reasoning.insert(ReasoningMode::Effort);
        }
    }
    let temperature_supported = matches!(temperature, ProbeFinding::Supported(_));
    let conclusive =
        !matches!(effort, ProbeFinding::Unknown) && !matches!(temperature, ProbeFinding::Unknown);
    ModelGenerationControls {
        temperature: if temperature_supported {
            ParameterSupport::NullableValue
        } else {
            ParameterSupport::Unsupported
        },
        max_temperature: temperature_supported.then_some(2.0),
        reasoning,
        efforts,
        min_token_budget: None,
        max_token_budget: None,
        source: if conclusive {
            GenerationControlsSource::ValidationProbe
        } else {
            GenerationControlsSource::ProviderDefaultOnly
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn names(controls: &ModelGenerationControls) -> Vec<&str> {
        controls
            .efforts
            .iter()
            .map(ReasoningEffort::as_str)
            .collect()
    }

    #[test]
    fn anthropic_current_models_offer_adaptive_and_every_reported_effort_without_sampling() {
        let controls = anthropic_model_controls(&json!({
            "id": "claude-opus-4-8",
            "capabilities": {
                "thinking": {"supported": true, "types": {
                    "enabled": {"supported": false}, "adaptive": {"supported": true}
                }},
                "effort": {"supported": true, "max": {"supported": true},
                    "low": {"supported": true}, "xhigh": {"supported": true},
                    "medium": {"supported": true}, "high": {"supported": true}}
            }
        }));

        assert_eq!(
            controls.reasoning,
            BTreeSet::from([ReasoningMode::Effort, ReasoningMode::Adaptive])
        );
        assert_eq!(names(&controls), ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(controls.temperature, ParameterSupport::Unsupported);
        assert_eq!(controls.min_token_budget, None);
    }

    #[test]
    fn anthropic_budget_models_keep_budgets_and_sampling() {
        let controls = anthropic_model_controls(&json!({
            "capabilities": {
                "thinking": {"supported": true, "types": {
                    "enabled": {"supported": true}, "adaptive": {"supported": false},
                    "disabled": {"supported": true}
                }},
                "effort": {"supported": false}
            }
        }));

        assert_eq!(
            controls.reasoning,
            BTreeSet::from([ReasoningMode::Disabled, ReasoningMode::TokenBudget])
        );
        assert!(controls.efforts.is_empty());
        assert_eq!(controls.temperature, ParameterSupport::Value);
        assert_eq!(controls.max_temperature, Some(1.0));
        assert_eq!(controls.min_token_budget, Some(1_024));
    }

    #[test]
    fn unknown_future_effort_levels_are_kept_after_known_ones() {
        let controls = anthropic_model_controls(&json!({"capabilities": {
            "thinking": {"supported": true, "types": {"adaptive": {"supported": true}}},
            "effort": {"supported": true, "ultra": {"supported": true},
                "low": {"supported": true}, "high": {"supported": false}}
        }}));
        assert_eq!(names(&controls), ["low", "ultra"]);
    }

    #[test]
    fn gemini_reports_the_temperature_ceiling_and_keeps_thinking_at_its_default() {
        let controls = gemini_model_controls(&json!({"thinking": true, "maxTemperature": 2.0}));
        assert!(controls.reasoning.is_empty());
        assert_eq!(controls.max_temperature, Some(2.0));
        assert_eq!(controls.temperature, ParameterSupport::Value);
    }

    #[test]
    fn ollama_distinguishes_boolean_thinking_from_gpt_oss_levels() {
        let hybrid = ollama_model_controls(
            "qwen3:8b",
            &json!({"capabilities": ["completion", "thinking"], "details": {"family": "qwen3"}}),
        );
        assert_eq!(hybrid.reasoning, BTreeSet::from([ReasoningMode::Disabled]));

        let gpt_oss = ollama_model_controls(
            "gpt-oss:20b",
            &json!({"capabilities": ["completion", "thinking"], "details": {"family": "gptoss"}}),
        );
        assert_eq!(gpt_oss.reasoning, BTreeSet::from([ReasoningMode::Effort]));
        assert_eq!(names(&gpt_oss), ["low", "medium", "high"]);

        let plain = ollama_model_controls("llama3.2", &json!({"capabilities": ["completion"]}));
        assert!(plain.reasoning.is_empty());
    }

    #[test]
    fn openai_probes_translate_validation_errors_into_allowed_options() {
        let effort = openai_effort_probe(
            400,
            br#"{"error":{"message":"Invalid value: '__probe__'. Supported values are: 'none', 'low', 'medium', 'high', 'xhigh', and 'max'.","type":"invalid_request_error","param":"reasoning.effort","code":"invalid_value"}}"#,
        );
        let temperature = openai_temperature_probe(
            400,
            br#"{"error":{"message":"Unsupported parameter: 'temperature' is not supported with this model.","param":"temperature","code":"unsupported_parameter"}}"#,
        );
        let controls = openai_model_controls(&effort, &temperature);

        assert_eq!(
            controls.reasoning,
            BTreeSet::from([ReasoningMode::Disabled, ReasoningMode::Effort])
        );
        assert_eq!(names(&controls), ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(controls.temperature, ParameterSupport::Unsupported);
        assert_eq!(controls.source, GenerationControlsSource::ValidationProbe);
    }

    #[test]
    fn openai_non_reasoning_models_keep_temperature_only() {
        let effort = openai_effort_probe(
            400,
            br#"{"error":{"message":"Unsupported parameter: 'reasoning.effort' is not supported with this model.","code":"unsupported_parameter"}}"#,
        );
        let temperature = openai_temperature_probe(
            400,
            br#"{"error":{"message":"Invalid 'temperature': decimal above maximum value. Expected a value <= 2, but got 3 instead.","code":"decimal_above_max_value"}}"#,
        );
        let controls = openai_model_controls(&effort, &temperature);
        assert!(controls.reasoning.is_empty());
        assert_eq!(controls.temperature, ParameterSupport::NullableValue);
        assert_eq!(controls.max_temperature, Some(2.0));
    }

    #[test]
    fn inconclusive_openai_probes_offer_only_provider_defaults() {
        let effort = openai_effort_probe(200, br#"{"id":"resp_1"}"#);
        let temperature = openai_temperature_probe(500, b"gateway error");
        let controls = openai_model_controls(&effort, &temperature);
        assert!(controls.reasoning.is_empty());
        assert_eq!(controls.temperature, ParameterSupport::Unsupported);
        assert_eq!(
            controls.source,
            GenerationControlsSource::ProviderDefaultOnly
        );
    }
}
