//! Proof that the stop condition (token budget + stop sequences) actually
//! changes generation, instead of being a knob that silently does nothing.
//!
//! Runs the identical prompt twice against the live endpoint — once with the
//! current settings' stop condition disabled, once with it applied — and
//! reports finish_reason, token/char counts and the two texts side by side.
//!
//! Text differing between runs proves nothing by itself: two unconstrained
//! calls to a non-deterministic model produce different text anyway. So the
//! check instead looks for the causal signature each mechanism leaves behind:
//! a token budget must yield `finish_reason=length` with `completion_tokens`
//! at (or just under) the configured budget; a stop sequence must yield
//! `finish_reason=stop` with a shorter, cut-off answer than the unconstrained
//! run.

use crate::api::{self, ChatMessage, Endpoint};
use crate::config::{Res, Settings, TEMP_MAX, TEMP_MIN};

pub const DEFAULT_PROMPT: &str =
    "Explain in detail how photosynthesis works, step by step, in at least 10 sentences.";

/// Prompt for the temperature proof. Deliberately open-ended *and* short:
/// temperature only shows up where the model has several equally good next
/// tokens to choose from, so a question with one obviously-right answer
/// ("what is 2+2") stays identical at every temperature and proves nothing.
pub const DEFAULT_TEMP_PROMPT: &str = "Name one animal. Reply with just the animal name, nothing else.";

pub const DEFAULT_TEMP_RUNS: usize = 4;
const MAX_TEMP_RUNS: usize = 10;

pub struct VerifyRun {
    pub label: &'static str,
    pub finish_reason: String,
    pub chars: usize,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub text: String,
}

pub struct VerifyReport {
    pub prompt: String,
    pub budget_tokens: Option<u32>,
    pub stop_configured: bool,
    pub off: VerifyRun,
    pub on: VerifyRun,
}

impl VerifyReport {
    /// True only when the "on" run shows the specific, mechanical signature
    /// of whichever lever was configured — not just "the text is different"
    /// (which non-determinism gives you for free).
    pub fn stop_condition_had_effect(&self) -> bool {
        let budget_confirmed = self.budget_tokens.is_some_and(|budget| {
            self.on.finish_reason == "length" && self.on.completion_tokens <= budget as u64
        });
        let stop_confirmed = self.stop_configured
            && self.on.finish_reason == "stop"
            && self.on.chars < self.off.chars;
        budget_confirmed || stop_confirmed
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("prompt: {}\n\n", self.prompt));
        for run in [&self.off, &self.on] {
            out.push_str(&format!(
                "[{}] finish={} chars={} completion_tokens={} reasoning_tokens={}\n{}\n\n",
                run.label, run.finish_reason, run.chars, run.completion_tokens, run.reasoning_tokens, run.text
            ));
        }
        out.push_str(if self.stop_condition_had_effect() {
            "=> stop condition CONFIRMED: the \"on\" run shows the mechanical signature (finish_reason + length) of the configured lever.\n"
        } else {
            "=> stop condition had NO measurable effect on this run — investigate before trusting it.\n"
        });
        out
    }
}

pub fn run(ep: &Endpoint, settings: &Settings, prompt: &str) -> Res<VerifyReport> {
    if settings.budget_tokens.is_none() && settings.stop.is_empty() {
        return Err(
            "no stop condition is configured — set a token budget or a stop sequence first \
             (/settings, or /stop add <seq>)"
                .into(),
        );
    }

    // The proof is about raw generation length, independent of whatever
    // JSON mode happens to be active in the caller's settings — leaving it
    // on would confuse token-budget truncation with schema formatting.
    let mut on_settings = settings.clone();
    on_settings.json_mode.enabled = false;
    let off_settings = on_settings.without_stop_condition();
    let history = vec![ChatMessage::user(prompt)];

    let off_outcome = api::chat(ep, &off_settings, "", &history, None)?;
    let on_outcome = api::chat(ep, &on_settings, "", &history, None)?;

    let to_run = |label: &'static str, o: &api::Outcome| VerifyRun {
        label,
        finish_reason: o.finish_reason.clone().unwrap_or_else(|| "?".into()),
        chars: o.text().chars().count(),
        completion_tokens: o.usage.completion_tokens,
        reasoning_tokens: o.usage.reasoning_tokens,
        text: o.text().to_string(),
    };

    Ok(VerifyReport {
        prompt: prompt.to_string(),
        budget_tokens: settings.budget_tokens,
        stop_configured: !settings.stop.is_empty(),
        off: to_run("stop condition OFF", &off_outcome),
        on: to_run("stop condition ON ", &on_outcome),
    })
}

