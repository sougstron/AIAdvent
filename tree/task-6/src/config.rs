//! Chat settings shared by the CLI, the TUI and the agent.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;

pub type Res<T> = Result<T, String>;

/// Default and only model that may be used for live completions.
pub const DEFAULT_MODEL: &str = "glm-5.3-flash";

/// Catalog from `GET https://api.z.ai/api/paas/v4/models` on 2026-09-07
/// (10 ids, `object: list`). Only `glm-5.3-flash` is cheap enough to call;
/// the rest are exposed as selectable settings but refused at send time.
pub const MODEL_CATALOG: &[&str] = &[
    "glm-4.5",
    "glm-4.5-air",
    "glm-4.6",
    "glm-4.7",
    "glm-5",
    "glm-5-turbo",
    "glm-5.1",
    "glm-5.2",
    "glm-5.3",
    "glm-5.3-flash",
];

pub const DEFAULT_SYSTEM_PROMPT: &str = "Ты — полезный ассистент. Отвечай ясно и по делу.";

/// Documented z.ai range for `temperature` is `[0.0, 1.0]`. Live, 1.5 and 2.0
/// returned HTTP 200 on `glm-5.3-flash` (the endpoint does not 400 the way
/// yolo-auto did at 2.01) — we still clamp to the documented range rather
/// than send values the docs say are invalid.
pub const TEMP_MIN: f32 = 0.0;
pub const TEMP_MAX: f32 = 1.0;

/// Documented `top_p` range on the Chat Completions schema: `[0.01, 1.0]`.
pub const TOP_P_MIN: f32 = 0.01;
pub const TOP_P_MAX: f32 = 1.0;

pub const MAX_TOKENS_MIN: u32 = 1;
pub const MAX_TOKENS_MAX: u32 = 131_072;

/// Reasoning effort sent as `reasoning_effort`.
///
/// Live against `glm-5.3-flash` (HTTP 400, error code 1210):
/// `thinking.type: disabled`, `reasoning_effort: none`, and
/// `reasoning_effort: medium` are all rejected with
/// "This model always engages in thinking and cannot be disabled;
/// please use low, high, or max".
///
/// `None` / `Medium` stay in the enum so older saved sessions still
/// deserialize; [`Effort::wire`] maps them onto a legal value.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Effort {
    /// Saved-session / `/effort none` alias — cannot actually disable thinking.
    None,
    #[default]
    Low,
    /// Saved-session / `/effort medium` alias — the API rejects `medium`.
    Medium,
    High,
    Max,
}

impl Effort {
    pub const ALL: [Effort; 3] = [Effort::Low, Effort::High, Effort::Max];

    pub fn label(self) -> &'static str {
        match self {
            Effort::None => "none",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Max => "max",
        }
    }

    /// Value actually put on the wire. Never `none` or `medium`.
    pub fn wire(self) -> &'static str {
        match self {
            Effort::None | Effort::Low => "low",
            Effort::Medium | Effort::High => "high",
            Effort::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Res<Effort> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => Ok(Effort::Low),
            "low" => Ok(Effort::Low),
            "medium" | "med" => Ok(Effort::High),
            "high" => Ok(Effort::High),
            "max" => Ok(Effort::Max),
            other => Err(format!(
                "unknown effort `{other}` (low|high|max; none/medium are aliases — glm-5.3-flash rejects them)"
            )),
        }
    }

    pub fn cycle(self, delta: i32) -> Effort {
        let current = match self {
            Effort::None | Effort::Low => Effort::Low,
            Effort::Medium | Effort::High => Effort::High,
            Effort::Max => Effort::Max,
        };
        let i = Effort::ALL
            .iter()
            .position(|e| *e == current)
            .unwrap_or(0) as i32;
        let n = Effort::ALL.len() as i32;
        Effort::ALL[(i + delta).rem_euclid(n) as usize]
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Structured-output mode: off (free prose) or on with a JSON Schema the
/// model is asked to fill. z.ai documents `response_format.type: json_object`
/// for GLM-5.3; live, `json_schema`/`strict` was accepted but not enforced
/// (fenced JSON plus extra fields). The schema is therefore a prompt hint
/// plus client-side flattening, not a provider guarantee.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JsonMode {
    pub enabled: bool,
    pub schema: Value,
}

impl Default for JsonMode {
    fn default() -> Self {
        JsonMode {
            enabled: false,
            schema: default_schema(),
        }
    }
}

pub fn flat_string_schema(fields: &[String]) -> Value {
    let mut properties = serde_json::Map::new();
    for f in fields {
        properties.insert(f.clone(), json!({ "type": "string" }));
    }
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": fields,
        "properties": properties,
    })
}

fn default_schema() -> Value {
    flat_string_schema(&["title".into(), "summary".into()])
}

fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}

