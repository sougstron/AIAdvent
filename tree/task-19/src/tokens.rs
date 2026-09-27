//! Token accounting behind the four footer meters.
//!
//! Two kinds of number live here and they are never mixed silently:
//!
//! * **measured** — what the provider reported in `usage` for a request that
//!   actually happened (`prompt_tokens`, `completion_tokens`, `total_tokens`).
//! * **estimated** — what the *next* request would cost, computed locally
//!   from the character count of the system prompt + history + the text still
//!   sitting in the input box. The app has no tokenizer for the provider's
//!   vocabulary, so this is a chars-per-token ratio, and every estimated
//!   number renders with a `~` so the screen never claims a precision it
//!   does not have.
//!
//! The ratio is not a constant guess: after every reply the meter divides the
//! characters it *would* have counted for that request by the `prompt_tokens`
//! the provider actually billed, and uses that ratio from then on. Russian
//! text costs far more tokens per character than English on these BPE
//! vocabularies (roughly 2 chars/token vs 4), so a fixed 4.0 would understate
//! a Russian chat by ~2x; one real request pins it down.

use crate::api::Usage;

/// Starting ratio, replaced by the calibrated one after the first reply.
/// 4 chars/token is the usual English rule of thumb for BPE vocabularies.
pub const DEFAULT_CHARS_PER_TOKEN: f64 = 4.0;

/// Guard rails for the calibrated ratio: a provider that reports a bogus
/// `prompt_tokens` (0, or a cached-prompt discount) must not turn the
/// estimate into nonsense.
const MIN_CHARS_PER_TOKEN: f64 = 1.0;
const MAX_CHARS_PER_TOKEN: f64 = 12.0;

/// Per-message envelope the provider bills on top of the content itself
/// (role tag + delimiters in the chat template). Small, but it is the
/// difference between "close" and "systematically low" on short turns.
const PER_MESSAGE_OVERHEAD_TOKENS: u64 = 4;

/// A token count plus whether it was measured or estimated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub value: u64,
    pub estimated: bool,
}

impl Tokens {
    pub fn measured(value: u64) -> Tokens {
        Tokens {
            value,
            estimated: false,
        }
    }

    pub fn estimated(value: u64) -> Tokens {
        Tokens {
            value,
            estimated: true,
        }
    }

    /// `1.2k~` — the compact form the footer renders.
    pub fn label(&self) -> String {
        format!(
            "{}{}",
            compact(self.value),
            if self.estimated { "~" } else { "" }
        )
    }
}

/// Rolling token state of one chat session.
#[derive(Clone, Debug)]
pub struct TokenMeter {
    /// Calibrated from real usage; `None` until the first reply lands.
    calibration: Option<f64>,
    /// Usage of the most recent reply. Its prompt plus completion is the
    /// measured size of the dialogue history currently retained by the app.
    last: Option<Usage>,
}

impl Default for TokenMeter {
    fn default() -> TokenMeter {
        TokenMeter::new()
    }
}

impl TokenMeter {
    pub fn new() -> TokenMeter {
        TokenMeter {
            calibration: None,
            last: None,
        }
    }

    /// `/new` starts a fresh session; the calibration is a property of the
    /// model and the language being typed, so it survives the reset.
    pub fn reset_session(&mut self) {
        self.last = None;
    }

    pub fn chars_per_token(&self) -> f64 {
        self.calibration.unwrap_or(DEFAULT_CHARS_PER_TOKEN)
    }

    /// Fold one finished reply in and recalibrate against the prompt sent.
    ///
    /// `prompt_chars` is the character count of exactly what went out as the
    /// request (system prompt + every history message), so that dividing it
    /// by the provider's `prompt_tokens` yields the ratio for *this* chat's
    /// language and model.
    pub fn record(&mut self, prompt_chars: usize, usage: &Usage) {
        if usage.prompt_tokens > 0 && prompt_chars > 0 {
            let ratio = prompt_chars as f64 / usage.prompt_tokens as f64;
            if (MIN_CHARS_PER_TOKEN..=MAX_CHARS_PER_TOKEN).contains(&ratio) {
                self.calibration = Some(ratio);
            }
        }
        self.last = Some(*usage);
    }

    /// Local estimate for `chars` of text spread over `messages` messages.
    pub fn estimate(&self, chars: usize, messages: usize) -> u64 {
        if chars == 0 && messages == 0 {
            return 0;
        }
        let body = (chars as f64 / self.chars_per_token()).ceil() as u64;
        body + messages as u64 * PER_MESSAGE_OVERHEAD_TOKENS
    }

    /// Stat 1 — the current user query only, not the system prompt or history
    /// which the API happens to resend alongside it.
    pub fn current_request(&self, request: &Shape) -> Tokens {
        Tokens::estimated(self.estimate(request.chars, request.messages))
    }

    /// Stat 2 — the dialogue history retained in this session. After a reply,
    /// the provider gives us its exact size as prompt + completion. A resumed
    /// session has no persisted usage, so it is explicitly estimated.
    pub fn session(&self, history: &Shape) -> Tokens {
        match self.last {
            Some(u) if u.prompt_tokens > 0 || u.completion_tokens > 0 => {
                Tokens::measured(u.prompt_tokens + u.completion_tokens)
            }
            _ => Tokens::estimated(self.estimate(history.chars, history.messages)),
        }
    }

    /// Stat 3 — the last reply's own output tokens (reasoning tokens are
    /// already inside `completion_tokens`; see `billing.rs`).
    pub fn last_reply(&self) -> Tokens {
        match self.last {
            Some(u) => Tokens::measured(u.completion_tokens),
            None => Tokens::measured(0),
        }
    }

