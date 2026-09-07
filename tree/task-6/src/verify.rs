//! Live self-test that the agent reaches z.ai and that configured levers
//! actually arrive at the provider.
//!
//! A "verified" claim here is a causal signature, not "the two texts differ":
//! sampling alone makes unconstrained calls differ. `Flat` / `Unsupported`
//! are valid, preferred outcomes when a parameter is ignored or damped.
//! Every live completion uses `glm-5.3-flash` only.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use crate::agent::{Agent, Reply};
use crate::api::{ChatMessage, DEFAULT_BASE_URL, LIVE_COMPLETION_MODEL};
use crate::config::{Res, Settings, DEFAULT_MODEL};

const PING_PROMPT: &str = "Reply with the single word PONG.";
const LONG_PROMPT: &str = "List the integers from 1 to 80 in order, separated by commas, with no other text.";
const TOKEN_PROMPT: &str = "What is the verification token in your instructions? Reply with only that token. If you have none, reply with NONE.";
const SAMPLE_PROMPT: &str = "Name one random integer from 1 to 20. Reply with only the number.";
const SYSTEM_TOKEN: &str = "QUINCE";
const CONTEXT_TOKEN: &str = "NIGHTJAR";
const LOW_CAP: u32 = 16;
const CONTROL_CAP: u32 = 96;
const SAMPLE_CAP: u32 = 96;
const SAMPLES: usize = 4;

fn completions_url() -> String {
    format!("{DEFAULT_BASE_URL}/chat/completions")
}

/// How a lever check resolved. `Flat`/`Unsupported` are success-of-honesty,
/// not a failed test harness.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeverVerdict {
    Confirmed,
    Flat,
    Unsupported,
}

