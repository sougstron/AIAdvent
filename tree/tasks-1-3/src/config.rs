//! Chat settings shared by the CLI, the TUI and the stop-condition self-test.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;

pub type Res<T> = Result<T, String>;

/// How much the model is allowed to reason before answering.
/// Maps straight onto the provider's `reasoning_effort` field.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Effort {
    /// Reasoning forced off (`reasoning_effort: none` + `enable_thinking: false`).
    None,
    Low,
    Medium,
    High,
}

impl Effort {
    pub const ALL: [Effort; 4] = [Effort::None, Effort::Low, Effort::Medium, Effort::High];

    pub fn label(self) -> &'static str {
        match self {
            Effort::None => "none",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }

    pub fn parse(s: &str) -> Res<Effort> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => Ok(Effort::None),
            "low" => Ok(Effort::Low),
            "medium" | "med" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            other => Err(format!("unknown effort `{other}` (none|low|medium|high)")),
        }
    }

    pub fn cycle(self, delta: i32) -> Effort {
        let i = Effort::ALL.iter().position(|e| *e == self).unwrap_or(0) as i32;
        let n = Effort::ALL.len() as i32;
        Effort::ALL[(i + delta).rem_euclid(n) as usize]
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The structured-output mode: off (free prose) or on with a JSON Schema the
/// model must fill. The schema is editable at runtime via `/json`.
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

/// A flat, all-string-fields schema — the common case from the spec example
/// (`{title, game, publisher, summary}`), buildable with `/json fields a,b,c`.
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

/// Everything one turn of generation needs. Built once per app, mutated live
/// by `/effort`, `/json`, `/settings`, and persisted per session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub effort: Effort,
    pub json_mode: JsonMode,
    /// Hard character cap on the *visible* answer. Enforced twice: as a
    /// system-prompt hint (soft, helps the model wrap up cleanly) and as a
    /// client-side truncation after the fact (hard, always true regardless
    /// of what the model does).
    pub max_chars: Option<usize>,
    /// Generation token budget — counts reasoning *and* visible tokens.
    /// This is the primary "stop condition" lever: set low enough and the
    /// model is cut off mid-thought, deterministically (`finish_reason=length`).
    pub budget_tokens: Option<u32>,
    /// Literal stop strings — the other stop-condition lever. The provider
    /// halts generation the instant one is emitted (`finish_reason=stop`).
    pub stop: Vec<String>,
    /// Sampling temperature. The provider accepts `TEMP_MIN..=TEMP_MAX` and
    /// rejects anything outside with HTTP 400, so it is validated client-side.
    pub temperature: Option<f32>,
    /// Nucleus sampling cutoff. `None` leaves the provider's default in place.
    #[serde(default)]
    pub top_p: Option<f32>,
    /// Top-k cutoff; `-1` disables it (full vocabulary). `None` leaves the
    /// provider's default in place — and that default is what makes a high
    /// temperature look like it does nothing, since it truncates the
    /// distribution before temperature ever gets to widen it.
    #[serde(default)]
    pub top_k: Option<i32>,
}

/// Range the provider validates `temperature` against — outside it the call
/// fails with `temperature: Validation error: range`, confirmed live.
pub const TEMP_MIN: f32 = 0.0;
pub const TEMP_MAX: f32 = 2.0;

/// Parses a temperature and rejects out-of-range values here rather than
/// letting the provider answer with an HTTP 400.
pub fn parse_temperature(t: f32) -> Res<f32> {
    if !(TEMP_MIN..=TEMP_MAX).contains(&t) {
        return Err(format!(
            "temperature must be between {TEMP_MIN} and {TEMP_MAX} (got {t})"
        ));
    }
    Ok(t)
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            effort: Effort::None,
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
    pub fn thinking(&self) -> bool {
        self.effort != Effort::None
    }

    /// One-line status strip, shown in the TUI header and CLI stats.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("effort={}", self.effort)];
        parts.push(format!(
            "json={}",
            if self.json_mode.enabled { "on" } else { "off" }
        ));
        parts.push(match self.max_chars {
            Some(n) => format!("max_chars={n}"),
            None => "max_chars=off".into(),
        });
        parts.push(match self.budget_tokens {
            Some(n) => format!("budget={n}tok"),
            None => "budget=off".into(),
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

    /// A copy with both stop-condition levers cleared — the "off" side of
    /// the `/verify` comparison.
    pub fn without_stop_condition(&self) -> Settings {
        let mut s = self.clone();
        s.budget_tokens = None;
        s.stop.clear();
        s
    }
}

/// `-1` is the provider's "no top-k cutoff at all" value; spell that out
/// instead of showing a bare negative number in the UI.
pub fn render_top_k(k: i32) -> String {
    if k < 0 {
        "full".into()
    } else {
        k.to_string()
    }
}

/// Escapes control characters so stop sequences stay readable on one line.
pub fn render_stops(stop: &[String]) -> String {
    stop.iter()
        .map(|s| format!("\"{}\"", s.replace('\n', "\\n").replace('\t', "\\t")))
        .collect::<Vec<_>>()
        .join(",")
}

/// Turns the literal two-character `\n` typed at a prompt into a real newline.
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
    fn effort_cycles_and_round_trips_through_label() {
        for e in Effort::ALL {
            assert_eq!(Effort::parse(e.label()).unwrap(), e);
        }
        assert_eq!(Effort::None.cycle(1), Effort::Low);
        assert_eq!(Effort::None.cycle(-1), Effort::High);
    }

    #[test]
    fn temperature_range_matches_the_provider() {
        assert_eq!(parse_temperature(0.0).unwrap(), 0.0);
        assert_eq!(parse_temperature(2.0).unwrap(), 2.0);
        // 2.01 is what the endpoint itself rejects with HTTP 400.
        assert!(parse_temperature(2.01).is_err());
        assert!(parse_temperature(-0.1).is_err());
    }

    #[test]
    fn summary_shows_temperature_and_only_set_sampling_knobs() {
        let mut s = Settings::default();
        assert!(s.summary().contains("temp=off"));
        assert!(!s.summary().contains("top_k"));
        s.temperature = Some(1.2);
        s.top_k = Some(-1);
        let summary = s.summary();
        assert!(summary.contains("temp=1.2"));
        assert!(summary.contains("top_k=full"));
        assert!(!summary.contains("top_p"));
    }

    #[test]
    fn thinking_is_off_only_at_effort_none() {
        let mut s = Settings::default();
        assert!(!s.thinking());
        s.effort = Effort::Low;
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
        assert_eq!(cleared.max_chars, Some(200)); // unrelated setting untouched
    }
}