    /// Stat 4 — how much of the model's context window the conversation
    /// currently occupies: the last request's prompt plus the reply it
    /// produced (both measured), plus whatever is being typed now.
    pub fn context_used(&self, conversation: &Shape, pending: &Shape) -> Tokens {
        match self.last {
            Some(u) if u.prompt_tokens > 0 => {
                let typed = self.estimate(pending.chars, pending.messages);
                Tokens {
                    value: u.prompt_tokens + u.completion_tokens + typed,
                    estimated: typed > 0,
                }
            }
            _ => Tokens::estimated(self.estimate(conversation.chars, conversation.messages)),
        }
    }
}

/// Size of a piece of a request in the two units the estimator needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    pub chars: usize,
    pub messages: usize,
}

impl Shape {
    pub fn new(chars: usize, messages: usize) -> Shape {
        Shape { chars, messages }
    }

    pub fn is_empty(&self) -> bool {
        self.chars == 0 && self.messages == 0
    }

    pub fn plus(self, other: Shape) -> Shape {
        Shape {
            chars: self.chars + other.chars,
            messages: self.messages + other.messages,
        }
    }
}

/// `912`, `15k`, `1.2M` — pi-style compact counts, so four meters fit on one
/// footer row even on a narrow terminal.
pub fn compact(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{}k", n / 1_000),
        _ => {
            let m = n as f64 / 1_000_000.0;
            if m < 10.0 {
                format!("{m:.1}M")
            } else {
                format!("{}M", m.round() as u64)
            }
        }
    }
}

/// `15k/1.0M (2%)` — used vs the model's context window. An unknown window
/// renders as `?` rather than a made-up number.
pub fn context_label(used: Tokens, limit: Option<u64>) -> String {
    match limit {
        Some(limit) if limit > 0 => {
            let pct = (used.value as f64 / limit as f64 * 100.0).round() as u64;
            format!("{}/{} ({}%)", used.label(), compact(limit), pct)
        }
        _ => format!("{}/?", used.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: u64, completion: u64) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            reasoning_tokens: 0,
            total_tokens: prompt + completion,
        }
    }

    #[test]
    fn compact_counts_read_like_pi() {
        assert_eq!(compact(0), "0");
        assert_eq!(compact(912), "912");
        assert_eq!(compact(9_999), "9999");
        assert_eq!(compact(15_400), "15k");
        assert_eq!(compact(999_999), "999k");
        assert_eq!(compact(1_048_576), "1.0M");
        assert_eq!(compact(12_000_000), "12M");
    }

    #[test]
    fn a_real_reply_calibrates_the_estimate_away_from_the_default() {
        let mut m = TokenMeter::new();
        assert_eq!(m.chars_per_token(), DEFAULT_CHARS_PER_TOKEN);
        // Russian text: the provider billed 500 tokens for 1000 chars.
        m.record(1000, &usage(500, 100));
        assert_eq!(m.chars_per_token(), 2.0);
        // 200 chars in one message now estimates at 100 + envelope, not 50.
        assert_eq!(m.estimate(200, 1), 104);
    }

    #[test]
    fn absurd_ratios_do_not_poison_the_calibration() {
        let mut m = TokenMeter::new();
        m.record(1000, &usage(1, 1)); // 1000 chars/token — a cached prompt
        assert_eq!(m.chars_per_token(), DEFAULT_CHARS_PER_TOKEN);
        m.record(1000, &usage(5000, 1)); // 0.2 chars/token — impossible
        assert_eq!(m.chars_per_token(), DEFAULT_CHARS_PER_TOKEN);
    }

    #[test]
    fn zero_usage_falls_back_to_an_estimate() {
        let mut m = TokenMeter::new();
        m.record(400, &Usage::default());
        assert_eq!(m.chars_per_token(), DEFAULT_CHARS_PER_TOKEN);
        assert_eq!(m.session(&Shape::new(400, 2)), Tokens::estimated(108));
    }

    #[test]
    fn session_is_current_history_not_repeated_api_billing() {
        let mut m = TokenMeter::new();
        m.record(400, &usage(100, 50));
        m.record(900, &usage(300, 70));
        assert_eq!(m.session(&Shape::default()), Tokens::measured(370));
        assert_eq!(m.last_reply(), Tokens::measured(70));
    }

    #[test]
    fn session_falls_back_to_an_estimate_before_the_first_reply() {
        let m = TokenMeter::new();
        let history = Shape::new(400, 2);
        assert_eq!(m.session(&history), Tokens::estimated(108));
        assert!(m.session(&history).label().ends_with('~'));
    }

    #[test]
    fn current_request_estimates_only_the_user_message() {
        let mut m = TokenMeter::new();
        m.record(400, &usage(100, 50));
        let query = Shape::new(40, 1);
        assert_eq!(m.current_request(&query), Tokens::estimated(14));
    }

    #[test]
    fn context_used_adds_the_reply_to_the_prompt_that_produced_it() {
        let mut m = TokenMeter::new();
        m.record(400, &usage(100, 50));
        let used = m.context_used(&Shape::new(400, 2), &Shape::default());
        assert_eq!(used, Tokens::measured(150));
        // Typing makes it an estimate again.
        let typed = m.context_used(&Shape::new(400, 2), &Shape::new(40, 1));
        assert!(typed.estimated);
        assert!(typed.value > 150);
    }

    #[test]
    fn context_label_shows_a_question_mark_when_the_window_is_unknown() {
        assert_eq!(
            context_label(Tokens::measured(15_000), Some(1_000_000)),
            "15k/1.0M (2%)"
        );
        assert_eq!(context_label(Tokens::estimated(300), None), "300~/?");
        assert_eq!(context_label(Tokens::measured(300), Some(0)), "300/?");
    }
}
