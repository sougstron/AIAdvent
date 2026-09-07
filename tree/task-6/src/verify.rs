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

use crate::agent::Agent;
use crate::api::ChatMessage;
use crate::config::{Res, Settings};

pub const DEFAULT_PROMPT: &str =
    "Explain in detail how photosynthesis works, step by step, in at least 10 sentences.";

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

pub fn run(settings: &Settings, prompt: &str) -> Res<VerifyReport> {
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

    let off_agent = Agent::new(off_settings)?;
    let on_agent = Agent::new(on_settings)?;
    let off_reply = off_agent.complete(&history)?;
    let on_reply = on_agent.complete(&history)?;

    let to_run = |label: &'static str, r: &crate::agent::Reply| VerifyRun {
        label,
        finish_reason: r.finish_reason.clone().unwrap_or_else(|| "?".into()),
        chars: r.text.chars().count(),
        completion_tokens: r.usage.completion_tokens,
        reasoning_tokens: r.usage.reasoning_tokens,
        text: r.text.clone(),
    };

    Ok(VerifyReport {
        prompt: prompt.to_string(),
        budget_tokens: settings.budget_tokens,
        stop_configured: !settings.stop.is_empty(),
        off: to_run("stop condition OFF", &off_reply),
        on: to_run("stop condition ON ", &on_reply),
    })
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

    #[test]
    fn run_refuses_when_nothing_is_configured() {
        let Err(err) = run(&Settings::default(), "hi") else {
            panic!("expected an error when no stop condition is configured");
        };
        assert!(err.contains("no stop condition is configured"));
    }
}
