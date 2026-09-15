//! `--verify-billing`: show, live, which z.ai product this binary spends on.
//!
//! z.ai sells two things behind one key: the **GLM Coding Plan**
//! (flat monthly quota, consumed only by calls routed through
//! `/api/coding/paas/v4` or the Anthropic-compatible `/api/anthropic`) and the
//! **pay-per-token API** (metered against the prepaid platform balance,
//! served at the plain `/api/paas/v4`). The same API key is accepted on all of
//! them — the *path* decides which pot pays. This check therefore proves the
//! path, not a guess about the key.
//!
//! What it can and cannot show, stated honestly:
//! * It *can* show that the only base URL this binary can reach is the plain
//!   one, and that the account accepts a metered call on it. An account with
//!   no prepaid balance answers the plain path with error `1113 Insufficient
//!   balance` even while its Coding Plan is healthy, so a 200 here means real
//!   balance-backed billing happened.
//! * It *cannot* read the account balance: z.ai publishes no balance endpoint.
//!   The USD figures below are list price × the usage the provider reported,
//!   not a statement of what the platform charged.

use crate::api::{self, ChatMessage, Endpoint, Usage, LIVE_COMPLETION_MODEL};
use crate::config::{Res, Settings};

/// Paths that would spend the Coding Plan subscription instead of tokens.
pub const PLAN_PATH_MARKERS: [&str; 2] = ["/coding/", "/anthropic"];

/// List price for `glm-5.3-flash`, USD per 1M tokens, from
/// <https://docs.z.ai/guides/overview/pricing> as read on 2026-09-11.
pub const PRICE_INPUT_PER_MTOK: f64 = 0.15;
pub const PRICE_CACHED_INPUT_PER_MTOK: f64 = 0.03;
pub const PRICE_OUTPUT_PER_MTOK: f64 = 0.50;

const PROBE_PROMPT: &str = "Reply with the single word PONG.";
const PROBE_CAP: u32 = 8;

/// Upper bound on what one exchange costs at list price: every prompt token is
/// billed as uncached (the response does not tell us the cache split), and
/// reasoning tokens are already inside `completion_tokens`.
pub fn cost_usd(u: &Usage) -> f64 {
    (u.prompt_tokens as f64) * PRICE_INPUT_PER_MTOK / 1e6
        + (u.completion_tokens as f64) * PRICE_OUTPUT_PER_MTOK / 1e6
}

/// How many output tokens it takes to move a balance by `usd`. The answer to
/// "the balance still says $5.00": a cent of flash output is 20k tokens.
pub fn output_tokens_per_usd(usd: f64) -> u64 {
    (usd / PRICE_OUTPUT_PER_MTOK * 1e6).round() as u64
}

pub fn input_tokens_per_usd(usd: f64) -> u64 {
    (usd / PRICE_INPUT_PER_MTOK * 1e6).round() as u64
}

/// True when a base URL would bill the subscription rather than tokens.
pub fn is_plan_path(base_url: &str) -> bool {
    PLAN_PATH_MARKERS.iter().any(|m| base_url.contains(m))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BillingVerdict {
    /// Plain metered path, and the account served a metered call on it.
    PayPerToken,
    /// The binary is pointed at a Coding Plan path: spend is subscription.
    CodingPlan,
    /// Plain path refused for lack of prepaid balance — nothing was metered,
    /// so this key can only spend through the plan until the balance is topped up.
    NoBalance,
    /// The call failed for an unrelated reason; billing not established.
    Unknown,
}

impl BillingVerdict {
    const fn as_str(self) -> &'static str {
        match self {
            BillingVerdict::PayPerToken => "PayPerToken",
            BillingVerdict::CodingPlan => "CodingPlan",
            BillingVerdict::NoBalance => "NoBalance",
            BillingVerdict::Unknown => "Unknown",
        }
    }
}

pub struct Report {
    pub base_url: String,
    pub plan_path: bool,
    pub verdict: BillingVerdict,
    pub model: String,
    pub usage: Usage,
    pub cost_usd: f64,
    pub error: Option<String>,
}