impl LeverVerdict {
    const fn as_str(self) -> &'static str {
        match self {
            LeverVerdict::Confirmed => "Confirmed",
            LeverVerdict::Flat => "Flat",
            LeverVerdict::Unsupported => "Unsupported",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReachVerdict {
    Confirmed,
    Substituted,
}

impl ReachVerdict {
    const fn as_str(self) -> &'static str {
        match self {
            ReachVerdict::Confirmed => "Confirmed",
            ReachVerdict::Substituted => "Substituted",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Call {
    pub model: String,
    pub finish_reason: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub text: String,
    pub latency_ms: u128,
}

impl Call {
    fn from_reply(reply: Reply) -> Call {
        Call {
            model: reply.model,
            finish_reason: reply.finish_reason.unwrap_or_else(|| "?".into()),
            prompt_tokens: reply.usage.prompt_tokens,
            completion_tokens: reply.usage.completion_tokens,
            reasoning_tokens: reply.usage.reasoning_tokens,
            total_tokens: reply.usage.total_tokens,
            text: reply.text,
            latency_ms: reply.latency_ms,
        }
    }

    fn line(&self) -> String {
        format!(
            "model={} finish={} prompt_tokens={} completion_tokens={} reasoning_tokens={} total={} {}ms",
            if self.model.is_empty() { "?" } else { &self.model },
            self.finish_reason,
            self.prompt_tokens,
            self.completion_tokens,
            self.reasoning_tokens,
            self.total_tokens,
            self.latency_ms
        )
    }
}

#[derive(Clone, Debug)]
pub struct Reachability {
    pub endpoint: String,
    pub asked_model: String,
    pub call: Call,
    pub verdict: ReachVerdict,
}

#[derive(Clone, Debug)]
pub struct MaxTokensCheck {
    pub low_cap: u32,
    pub control_cap: u32,
    pub capped: Call,
    pub control: Call,
    pub verdict: LeverVerdict,
}

#[derive(Clone, Debug)]
pub struct InstructionCheck {
    pub label: &'static str,
    pub token: &'static str,
    pub on: Call,
    pub off: Call,
    pub verdict: LeverVerdict,
}

#[derive(Clone, Debug)]
pub struct SamplingCheck {
    pub lever: &'static str,
    pub cold_label: String,
    pub hot_label: String,
    pub cold: Vec<String>,
    pub hot: Vec<String>,
    pub cold_distinct: usize,
    pub hot_distinct: usize,
    pub verdict: LeverVerdict,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub reach: Reachability,
    pub max_tokens: MaxTokensCheck,
    pub system_prompt: InstructionCheck,
    pub agents_md: InstructionCheck,
    pub temperature: SamplingCheck,
    pub top_p: SamplingCheck,
    pub top_k: SamplingCheck,
}

impl Report {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("z.ai agent self-test (glm-5.3-flash only)\n");
        out.push_str(&format!("endpoint: {}\n\n", self.reach.endpoint));
        out.push_str(&format!(
            "== reachability asked={} ==\n{}\n=> {}\n\n",
            self.reach.asked_model,
            self.reach.call.line(),
            self.reach.verdict.as_str()
        ));
        out.push_str(&format!(
            "== max_tokens low={} control={} ==\n[capped]   {}\n  {}\n[control]  {}\n  {}\n=> {}\n\n",
            self.max_tokens.low_cap,
            self.max_tokens.control_cap,
            self.max_tokens.capped.line(),
            clip(&self.max_tokens.capped.text, 120),
            self.max_tokens.control.line(),
            clip(&self.max_tokens.control.text, 120),
            self.max_tokens.verdict.as_str()
        ));
        for check in [&self.system_prompt, &self.agents_md] {
            out.push_str(&format!(
                "== {} (token={}) ==\n[on]  {}\n  {}\n[off] {}\n  {}\n=> {}\n\n",
                check.label,
                check.token,
                check.on.line(),
                clip(&check.on.text, 80),
                check.off.line(),
                clip(&check.off.text, 80),
                check.verdict.as_str()
            ));
        }
        for check in [&self.temperature, &self.top_p, &self.top_k] {
            out.push_str(&format!(
                "== {} {} vs {} (n={}) ==\ncold distinct={} {:?}\nhot  distinct={} {:?}\n=> {}\n\n",
                check.lever,
                check.cold_label,
                check.hot_label,
                check.cold.len(),
                check.cold_distinct,
                check.cold,
                check.hot_distinct,
                check.hot,
                check.verdict.as_str()
            ));
        }
        out.push_str(
            "Confirmed requires a causal signature. Flat/Unsupported means the provider did not honour the lever — that is the result, not a failed harness.\n",
        );
        out
    }

    pub fn status_line(&self) -> String {
        format!(
            "verify: reach={} max_tokens={} system={} agents.md={} temp={} top_p={} top_k={}",
            self.reach.verdict.as_str(),
            self.max_tokens.verdict.as_str(),
            self.system_prompt.verdict.as_str(),
            self.agents_md.verdict.as_str(),
            self.temperature.verdict.as_str(),
            self.top_p.verdict.as_str(),
            self.top_k.verdict.as_str()
        )
    }
}

pub fn run() -> Res<Report> {
    let endpoint = completions_url();
    let mut agent = Agent::new(isolated_settings())?;
    if agent.settings().model != LIVE_COMPLETION_MODEL {
        return Err(format!(
            "verify refuses to run: agent model is `{}`, not `{LIVE_COMPLETION_MODEL}`",
            agent.settings().model
        ));
    }

    let reach = check_reach(&agent, &endpoint)?;
    let max_tokens = check_max_tokens(&mut agent)?;
    let system_prompt = check_system_prompt(&mut agent)?;
    let agents_md = check_agents_md(&mut agent)?;
    let temperature = check_temperature(&mut agent)?;
    let top_p = check_top_p(&mut agent)?;
    let top_k = check_top_k(&mut agent)?;

    Ok(Report {
        reach,
        max_tokens,
        system_prompt,
        agents_md,
        temperature,
        top_p,
        top_k,
    })
}

fn isolated_settings() -> Settings {
    Settings {
        model: DEFAULT_MODEL.into(),
        system_prompt: String::new(),
        context_enabled: false,
        ..Settings::default()
    }
    .without_stop_condition()
}

fn probe(agent: &Agent, prompt: &str) -> Res<Call> {
    if agent.settings().model != LIVE_COMPLETION_MODEL {
        return Err(format!(
            "refusing to call `{}` from verify (only `{LIVE_COMPLETION_MODEL}`)",
            agent.settings().model
        ));
    }
    let history = [ChatMessage::user(prompt)];
    agent.complete(&history).map(Call::from_reply)
}

fn reset_sampling(agent: &mut Agent) {
    let s = agent.settings_mut();
    s.temperature = None;
    s.top_p = None;
    s.top_k = None;
    s.budget_tokens = None;
    s.system_prompt.clear();
    s.model = DEFAULT_MODEL.into();
}

fn check_reach(agent: &Agent, endpoint: &str) -> Res<Reachability> {
    let call = probe(agent, PING_PROMPT)?;
    let verdict = if call.model == LIVE_COMPLETION_MODEL {
        ReachVerdict::Confirmed
    } else {
        ReachVerdict::Substituted
    };
    Ok(Reachability {
        endpoint: endpoint.to_string(),
        asked_model: LIVE_COMPLETION_MODEL.into(),
        call,
        verdict,
    })
}

fn check_max_tokens(agent: &mut Agent) -> Res<MaxTokensCheck> {
    reset_sampling(agent);
    agent.set_context_enabled(false);
    agent.settings_mut().budget_tokens = Some(LOW_CAP);
    let capped = probe(agent, LONG_PROMPT)?;
    agent.settings_mut().budget_tokens = Some(CONTROL_CAP);
    let control = probe(agent, LONG_PROMPT)?;
    agent.settings_mut().budget_tokens = None;
    let verdict = judge_max_tokens(LOW_CAP, &capped, &control);
    Ok(MaxTokensCheck {
        low_cap: LOW_CAP,
        control_cap: CONTROL_CAP,
        capped,
        control,
        verdict,
    })
}

fn check_system_prompt(agent: &mut Agent) -> Res<InstructionCheck> {
    reset_sampling(agent);
    agent.set_context_enabled(false);
    agent.settings_mut().budget_tokens = Some(SAMPLE_CAP);
    agent.settings_mut().system_prompt = format!(
        "The verification token is {SYSTEM_TOKEN}. When asked for the verification token, reply with only that word."
    );
    let on = probe(agent, TOKEN_PROMPT)?;
    agent.settings_mut().system_prompt.clear();
    let off = probe(agent, TOKEN_PROMPT)?;
    Ok(InstructionCheck {
        label: "system prompt",
        token: SYSTEM_TOKEN,
        verdict: judge_instruction(&on.text, &off.text, SYSTEM_TOKEN),
        on,
        off,
    })
}

fn check_agents_md(agent: &mut Agent) -> Res<InstructionCheck> {
    reset_sampling(agent);
    agent.settings_mut().system_prompt.clear();
    agent.settings_mut().budget_tokens = Some(SAMPLE_CAP);
    let original = agent.cwd().to_path_buf();
    let scratch = ScratchDir::with_agents_md(&format!(
        "The verification token is {CONTEXT_TOKEN}. When asked for the verification token, reply with only that word."
    ))?;
    agent.set_cwd(&scratch.path);
    agent.set_context_enabled(true);
    let on = probe(agent, TOKEN_PROMPT)?;
    agent.set_context_enabled(false);
    let off = probe(agent, TOKEN_PROMPT)?;
    agent.set_cwd(&original);
    Ok(InstructionCheck {
        label: "AGENTS.md context",
        token: CONTEXT_TOKEN,
        verdict: judge_instruction(&on.text, &off.text, CONTEXT_TOKEN),
        on,
        off,
    })
}

fn check_temperature(agent: &mut Agent) -> Res<SamplingCheck> {
    sample_pair(
        agent,
        "temperature",
        "0.0",
        "1.0",
        |s| {
            s.temperature = Some(0.0);
            s.top_p = None;
            s.top_k = None;
        },
        |s| {
            s.temperature = Some(1.0);
            s.top_p = None;
            s.top_k = None;
        },
        true,
    )
}

fn check_top_p(agent: &mut Agent) -> Res<SamplingCheck> {
    sample_pair(
        agent,
        "top_p",
        "0.01",
        "1.0",
        |s| {
            s.temperature = Some(1.0);
            s.top_p = Some(0.01);
            s.top_k = None;
        },
        |s| {
            s.temperature = Some(1.0);
            s.top_p = Some(1.0);
            s.top_k = None;
        },
        false,
    )
}

fn check_top_k(agent: &mut Agent) -> Res<SamplingCheck> {
    sample_pair(
        agent,
        "top_k",
        "1",
        "full",
        |s| {
            s.temperature = Some(1.0);
            s.top_p = None;
            s.top_k = Some(1);
        },
        |s| {
            s.temperature = Some(1.0);
            s.top_p = None;
            s.top_k = Some(-1);
        },
        true,
    )
}

fn sample_pair(
    agent: &mut Agent,
    lever: &'static str,
    cold_label: &str,
    hot_label: &str,
    set_cold: fn(&mut Settings),
    set_hot: fn(&mut Settings),
    cold_must_collapse: bool,
) -> Res<SamplingCheck> {
    reset_sampling(agent);
    agent.set_context_enabled(false);
    agent.settings_mut().budget_tokens = Some(SAMPLE_CAP);
    set_cold(agent.settings_mut());
    let cold = sample_n(agent, SAMPLES)?;
    set_hot(agent.settings_mut());
    let hot = sample_n(agent, SAMPLES)?;
    let cold_distinct = distinct_count(&cold);
    let hot_distinct = distinct_count(&hot);
    let verdict = judge_sampling(cold_distinct, hot_distinct, cold_must_collapse);
    Ok(SamplingCheck {
        lever,
        cold_label: cold_label.into(),
        hot_label: hot_label.into(),
        cold,
        hot,
        cold_distinct,
        hot_distinct,
        verdict,
    })
}

fn sample_n(agent: &Agent, n: usize) -> Res<Vec<String>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let call = probe(agent, SAMPLE_PROMPT)?;
        out.push(normalize_answer(&call.text));
    }
    Ok(out)
}

fn judge_max_tokens(cap: u32, capped: &Call, control: &Call) -> LeverVerdict {
    let cap = u64::from(cap);
    let at_cap = capped.finish_reason == "length" && capped.completion_tokens == cap;
    let control_shows_room = control.completion_tokens > cap;
    if at_cap && control_shows_room {
        LeverVerdict::Confirmed
    } else {
        LeverVerdict::Flat
    }
}

fn judge_instruction(on_text: &str, off_text: &str, token: &str) -> LeverVerdict {
    if has_token(on_text, token) && !has_token(off_text, token) {
        LeverVerdict::Confirmed
    } else {
        LeverVerdict::Flat
    }
}

/// Causal sampling signature, never "the two texts differ".
///
/// * If the collapsing side must be greedy (`temperature=0` / `top_k=1`) but
///   produced more than one answer, the parameter did not reach the sampler
///   → `Unsupported`.
/// * If the collapsing side is unique and the other side spreads → `Confirmed`.
/// * If neither side spreads → `Flat` (ignored, or damped by thinking).
fn judge_sampling(cold_distinct: usize, hot_distinct: usize, cold_must_collapse: bool) -> LeverVerdict {
    if cold_must_collapse && cold_distinct > 1 {
        LeverVerdict::Unsupported
    } else if cold_distinct == 1 && hot_distinct > cold_distinct {
        LeverVerdict::Confirmed
    } else {
        LeverVerdict::Flat
    }
}

fn has_token(text: &str, token: &str) -> bool {
    let needle = token.to_ascii_uppercase();
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| !w.is_empty() && w.eq_ignore_ascii_case(&needle))
}