/// One side of the temperature comparison: the same prompt sampled `runs`
/// times at a single temperature.
pub struct TempSide {
    pub label: &'static str,
    pub temperature: f32,
    pub answers: Vec<String>,
}

impl TempSide {
    pub fn distinct(&self) -> usize {
        let mut seen: Vec<&str> = Vec::new();
        for a in &self.answers {
            if !seen.contains(&a.as_str()) {
                seen.push(a);
            }
        }
        seen.len()
    }
}

pub enum TempVerdict {
    /// Cold side collapsed to one answer, hot side spread out: temperature is
    /// the only thing that changed, so it is the only thing that can explain it.
    Confirmed,
    /// Both sides collapsed — temperature reached the model but nothing in the
    /// visible answer moved.
    NoEffect,
    /// The cold side was not deterministic, so the two sides cannot be
    /// compared at all: something other than temperature is varying.
    Inconclusive,
}

pub struct TempReport {
    pub prompt: String,
    pub cold: TempSide,
    pub hot: TempSide,
    /// The request body the hot run actually put on the wire, minus
    /// `messages` — this is what answers "are the settings even being sent?".
    pub sent: String,
    /// Dampers that are active in the caller's settings and can hide
    /// temperature even when it is applied correctly.
    pub thinking: bool,
    pub top_k_is_provider_default: bool,
    pub top_p_is_provider_default: bool,
}

impl TempReport {
    pub fn verdict(&self) -> TempVerdict {
        if self.cold.distinct() > 1 {
            return TempVerdict::Inconclusive;
        }
        if self.hot.distinct() > self.cold.distinct() {
            TempVerdict::Confirmed
        } else {
            TempVerdict::NoEffect
        }
    }

    pub fn headline(&self) -> String {
        match self.verdict() {
            TempVerdict::Confirmed => format!(
                "temperature CONFIRMED working: {} -> 1 answer, {} -> {} different answers",
                self.cold.temperature,
                self.hot.temperature,
                self.hot.distinct()
            ),
            TempVerdict::NoEffect => format!(
                "temperature had NO visible effect: {} and {} both gave the same single answer",
                self.cold.temperature, self.hot.temperature
            ),
            TempVerdict::Inconclusive => format!(
                "INCONCLUSIVE: temperature {} should be deterministic but gave {} different answers",
                self.cold.temperature,
                self.cold.distinct()
            ),
        }
    }

    /// Why a correctly-sent temperature can still look dead. Only listed when
    /// the run did not confirm, so a passing proof stays terse.
    fn hints(&self) -> Vec<&'static str> {
        let mut hints = Vec::new();
        if self.thinking {
            hints.push(
                "reasoning is on: the model thinks its way to the same conclusion and the \
                 visible answer converges regardless of temperature — retry with /effort none",
            );
        }
        if self.top_k_is_provider_default {
            hints.push(
                "top_k is at the provider's default, which truncates the distribution before \
                 temperature can widen it — set top_k=full (-1) in /settings",
            );
        }
        if self.top_p_is_provider_default {
            hints.push(
                "top_p is at the provider's default, another truncation applied ahead of \
                 temperature — set top_p=1.0 in /settings",
            );
        }
        hints.push(
            "some questions have one dominant answer at any temperature — retry with an \
             open-ended prompt: /temp verify Name one colour, one word only.",
        );
        hints
    }

    pub fn render(&self) -> String {
        let mut out = format!("prompt: {}\n\n", self.prompt);
        out.push_str(&format!("sent on the wire: {}\n\n", self.sent));
        for side in [&self.cold, &self.hot] {
            out.push_str(&format!(
                "[{} temperature={}] {} distinct of {} runs\n",
                side.label,
                side.temperature,
                side.distinct(),
                side.answers.len()
            ));
            for (i, a) in side.answers.iter().enumerate() {
                out.push_str(&format!("  {}. {}\n", i + 1, a));
            }
            out.push('\n');
        }
        out.push_str(&format!("=> {}\n", self.headline()));
        if !matches!(self.verdict(), TempVerdict::Confirmed) {
            for hint in self.hints() {
                out.push_str(&format!("   - {hint}\n"));
            }
        }
        out
    }
}