fn default_system_prompt() -> String {
    DEFAULT_SYSTEM_PROMPT.to_string()
}

const fn default_context_enabled() -> bool {
    true
}

/// Everything one turn of generation needs. Built once per app, mutated live
/// by `/effort`, `/json`, `/settings`, and persisted per session.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
    /// When true, `Agent` injects discovered AGENTS.md / CLAUDE.md files
    /// into the system message. Toggled at runtime via `set_context_enabled`.
    #[serde(default = "default_context_enabled")]
    pub context_enabled: bool,
    pub effort: Effort,
    pub json_mode: JsonMode,
    /// Hard character cap on the *visible* answer. Enforced twice: as a
    /// system-prompt hint and as client-side truncation after the fact.
    pub max_chars: Option<usize>,
    /// Cap on generated tokens (reasoning + visible). Sent as `max_tokens`.
    /// Kept under this name so existing TUI/CLI/`/verify` wiring still compiles;
    /// `max_tokens` is accepted as a serde alias for older/newer session files.
    #[serde(default, alias = "max_tokens")]
    pub budget_tokens: Option<u32>,
    /// Literal stop strings. Confirmed live: `stop: ["3"]` on a counting
    /// prompt returned `finish_reason=stop` with content cut before `3`.
    pub stop: Vec<String>,
    /// Sampling temperature, clamped to [`TEMP_MIN`]..=[`TEMP_MAX`].
    pub temperature: Option<f32>,
    /// Nucleus sampling cutoff. `None` leaves the provider default (0.95).
    #[serde(default)]
    pub top_p: Option<f32>,
    /// Top-k cutoff; `-1` is "full vocabulary". Not in the public Chat
    /// Completions docs, but the Java request class has the field: sending
    /// `top_k: "nope"` returned HTTP 400 naming
    /// `ChatCompletionRequest["top_k"]`. Sent when set.
    #[serde(default)]
    pub top_k: Option<i32>,
}

pub fn parse_temperature(t: f32) -> Res<f32> {
    if !(TEMP_MIN..=TEMP_MAX).contains(&t) {
        return Err(format!(
            "temperature must be between {TEMP_MIN} and {TEMP_MAX} (got {t})"
        ));
    }
    Ok(t)
}

pub fn parse_top_p(p: f32) -> Res<f32> {
    if !(TOP_P_MIN..=TOP_P_MAX).contains(&p) {
        return Err(format!(
            "top_p must be between {TOP_P_MIN} and {TOP_P_MAX} (got {p})"
        ));
    }
    Ok(p)
}

pub fn parse_max_tokens(n: u32) -> Res<u32> {
    if !(MAX_TOKENS_MIN..=MAX_TOKENS_MAX).contains(&n) {
        return Err(format!(
            "max_tokens must be between {MAX_TOKENS_MIN} and {MAX_TOKENS_MAX} (got {n})"
        ));
    }
    Ok(n)
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            model: default_model(),
            system_prompt: default_system_prompt(),
            context_enabled: true,
            effort: Effort::Low,
            json_mode: JsonMode::default(),
            max_chars: None,
            budget_tokens: None,
            stop: Vec::new(),
            temperature: None,
            top_p: None,
            top_k: None,
        }
    }
}

impl Settings {
    /// Token cap sent as `max_tokens`.
    pub fn max_tokens(&self) -> Option<u32> {
        self.budget_tokens
    }

    pub fn thinking(&self) -> bool {
        // glm-5.3-flash cannot disable thinking (HTTP 400 / 1210).
        true
    }

    /// Clamp every lever into the range the endpoint documents. Called by
    /// the agent before building a body so TUI cycling cannot smuggle 2.0.
    pub fn clamp(&mut self) {
        if self.model.trim().is_empty() {
            self.model = default_model();
        }
        self.effort = match self.effort {
            Effort::None => Effort::Low,
            Effort::Medium => Effort::High,
            other => other,
        };
        if let Some(t) = self.temperature {
            self.temperature = Some(t.clamp(TEMP_MIN, TEMP_MAX));
        }
        if let Some(p) = self.top_p {
            self.top_p = Some(p.clamp(TOP_P_MIN, TOP_P_MAX));
        }
        if let Some(n) = self.budget_tokens {
            self.budget_tokens = Some(n.clamp(MAX_TOKENS_MIN, MAX_TOKENS_MAX));
        }
        if let Some(k) = self.top_k {
            if k == 0 || k < -1 {
                self.top_k = None;
            }
        }
        if self.stop.len() > 4 {
            self.stop.truncate(4);
        }
    }