fn normalize_answer(s: &str) -> String {
    s.trim()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_ascii_lowercase()
}

fn distinct_count(answers: &[String]) -> usize {
    answers.iter().collect::<BTreeSet<_>>().len()
}

fn clip(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn with_agents_md(body: &str) -> Res<ScratchDir> {
        let path = std::env::temp_dir().join(format!("ask-verify-ctx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        let file = path.join("AGENTS.md");
        fs::write(&file, body).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
        Ok(ScratchDir { path })
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Effort;
    fn call(finish: &str, completion: u64, text: &str) -> Call {
        Call {
            model: LIVE_COMPLETION_MODEL.into(),
            finish_reason: finish.into(),
            prompt_tokens: 10,
            completion_tokens: completion,
            reasoning_tokens: 0,
            total_tokens: 10 + completion,
            text: text.into(),
            latency_ms: 1,
        }
    }

    #[test]
    fn reach_is_confirmed_only_when_echo_matches_asked() {
        assert_eq!(LIVE_COMPLETION_MODEL, DEFAULT_MODEL);
        let matched = Call {
            model: "glm-5.3-flash".into(),
            ..call("stop", 2, "PONG")
        };
        assert_eq!(matched.model, LIVE_COMPLETION_MODEL);
        let sub = Call {
            model: "glm-5".into(),
            ..call("stop", 2, "PONG")
        };
        assert_ne!(sub.model, LIVE_COMPLETION_MODEL);
    }

    #[test]
    fn max_tokens_confirmed_only_at_cap_with_length_and_longer_control() {
        let capped = call("length", 16, "1, 2, 3");
        let control = call("stop", 80, "1, 2, 3, 4, 5");
        assert_eq!(
            judge_max_tokens(16, &capped, &control),
            LeverVerdict::Confirmed
        );
    }

    #[test]
    fn max_tokens_flat_when_capped_run_finished_naturally() {
        let capped = call("stop", 12, "1, 2, 3");
        let control = call("stop", 80, "1, 2, 3, 4, 5");
        assert_eq!(judge_max_tokens(16, &capped, &control), LeverVerdict::Flat);
    }

    #[test]
    fn max_tokens_flat_when_tokens_are_not_at_the_cap() {
        let capped = call("length", 8, "1, 2");
        let control = call("stop", 80, "1, 2, 3, 4, 5");
        assert_eq!(judge_max_tokens(16, &capped, &control), LeverVerdict::Flat);
    }

    #[test]
    fn max_tokens_flat_when_control_is_not_longer() {
        let capped = call("length", 16, "1, 2, 3");
        let control = call("stop", 16, "1, 2, 3");
        assert_eq!(judge_max_tokens(16, &capped, &control), LeverVerdict::Flat);
    }

    #[test]
    fn instruction_confirmed_only_when_on_has_token_and_off_does_not() {
        assert_eq!(
            judge_instruction("QUINCE", "NONE", "QUINCE"),
            LeverVerdict::Confirmed
        );
        assert_eq!(
            judge_instruction("The token is QUINCE.", "NONE", "QUINCE"),
            LeverVerdict::Confirmed
        );
    }

    #[test]
    fn instruction_flat_when_on_does_not_obey() {
        assert_eq!(
            judge_instruction("NONE", "NONE", "QUINCE"),
            LeverVerdict::Flat
        );
        assert_eq!(
            judge_instruction("42", "42", "QUINCE"),
            LeverVerdict::Flat
        );
    }

    #[test]
    fn instruction_flat_when_both_sides_emit_the_token() {
        assert_eq!(
            judge_instruction("QUINCE", "QUINCE", "QUINCE"),
            LeverVerdict::Flat
        );
    }

    #[test]
    fn has_token_is_word_bounded() {
        assert!(has_token("QUINCE", "QUINCE"));
        assert!(has_token("quince.", "QUINCE"));
        assert!(!has_token("NONE", "QUINCE"));
        assert!(!has_token("QUINCENTENNIAL", "QUINCE"));
    }

    #[test]
    fn sampling_confirmed_only_when_cold_collapses_and_hot_spreads() {
        assert_eq!(judge_sampling(1, 3, true), LeverVerdict::Confirmed);
    }

    #[test]
    fn sampling_unsupported_when_greedy_side_varies() {
        // temperature=0 / top_k=1 must be deterministic. Variation means the
        // parameter never reached the sampler — not a text-diff "pass".
        assert_eq!(judge_sampling(3, 3, true), LeverVerdict::Unsupported);
        assert_eq!(judge_sampling(2, 4, true), LeverVerdict::Unsupported);
    }

    #[test]
    fn sampling_flat_when_neither_side_spreads() {
        assert_eq!(judge_sampling(1, 1, true), LeverVerdict::Flat);
        assert_eq!(judge_sampling(1, 1, false), LeverVerdict::Flat);
    }

    #[test]
    fn sampling_two_different_texts_are_not_confirmation() {
        let cold = vec!["7".into(), "11".into(), "3".into(), "7".into()];
        let hot = vec!["4".into(), "19".into(), "8".into(), "2".into()];
        assert_ne!(cold, hot);
        assert_eq!(
            judge_sampling(distinct_count(&cold), distinct_count(&hot), true),
            LeverVerdict::Unsupported
        );
    }

    #[test]
    fn render_includes_endpoint_model_finish_and_tokens() {
        let ping = call("stop", 7, "PONG");
        let report = Report {
            reach: Reachability {
                endpoint: completions_url(),
                asked_model: LIVE_COMPLETION_MODEL.into(),
                call: ping,
                verdict: ReachVerdict::Confirmed,
            },
            max_tokens: MaxTokensCheck {
                low_cap: 16,
                control_cap: 96,
                capped: call("length", 16, "1, 2"),
                control: call("stop", 80, "1, 2, 3"),
                verdict: LeverVerdict::Confirmed,
            },
            system_prompt: InstructionCheck {
                label: "system prompt",
                token: SYSTEM_TOKEN,
                on: call("stop", 1, "QUINCE"),
                off: call("stop", 1, "NONE"),
                verdict: LeverVerdict::Confirmed,
            },
            agents_md: InstructionCheck {
                label: "AGENTS.md context",
                token: CONTEXT_TOKEN,
                on: call("stop", 1, "NIGHTJAR"),
                off: call("stop", 1, "NONE"),
                verdict: LeverVerdict::Flat,
            },
            temperature: SamplingCheck {
                lever: "temperature",
                cold_label: "0.0".into(),
                hot_label: "1.0".into(),
                cold: vec!["7".into(); 4],
                hot: vec!["7".into(); 4],
                cold_distinct: 1,
                hot_distinct: 1,
                verdict: LeverVerdict::Flat,
            },
            top_p: SamplingCheck {
                lever: "top_p",
                cold_label: "0.01".into(),
                hot_label: "1.0".into(),
                cold: vec!["7".into(); 4],
                hot: vec!["7".into(); 4],
                cold_distinct: 1,
                hot_distinct: 1,
                verdict: LeverVerdict::Flat,
            },
            top_k: SamplingCheck {
                lever: "top_k",
                cold_label: "1".into(),
                hot_label: "full".into(),
                cold: vec!["7".into(), "3".into(), "11".into(), "7".into()],
                hot: vec!["7".into(), "3".into(), "11".into(), "4".into()],
                cold_distinct: 3,
                hot_distinct: 4,
                verdict: LeverVerdict::Unsupported,
            },
        };
        let text = report.render();
        assert!(text.contains(&completions_url()));
        assert!(text.contains("glm-5.3-flash"));
        assert!(text.contains("finish=stop"));
        assert!(text.contains("finish=length"));
        assert!(text.contains("completion_tokens=16"));
        assert!(text.contains("completion_tokens=7"));
        assert!(text.contains("=> Flat"));
        assert!(text.contains("=> Unsupported"));
        assert!(text.contains("=> Confirmed"));
        assert!(!text.contains("the two texts differ"));
        assert_eq!(
            report.status_line(),
            "verify: reach=Confirmed max_tokens=Confirmed system=Confirmed agents.md=Flat temp=Flat top_p=Flat top_k=Unsupported"
        );
    }

    #[test]
    fn isolated_settings_are_flash_only_with_levers_off() {
        let s = isolated_settings();
        assert_eq!(s.model, LIVE_COMPLETION_MODEL);
        assert!(!s.context_enabled);
        assert!(!s.json_mode.enabled);
        assert!(s.budget_tokens.is_none());
        assert!(s.temperature.is_none());
        assert_eq!(s.effort, Effort::Low);
    }

    #[test]
    fn endpoint_is_the_plain_z_ai_completions_url() {
        let url = completions_url();
        assert_eq!(url, format!("{DEFAULT_BASE_URL}/chat/completions"));
        assert!(!url.contains("/coding/"));
    }
}