/// Proof that `temperature` actually reaches the model and changes what it
/// says. Sends the identical prompt `runs` times at `TEMP_MIN` and `runs`
/// times at the configured temperature, with everything else held fixed.
///
/// As with the stop condition, "the answers differ" is not on its own a
/// proof — a model is non-deterministic anyway. The signature looked for
/// here is the *pair*: temperature 0 is greedy decoding, so it must collapse
/// to a single answer; only then does a spread on the hot side have nothing
/// left to attribute it to but temperature. A cold side that varies means
/// something else is loose (a router picking different backends, say), and
/// the run reports itself inconclusive rather than claiming a pass.
pub fn run_temperature(
    ep: &Endpoint,
    settings: &Settings,
    prompt: &str,
    runs: usize,
) -> Res<TempReport> {
    if !(2..=MAX_TEMP_RUNS).contains(&runs) {
        return Err(format!("runs must be between 2 and {MAX_TEMP_RUNS} (got {runs})"));
    }
    let hot_temp = match settings.temperature {
        Some(t) if t > TEMP_MIN => t,
        _ => TEMP_MAX,
    };

    // Everything that could collapse or truncate the answers independently of
    // temperature is cleared: a stop sequence or a small token budget would cut
    // every answer to the same prefix, JSON mode would force one shape, and
    // max_chars truncates client-side after the fact. What is deliberately
    // *kept* is effort/top_k/top_p — the point is to test the user's own
    // sampling setup, dampers included, and name them in the report.
    let mut base = settings.without_stop_condition();
    base.json_mode.enabled = false;
    base.max_chars = None;

    let mut cold_settings = base.clone();
    cold_settings.temperature = Some(TEMP_MIN);
    let mut hot_settings = base;
    hot_settings.temperature = Some(hot_temp);

    let history = vec![ChatMessage::user(prompt)];
    let sent = render_sent_body(ep, &hot_settings, &history);

    // Sequential on purpose: the provider handles concurrent requests from one
    // client poorly, and a queued/rate-limited call would show up as a bogus
    // difference between the two sides.
    let sample = |s: &Settings| -> Res<Vec<String>> {
        let mut answers = Vec::with_capacity(runs);
        for _ in 0..runs {
            let outcome = api::chat(ep, s, "", &history, None)?;
            answers.push(normalize(outcome.text()));
        }
        Ok(answers)
    };

    let cold = TempSide {
        label: "cold",
        temperature: TEMP_MIN,
        answers: sample(&cold_settings)?,
    };
    let hot = TempSide {
        label: "hot ",
        temperature: hot_temp,
        answers: sample(&hot_settings)?,
    };

    Ok(TempReport {
        prompt: prompt.to_string(),
        cold,
        hot,
        sent,
        thinking: settings.thinking(),
        top_k_is_provider_default: settings.top_k.is_none(),
        top_p_is_provider_default: settings.top_p.is_none(),
    })
}

/// The real request body, minus `messages`, so the report shows the sampling
/// fields exactly as they leave the process rather than as the UI describes
/// them.
fn render_sent_body(ep: &Endpoint, settings: &Settings, history: &[ChatMessage]) -> String {
    let mut body = api::build_body(&ep.model, settings, "", history, None);
    if let Some(obj) = body.as_object_mut() {
        obj.remove("messages");
    }
    body.to_string()
}