    pub fn summary(&self) -> String {
        let mut parts = vec![
            format!("model={}", self.model),
            format!("effort={}", self.effort.wire()),
        ];
        parts.push(format!(
            "thinking={}",
            if self.thinking() { "on" } else { "off" }
        ));
        parts.push(format!(
            "json={}",
            if self.json_mode.enabled { "on" } else { "off" }
        ));
        parts.push(format!(
            "context={}",
            if self.context_enabled { "on" } else { "off" }
        ));
        parts.push(match self.max_chars {
            Some(n) => format!("max_chars={n}"),
            None => "max_chars=off".into(),
        });
        parts.push(match self.budget_tokens {
            Some(n) => format!("max_tokens={n}"),
            None => "max_tokens=off".into(),
        });
        parts.push(if self.stop.is_empty() {
            "stop=off".into()
        } else {
            format!("stop={}", render_stops(&self.stop))
        });
        parts.push(match self.temperature {
            Some(t) => format!("temp={t}"),
            None => "temp=off".into(),
        });
        if let Some(p) = self.top_p {
            parts.push(format!("top_p={p}"));
        }
        if let Some(k) = self.top_k {
            parts.push(format!("top_k={}", render_top_k(k)));
        }
        parts.join("  ")
    }

    pub fn without_stop_condition(&self) -> Settings {
        let mut s = self.clone();
        s.budget_tokens = None;
        s.stop.clear();
        s
    }
}

pub fn render_top_k(k: i32) -> String {
    if k < 0 {
        "full".into()
    } else {
        k.to_string()
    }
}

pub fn render_stops(stop: &[String]) -> String {
    stop.iter()
        .map(|s| format!("\"{}\"", s.replace('\n', "\\n").replace('\t', "\\t")))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_cycles_real_wire_values() {
        for e in Effort::ALL {
            assert_eq!(Effort::parse(e.label()).unwrap(), e);
        }
        assert_eq!(Effort::Low.cycle(1), Effort::High);
        assert_eq!(Effort::Low.cycle(-1), Effort::Max);
        assert_eq!(Effort::Max.cycle(1), Effort::Low);
    }

    #[test]
    fn effort_aliases_map_onto_legal_wire_values() {
        assert_eq!(Effort::parse("none").unwrap(), Effort::Low);
        assert_eq!(Effort::parse("off").unwrap(), Effort::Low);
        assert_eq!(Effort::parse("medium").unwrap(), Effort::High);
        assert_eq!(Effort::None.wire(), "low");
        assert_eq!(Effort::Medium.wire(), "high");
        assert_eq!(Effort::Max.wire(), "max");
    }

    #[test]
    fn temperature_range_matches_z_ai_docs() {
        assert_eq!(parse_temperature(0.0).unwrap(), 0.0);
        assert_eq!(parse_temperature(1.0).unwrap(), 1.0);
        assert!(parse_temperature(1.5).is_err());
        assert!(parse_temperature(2.0).is_err());
        assert!(parse_temperature(-0.1).is_err());
    }

    #[test]
    fn clamp_pulls_legacy_levers_into_range() {
        let mut s = Settings {
            effort: Effort::None,
            temperature: Some(2.0),
            top_p: Some(0.0),
            budget_tokens: Some(0),
            top_k: Some(0),
            ..Settings::default()
        };
        s.clamp();
        assert_eq!(s.effort, Effort::Low);
        assert_eq!(s.temperature, Some(1.0));
        assert!((s.top_p.unwrap() - TOP_P_MIN).abs() < 1e-6);
        assert_eq!(s.budget_tokens, Some(MAX_TOKENS_MIN));
        assert_eq!(s.top_k, None);
    }

    #[test]
    fn summary_shows_model_and_only_set_sampling_knobs() {
        let mut s = Settings::default();
        assert!(s.summary().contains("model=glm-5.3-flash"));
        assert!(s.summary().contains("temp=off"));
        assert!(!s.summary().contains("top_k"));
        s.temperature = Some(0.7);
        s.top_k = Some(-1);
        let summary = s.summary();
        assert!(summary.contains("temp=0.7"));
        assert!(summary.contains("top_k=full"));
        assert!(!summary.contains("top_p"));
    }

    #[test]
    fn thinking_cannot_be_turned_off() {
        let mut s = Settings::default();
        assert!(s.thinking());
        s.effort = Effort::None;
        assert!(s.thinking());
    }

    #[test]
    fn without_stop_condition_clears_both_levers_only() {
        let s = Settings {
            budget_tokens: Some(64),
            stop: vec!["\n\n".into()],
            max_chars: Some(200),
            ..Settings::default()
        };
        let cleared = s.without_stop_condition();
        assert_eq!(cleared.budget_tokens, None);
        assert!(cleared.stop.is_empty());
        assert_eq!(cleared.max_chars, Some(200));
    }

    #[test]
    fn catalog_includes_the_live_default() {
        assert!(MODEL_CATALOG.contains(&DEFAULT_MODEL));
        assert_eq!(MODEL_CATALOG.len(), 10);
    }
}