impl Report {
    pub fn confirmed(&self) -> bool {
        self.verdict == BillingVerdict::PayPerToken
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("billing check (z.ai)\n");
        s.push_str(&format!("  base URL          {}\n", self.base_url));
        s.push_str(&format!(
            "  plan path?        {} (markers: {})\n",
            if self.plan_path { "YES" } else { "no" },
            PLAN_PATH_MARKERS.join(", ")
        ));
        s.push_str(&format!("  verdict           {}\n", self.verdict.as_str()));
        if let Some(e) = &self.error {
            s.push_str(&format!("  error             {e}\n"));
        } else {
            s.push_str(&format!(
                "  live probe        model={} prompt={} completion={} total={}\n",
                self.model,
                self.usage.prompt_tokens,
                self.usage.completion_tokens,
                self.usage.total_tokens
            ));
            s.push_str(&format!(
                "  probe cost        ${:.6} at list price ({} in / {} cached / {} out per 1M)\n",
                self.cost_usd,
                PRICE_INPUT_PER_MTOK,
                PRICE_CACHED_INPUT_PER_MTOK,
                PRICE_OUTPUT_PER_MTOK
            ));
        }
        s.push_str(&format!(
            "  $0.01 costs       {} output tokens, or {} input tokens\n",
            output_tokens_per_usd(0.01),
            input_tokens_per_usd(0.01)
        ));
        s.push_str(match self.verdict {
            BillingVerdict::PayPerToken => {
                "  → Metered API. Coding Plan quota is only spendable on /api/coding/paas/v4\n\
                 \x20   or /api/anthropic, which this binary never builds a URL for. This path\n\
                 \x20   answers 1113 (\"insufficient balance or no resource package\") when there\n\
                 \x20   is nothing metered to draw on; it answered 200, so the call was metered.\n\
                 \x20   Two reasons a cash balance still reads unchanged: free/trial resource\n\
                 \x20   packages are drawn down before cash, and at these rates a cent is 20k\n\
                 \x20   output tokens. Read the platform's usage log and resource-package page,\n\
                 \x20   not the rounded balance.\n"
            }
            BillingVerdict::CodingPlan => {
                "  → Subscription. This base URL spends Coding Plan quota, not tokens.\n"
            }
            BillingVerdict::NoBalance => {
                "  → Not metered: the account has no prepaid API balance, so nothing can be\n\
                 \x20   charged per token. Top up on the z.ai platform to bill outside the plan.\n"
            }
            BillingVerdict::Unknown => {
                "  → Inconclusive: the probe failed before billing could be established.\n"
            }
        });
        s
    }
}

fn probe_settings() -> Settings {
    Settings {
        budget_tokens: Some(PROBE_CAP),
        effort: crate::config::Effort::None,
        ..Settings::default()
    }
}

pub fn run() -> Res<Report> {
    let ep = Endpoint::resolve()?;
    let base_url = ep.base_url.clone();
    let plan_path = is_plan_path(&base_url);

    let settings = probe_settings();
    let history = [ChatMessage::user(PROBE_PROMPT)];
    let outcome = api::chat(&ep, &settings, "", &history, None);

    let (verdict, model, usage, error) = match outcome {
        Ok(o) => {
            let verdict = if plan_path {
                BillingVerdict::CodingPlan
            } else {
                BillingVerdict::PayPerToken
            };
            (
                verdict,
                o.model
                    .clone()
                    .unwrap_or_else(|| LIVE_COMPLETION_MODEL.into()),
                o.usage,
                None,
            )
        }
        Err(e) => {
            let verdict = if mentions_insufficient_balance(&e) {
                BillingVerdict::NoBalance
            } else {
                BillingVerdict::Unknown
            };
            (verdict, String::new(), Usage::default(), Some(e))
        }
    };

    let cost_usd = cost_usd(&usage);
    Ok(Report {
        base_url,
        plan_path,
        verdict,
        model,
        usage,
        cost_usd,
        error,
    })
}

/// z.ai reports an empty prepaid balance as error `1113`, wording varies.
fn mentions_insufficient_balance(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    low.contains("1113") || low.contains("insufficient balance") || low.contains("balance")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::DEFAULT_BASE_URL;

    #[test]
    fn default_base_url_is_not_a_plan_path() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.z.ai/api/paas/v4");
        assert!(!is_plan_path(DEFAULT_BASE_URL));
    }

    #[test]
    fn plan_paths_are_recognised() {
        assert!(is_plan_path("https://api.z.ai/api/coding/paas/v4"));
        assert!(is_plan_path("https://api.z.ai/api/anthropic"));
    }

    #[test]
    fn cost_uses_published_flash_prices() {
        let u = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            reasoning_tokens: 0,
            total_tokens: 2_000_000,
        };
        let c = cost_usd(&u);
        assert!(
            (c - (PRICE_INPUT_PER_MTOK + PRICE_OUTPUT_PER_MTOK)).abs() < 1e-9,
            "{c}"
        );
    }

    #[test]
    fn a_cent_of_output_is_twenty_thousand_tokens() {
        assert_eq!(output_tokens_per_usd(0.01), 20_000);
        assert_eq!(input_tokens_per_usd(0.01), 66_667);
    }

    #[test]
    fn balance_error_is_classified_not_unknown() {
        assert!(mentions_insufficient_balance(
            "HTTP 429 from https://api.z.ai/api/paas/v4/chat/completions: [1113] Insufficient balance"
        ));
        assert!(!mentions_insufficient_balance("[1302] Rate limit reached"));
    }

    #[test]
    fn plan_verdict_renders_as_not_confirmed() {
        let r = Report {
            base_url: "https://api.z.ai/api/coding/paas/v4".into(),
            plan_path: true,
            verdict: BillingVerdict::CodingPlan,
            model: LIVE_COMPLETION_MODEL.into(),
            usage: Usage::default(),
            cost_usd: 0.0,
            error: None,
        };
        assert!(!r.confirmed());
        assert!(r.render().contains("Subscription"));
    }
}