/// Answers are compared after collapsing whitespace and case, so "Dolphin"
/// and "dolphin.\n" count as the same answer rather than as evidence that
/// temperature did something.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(label: &'static str, finish: &str, tokens: u64, text: &str) -> VerifyRun {
        VerifyRun {
            label,
            finish_reason: finish.into(),
            chars: text.chars().count(),
            completion_tokens: tokens,
            reasoning_tokens: 0,
            text: text.into(),
        }
    }

    #[test]
    fn budget_is_confirmed_only_when_length_and_within_budget() {
        let report = VerifyReport {
            prompt: "p".into(),
            budget_tokens: Some(60),
            stop_configured: false,
            off: fixture("off", "stop", 300, "a very long unconstrained answer"),
            on: fixture("on", "length", 60, "a very long uncon"),
        };
        assert!(report.stop_condition_had_effect());
    }

    #[test]
    fn budget_is_not_confirmed_if_on_run_finished_naturally() {
        // The model just happened to answer briefly — not proof the budget did anything.
        let report = VerifyReport {
            prompt: "p".into(),
            budget_tokens: Some(600),
            stop_configured: false,
            off: fixture("off", "stop", 300, "a very long unconstrained answer"),
            on: fixture("on", "stop", 50, "a short answer"),
        };
        assert!(!report.stop_condition_had_effect());
    }

    #[test]
    fn stop_sequence_is_confirmed_when_shorter_and_finish_is_stop() {
        let report = VerifyReport {
            prompt: "p".into(),
            budget_tokens: None,
            stop_configured: true,
            off: fixture("off", "stop", 300, "one\n\ntwo\n\nthree"),
            on: fixture("on", "stop", 40, "one"),
        };
        assert!(report.stop_condition_had_effect());
    }

    #[test]
    fn no_lever_configured_means_no_effect_even_if_text_differs() {
        // Two unconstrained, non-deterministic calls naturally produce
        // different text — that alone must never read as "confirmed".
        let report = VerifyReport {
            prompt: "p".into(),
            budget_tokens: None,
            stop_configured: false,
            off: fixture("off", "stop", 300, "first phrasing of the answer"),
            on: fixture("on", "stop", 280, "second, slightly shorter phrasing"),
        };
        assert!(!report.stop_condition_had_effect());
    }

    fn temp_report(cold: &[&str], hot: &[&str]) -> TempReport {
        let side = |label: &'static str, t: f32, xs: &[&str]| TempSide {
            label,
            temperature: t,
            answers: xs.iter().map(|s| normalize(s)).collect(),
        };
        TempReport {
            prompt: "p".into(),
            cold: side("cold", 0.0, cold),
            hot: side("hot ", 2.0, hot),
            sent: "{}".into(),
            thinking: false,
            top_k_is_provider_default: true,
            top_p_is_provider_default: true,
        }
    }

    #[test]
    fn temperature_is_confirmed_when_greedy_collapses_and_hot_spreads() {
        let r = temp_report(&["Cat", "Cat", "Cat"], &["Cat", "Owl", "Frog"]);
        assert!(matches!(r.verdict(), TempVerdict::Confirmed));
    }

    #[test]
    fn temperature_that_changes_nothing_is_not_confirmed() {
        let r = temp_report(&["Cat", "Cat", "Cat"], &["Cat", "Cat", "Cat"]);
        assert!(matches!(r.verdict(), TempVerdict::NoEffect));
        // The report has to say *why* a live knob can still look dead.
        assert!(r.render().contains("top_k is at the provider's default"));
    }

    #[test]
    fn a_non_deterministic_cold_side_is_inconclusive_not_a_pass() {
        // Hot spreads more, but temperature 0 already varied, so the spread
        // cannot be attributed to temperature.
        let r = temp_report(&["Cat", "Owl", "Cat"], &["Cat", "Owl", "Frog"]);
        assert!(matches!(r.verdict(), TempVerdict::Inconclusive));
    }

    #[test]
    fn answers_differing_only_in_case_or_punctuation_are_one_answer() {
        let r = temp_report(&["Cat", "cat.", " CAT\n"], &["Cat", "cat!", "CAT"]);
        assert_eq!(r.cold.distinct(), 1);
        assert!(matches!(r.verdict(), TempVerdict::NoEffect));
    }

    #[test]
    fn run_temperature_rejects_a_useless_sample_size() {
        let ep = Endpoint::dummy();
        let s = Settings::default();
        // One run per side can never show a spread, so it is refused up front
        // rather than reporting a confident "no effect".
        assert!(run_temperature(&ep, &s, "hi", 1).is_err());
        assert!(run_temperature(&ep, &s, "hi", MAX_TEMP_RUNS + 1).is_err());
    }

    #[test]
    fn run_refuses_when_nothing_is_configured() {
        let ep = Endpoint::dummy();
        let Err(err) = run(&ep, &Settings::default(), "hi") else {
            panic!("expected an error when no stop condition is configured");
        };
        assert!(err.contains("no stop condition is configured"));
    }
}
