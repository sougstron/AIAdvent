//! The box: one agent + one session + its own policies, and a runtime that
//! holds many of them inside a single process.
//!
//! `agent.rs` already encapsulates *a* conversation (settings, history,
//! AGENTS.md, transport). What it does not do is scale: the CLI and the TUI
//! each own exactly one `Agent`, so "100 agents in one app instance" had no
//! home. This module is that home.
//!
//! Shape:
//!
//! ```text
//! Runtime  (one per process — resolves the API key once)
//!  ├─ AgentBox "alpha"  = Agent + Session + InputPolicy + OutputPolicy + Judge?
//!  ├─ AgentBox "beta"   = ...
//!  └─ ...
//! ```
//!
//! Isolation rule, and the reason the box exists: **the session is the only
//! memory**. A box drives `Agent::complete`, which does not touch the agent's
//! own history, so what the model sees on turn N is exactly what that box's
//! session holds — nothing from any sibling box. Dropping a box and resuming
//! its id from disk restores that memory; a different id never sees it.
//! `isolation.rs` proves this live rather than by assertion.
//!
//! The module's API is deliberately wider than today's callers use: the CLI
//! drives a single box and the TUI still talks to `Agent` directly (wiring it
//! onto `Runtime` is the next step, not this one). Everything here is
//! exercised by the tests at the bottom of the file and by `isolation.rs`, so
//! dead-code warnings would only be noise about front-ends not yet written.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::agent::{Agent, Reply};
use crate::api::{ChatMessage, Endpoint};
use crate::config::{Res, Settings};
use crate::session::{self, Session, SessionSummary};

// ---------------------------------------------------------------- policies

/// What a box refuses to *send*. Checked before any network call, so a
/// rejection costs nothing.
#[derive(Clone, Debug, Default)]
pub struct InputPolicy {
    /// Reject prompts longer than this many characters.
    pub max_chars: Option<usize>,
    /// Reject a prompt that is empty or whitespace-only.
    pub allow_empty: bool,
    /// Reject a prompt containing any of these (case-insensitive).
    pub deny: Vec<String>,
    /// Prepended to every accepted prompt — a per-box instruction that is
    /// part of the turn rather than of the system message.
    pub prefix: Option<String>,
}



#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputVerdict {
    /// Prompt passed; the payload is what will actually be sent.
    Allow,
    Reject(String),
}

impl InputVerdict {
    pub fn rejected(&self) -> Option<&str> {
        match self {
            InputVerdict::Allow => None,
            InputVerdict::Reject(why) => Some(why),
        }
    }
}

impl InputPolicy {
    /// Returns the prompt to send, or why it will not be sent.
    pub fn apply(&self, prompt: &str) -> (InputVerdict, String) {
        let trimmed = prompt.trim();
        if trimmed.is_empty() && !self.allow_empty {
            return (
                InputVerdict::Reject("empty prompt".into()),
                String::new(),
            );
        }
        let count = trimmed.chars().count();
        if let Some(max) = self.max_chars {
            if count > max {
                return (
                    InputVerdict::Reject(format!(
                        "prompt is {count} chars, input policy allows {max}"
                    )),
                    String::new(),
                );
            }
        }
        let lowered = trimmed.to_lowercase();
        for needle in &self.deny {
            let needle = needle.trim();
            if needle.is_empty() {
                continue;
            }
            if lowered.contains(&needle.to_lowercase()) {
                return (
                    InputVerdict::Reject(format!("prompt matches denied phrase `{needle}`")),
                    String::new(),
                );
            }
        }
        let sent = match &self.prefix {
            Some(p) if !p.trim().is_empty() => format!("{}\n\n{trimmed}", p.trim()),
            _ => trimmed.to_string(),
        };
        (InputVerdict::Allow, sent)
    }
}

/// What a box refuses to *hand back*. Applied to the model's answer before it
/// is allowed into the session.
#[derive(Clone, Debug)]
pub struct OutputPolicy {
    /// Hard character cap on the answer. Over-long answers are truncated,
    /// not rejected (this mirrors `Settings::max_chars`).
    pub max_chars: Option<usize>,
    /// Reject an answer that is empty after trimming.
    pub require_nonempty: bool,
    /// Reject an answer containing any of these (case-insensitive).
    pub deny: Vec<String>,
    /// Reject an answer that does not parse as a single JSON value.
    pub require_json: bool,
}

impl Default for OutputPolicy {
    fn default() -> Self {
        OutputPolicy {
            max_chars: None,
            require_nonempty: true,
            deny: Vec::new(),
            require_json: false,
        }
    }
}

impl OutputPolicy {
    /// Default output policy for a set of settings: inherit the character cap
    /// and the JSON requirement the user already configured.
    pub fn from_settings(settings: &Settings) -> OutputPolicy {
        OutputPolicy {
            max_chars: settings.max_chars,
            require_nonempty: true,
            deny: Vec::new(),
            require_json: settings.json_mode.enabled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputVerdict {
    Accept,
    /// Accepted after cutting to the cap; payload is the original length.
    Truncated(usize),
    Reject(String),
}

impl OutputVerdict {
    pub fn accepted(&self) -> bool {
        !matches!(self, OutputVerdict::Reject(_))
    }

    pub fn rejected(&self) -> Option<&str> {
        match self {
            OutputVerdict::Reject(why) => Some(why),
            _ => None,
        }
    }
}

impl OutputPolicy {
    /// Returns the text to keep and the verdict on it.
    pub fn apply(&self, text: &str) -> (OutputVerdict, String) {
        let trimmed = text.trim();
        if self.require_nonempty && trimmed.is_empty() {
            return (
                OutputVerdict::Reject("model returned no content".into()),
                String::new(),
            );
        }
        let lowered = trimmed.to_lowercase();
        for needle in &self.deny {
            let needle = needle.trim();
            if needle.is_empty() {
                continue;
            }
            if lowered.contains(&needle.to_lowercase()) {
                return (
                    OutputVerdict::Reject(format!("answer matches denied phrase `{needle}`")),
                    String::new(),
                );
            }
        }
        if self.require_json && serde_json::from_str::<serde_json::Value>(trimmed).is_err() {
            return (
                OutputVerdict::Reject("answer is not valid JSON".into()),
                String::new(),
            );
        }
        let original = trimmed.chars().count();
        if let Some(max) = self.max_chars {
            if original > max {
                let cut: String = trimmed.chars().take(max).collect();
                return (OutputVerdict::Truncated(original), cut);
            }
        }
        (OutputVerdict::Accept, trimmed.to_string())
    }
}

// ------------------------------------------------------------------- judge

#[derive(Clone, Debug, PartialEq)]
pub struct Judgement {
    /// 0..=10.
    pub score: u8,
    pub pass: bool,
    pub note: String,
}

/// A box's own quality gate. `Send` so boxes can run on their own threads.
pub trait Judge: Send {
    fn name(&self) -> &str;
    fn judge(&self, prompt: &str, answer: &str) -> Res<Judgement>;
}

/// Offline, deterministic judge. Cheap enough to leave on: it only catches
/// the failure modes that need no model — a stub answer, or one that just
/// parrots the question back.
#[derive(Clone, Debug)]
pub struct RuleJudge {
    pub min_chars: usize,
    pub min_score: u8,
}

impl Default for RuleJudge {
    fn default() -> Self {
        RuleJudge {
            min_chars: 1,
            min_score: 5,
        }
    }
}

impl Judge for RuleJudge {
    fn name(&self) -> &str {
        "rule"
    }

    fn judge(&self, prompt: &str, answer: &str) -> Res<Judgement> {
        let answer = answer.trim();
        let prompt = prompt.trim();
        if answer.is_empty() {
            return Ok(Judgement {
                score: 0,
                pass: false,
                note: "empty answer".into(),
            });
        }
        let len = answer.chars().count();
        if len < self.min_chars {
            return Ok(Judgement {
                score: 2,
                pass: false,
                note: format!("answer is {len} chars, judge wants at least {}", self.min_chars),
            });
        }
        if !prompt.is_empty() && answer.eq_ignore_ascii_case(prompt) {
            return Ok(Judgement {
                score: 1,
                pass: false,
                note: "answer only echoes the prompt".into(),
            });
        }
        let score = 8;
        Ok(Judgement {
            score,
            pass: score >= self.min_score,
            note: format!("{len} chars, not an echo"),
        })
    }
}

/// Model-backed judge: a second agent, with its own settings and rubric,
/// scores the answer 0..=10. Its own conversation is stateless — one call per
/// judgement, no history — so it cannot leak one box's turns into another's.
pub struct ModelJudge {
    agent: Agent,
    rubric: String,
    min_score: u8,
}

impl ModelJudge {
    pub fn new(endpoint: Endpoint, rubric: impl Into<String>, min_score: u8) -> ModelJudge {
        let settings = Settings {
            context_enabled: false,
            max_chars: Some(200),
            system_prompt: String::new(),
            ..Settings::default()
        };
        ModelJudge {
            agent: Agent::with_endpoint(endpoint, settings),
            rubric: rubric.into(),
            min_score,
        }
    }

    fn system(&self) -> String {
        format!(
            "You grade one answer against a rubric. Rubric: {}\n\
             Reply with a single integer from 0 to 10 and nothing else. \
             10 means the answer fully satisfies the rubric, 0 means it does not at all.",
            self.rubric.trim()
        )
    }
}

impl Judge for ModelJudge {
    fn name(&self) -> &str {
        "model"
    }

    fn judge(&self, prompt: &str, answer: &str) -> Res<Judgement> {
        let ask = format!("Question:\n{prompt}\n\nAnswer:\n{answer}\n\nScore (0-10):");
        let outcome = self
            .agent
            .complete_with_system(&self.system(), &[ChatMessage::user(ask)])?;
        let text = outcome.text().to_string();
        let score = first_integer(&text).ok_or_else(|| {
            format!("judge did not return a score (got `{}`)", clip(&text, 60))
        })?;
        let score = score.min(10) as u8;
        Ok(Judgement {
            score,
            pass: score >= self.min_score,
            note: format!("model score {score}/10 (min {})", self.min_score),
        })
    }
}

fn first_integer(s: &str) -> Option<u32> {
    let mut digits = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if !digits.is_empty() {
            break;
        }
    }
    digits.parse().ok()
}

fn clip(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

// --------------------------------------------------------------------- box

/// Everything needed to stand up one box.
pub struct BoxSpec {
    /// Human label. The box id is the session id, assigned on spawn.
    pub label: String,
    pub settings: Settings,
    pub input: InputPolicy,
    pub output: OutputPolicy,
    pub judge: Option<Box<dyn Judge>>,
}

impl BoxSpec {
    pub fn new(label: impl Into<String>, settings: Settings) -> BoxSpec {
        let output = OutputPolicy::from_settings(&settings);
        BoxSpec {
            label: label.into(),
            settings,
            input: InputPolicy::default(),
            output,
            judge: None,
        }
    }

    pub fn with_input(mut self, input: InputPolicy) -> BoxSpec {
        self.input = input;
        self
    }

    pub fn with_output(mut self, output: OutputPolicy) -> BoxSpec {
        self.output = output;
        self
    }

    pub fn with_judge(mut self, judge: Box<dyn Judge>) -> BoxSpec {
        self.judge = Some(judge);
        self
    }
}

/// One completed pass through the box. A turn that never reached the network
/// (input rejected) and a turn whose answer was refused (output or judge) are
/// both `accepted == false`, and neither one enters the session history: the
/// box's memory holds only what its policies let through.
#[derive(Clone, Debug)]
pub struct Turn {
    pub box_id: String,
    /// The prompt as actually sent, after the input policy's prefix.
    pub sent: String,
    pub input: InputVerdict,
    pub output: Option<OutputVerdict>,
    pub judgement: Option<Judgement>,
    /// Answer kept after the output policy. Empty when nothing was accepted.
    pub text: String,
    pub reply: Option<Reply>,
}

impl Turn {
    pub fn accepted(&self) -> bool {
        self.input == InputVerdict::Allow
            && self.output.as_ref().is_some_and(OutputVerdict::accepted)
            && self.judgement.as_ref().is_none_or(|j| j.pass)
    }

    /// Why the turn produced nothing, if it did not.
    pub fn refusal(&self) -> Option<String> {
        if let Some(why) = self.input.rejected() {
            return Some(format!("input policy: {why}"));
        }
        if let Some(why) = self.output.as_ref().and_then(OutputVerdict::rejected) {
            return Some(format!("output policy: {why}"));
        }
        match &self.judgement {
            Some(j) if !j.pass => Some(format!("judge: {} ({})", j.note, j.score)),
            _ => None,
        }
    }
}

/// One isolated conversation: an agent, the session that *is* its memory, and
/// the policies that guard both ends of a turn.
pub struct AgentBox {
    id: String,
    label: String,
    agent: Agent,
    session: Session,
    input: InputPolicy,
    output: OutputPolicy,
    judge: Option<Box<dyn Judge>>,
    turns: usize,
    refused: usize,
}

impl AgentBox {
    fn from_spec(endpoint: Endpoint, spec: BoxSpec) -> AgentBox {
        let agent = Agent::with_endpoint(endpoint, spec.settings.clone());
        let mut session = Session::new(agent.settings().clone());
        if !spec.label.is_empty() {
            session.title = spec.label.clone();
        }
        AgentBox {
            id: session.id.clone(),
            label: spec.label,
            agent,
            session,
            input: spec.input,
            output: spec.output,
            judge: spec.judge,
            turns: 0,
            refused: 0,
        }
    }

    fn from_session(endpoint: Endpoint, session: Session, spec: BoxSpec) -> AgentBox {
        let mut agent = Agent::with_endpoint(endpoint, session.settings.clone());
        agent.resume(&session);
        // The session is the memory; the agent's own history stays empty so
        // there is exactly one place a turn can come from.
        agent.reset();
        AgentBox {
            id: session.id.clone(),
            label: spec.label,
            input: spec.input,
            output: spec.output,
            judge: spec.judge,
            turns: session.messages.iter().filter(|m| m.role == "user").count(),
            refused: 0,
            agent,
            session,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// A pinned label wins; an unlabelled box falls back to the session title,
    /// which `Session::push_user` derives from the first question.
    pub fn label(&self) -> &str {
        if self.label.is_empty() {
            &self.session.title
        } else {
            &self.label
        }
    }

    /// `Session::push_user` retitles a session from its first message. That is
    /// the right default for an unlabelled chat and wrong for a named box, so
    /// a pinned label is reapplied after every push.
    fn pin_title(&mut self) {
        if !self.label.is_empty() {
            self.session.title = self.label.clone();
        }
    }

    pub fn settings(&self) -> &Settings {
        self.agent.settings()
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        self.agent.settings_mut()
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// This box's memory: exactly the messages the model will be shown next
    /// turn.
    pub fn history(&self) -> Vec<ChatMessage> {
        self.session.history()
    }

    pub fn turns(&self) -> usize {
        self.turns
    }

    pub fn refused(&self) -> usize {
        self.refused
    }

    pub fn judge_name(&self) -> Option<&str> {
        self.judge.as_deref().map(Judge::name)
    }

    pub fn context_files(&self) -> &[crate::context::LoadedFile] {
        self.agent.context_files()
    }

    pub fn input_policy(&self) -> &InputPolicy {
        &self.input
    }

    pub fn output_policy(&self) -> &OutputPolicy {
        &self.output
    }

    pub fn set_judge(&mut self, judge: Option<Box<dyn Judge>>) {
        self.judge = judge;
    }

    /// Erase this box's memory without touching its settings or policies.
    pub fn clear(&mut self) {
        self.session.messages.clear();
        self.agent.reset();
        self.turns = 0;
    }

    /// One turn: input policy → model → output policy → judge → memory.
    /// `Err` means the transport failed; a policy refusal is an `Ok` turn
    /// with `accepted() == false`.
    pub fn ask(&mut self, prompt: &str) -> Res<Turn> {
        let (verdict, sent) = self.input.apply(prompt);
        if verdict != InputVerdict::Allow {
            self.refused += 1;
            return Ok(Turn {
                box_id: self.id.clone(),
                sent,
                input: verdict,
                output: None,
                judgement: None,
                text: String::new(),
                reply: None,
            });
        }

        let mut history = self.session.history();
        history.push(ChatMessage::user(sent.clone()));
        let reply = self.agent.complete(&history)?;

        let (out_verdict, text) = self.output.apply(&reply.text);
        let judgement = match (&self.judge, out_verdict.accepted()) {
            (Some(judge), true) => Some(judge.judge(&sent, &text)?),
            _ => None,
        };
        let turn = Turn {
            box_id: self.id.clone(),
            sent: sent.clone(),
            input: verdict,
            output: Some(out_verdict),
            judgement,
            text,
            reply: Some(reply),
        };

        if turn.accepted() {
            self.session.push_user(sent);
            self.session.push_assistant(turn.text.clone());
            self.session.settings = self.agent.settings().clone();
            self.pin_title();
            self.turns += 1;
        } else {
            self.refused += 1;
        }
        Ok(turn)
    }

    /// Append a turn without calling the provider. Used by tests and by the
    /// offline half of the isolation proof.
    pub fn seed(&mut self, user: &str, assistant: &str) {
        self.session.push_user(user.to_string());
        self.session.push_assistant(assistant.to_string());
        self.pin_title();
        self.turns += 1;
    }

    pub fn save(&mut self, dir: &Path) -> Res<()> {
        let history = self.session.history();
        self.session.capture_from(
            self.agent.settings(),
            self.agent
                .context_files()
                .iter()
                .map(|f| f.path.to_string_lossy().into_owned()),
            &history,
        );
        self.session.save(dir)
    }
}

// ----------------------------------------------------------------- runtime

/// The app instance: many boxes, one resolved endpoint, one sessions
/// directory. Spawning a box does no I/O beyond reading AGENTS.md, so N boxes
/// cost N clones of a `String` key rather than N key lookups.
pub struct Runtime {
    endpoint: Endpoint,
    dir: PathBuf,
    boxes: BTreeMap<String, AgentBox>,
    active: Option<String>,
}

impl Runtime {
    /// Resolves the API key once for the whole process.
    pub fn new() -> Res<Runtime> {
        Ok(Runtime::with_endpoint(
            Endpoint::resolve()?,
            session::sessions_dir(),
        ))
    }

    /// Like `new`, but the process endpoint follows a specific model id, so
    /// `--model deepseek-chat` actually reaches DeepSeek instead of posting
    /// a deepseek model id at the glm endpoint.
    pub fn for_model(model: &str, dir: PathBuf) -> Res<Runtime> {
        Ok(Runtime::with_endpoint(Endpoint::for_model(model)?, dir))
    }

    pub fn with_endpoint(endpoint: Endpoint, dir: PathBuf) -> Runtime {
        Runtime {
            endpoint,
            dir,
            boxes: BTreeMap::new(),
            active: None,
        }
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.dir
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    pub fn ids(&self) -> Vec<String> {
        self.boxes.keys().cloned().collect()
    }

    /// Stand up a new box. Returns its id, which is also its session id.
    pub fn spawn(&mut self, spec: BoxSpec) -> String {
        let b = AgentBox::from_spec(self.endpoint.clone(), spec);
        let id = b.id.clone();
        self.boxes.insert(id.clone(), b);
        if self.active.is_none() {
            self.active = Some(id.clone());
        }
        id
    }

    /// Load a saved session back into a live box. Its memory comes back with
    /// it; a box spawned fresh never sees those turns.
    pub fn resume(&mut self, id: &str, spec: BoxSpec) -> Res<String> {
        if self.boxes.contains_key(id) {
            return Ok(id.to_string());
        }
        let saved = session::load_session(&self.dir, id)?;
        let b = AgentBox::from_session(self.endpoint.clone(), saved, spec);
        let id = b.id.clone();
        self.boxes.insert(id.clone(), b);
        if self.active.is_none() {
            self.active = Some(id.clone());
        }
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Option<&AgentBox> {
        self.boxes.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut AgentBox> {
        self.boxes.get_mut(id)
    }

    pub fn active_id(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Switch which box the front-end is talking to. The other boxes keep
    /// their memory; the newly active one does not gain any of it.
    pub fn switch(&mut self, id: &str) -> Res<()> {
        if !self.boxes.contains_key(id) {
            return Err(format!("no live box `{id}`"));
        }
        self.active = Some(id.to_string());
        Ok(())
    }

    pub fn active_mut(&mut self) -> Option<&mut AgentBox> {
        let id = self.active.clone()?;
        self.boxes.get_mut(&id)
    }

    /// Persist a box and drop it from the process. Its session file stays, so
    /// `resume` can bring it back.
    pub fn close(&mut self, id: &str) -> Res<()> {
        let Some(mut b) = self.boxes.remove(id) else {
            return Err(format!("no live box `{id}`"));
        };
        if self.active.as_deref() == Some(id) {
            self.active = self.boxes.keys().next().cloned();
        }
        b.save(&self.dir)
    }

    pub fn save_all(&mut self) -> Res<()> {
        let dir = self.dir.clone();
        let mut failures = Vec::new();
        for b in self.boxes.values_mut() {
            if let Err(e) = b.save(&dir) {
                failures.push(e);
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    pub fn list_saved(&self) -> Vec<SessionSummary> {
        session::list_sessions(&self.dir)
    }

    /// Run one turn on each named box, each on its own thread. This is the
    /// subagent primitive: the boxes share nothing but the endpoint, and the
    /// transport (`ureq`) is blocking and thread-safe, so N boxes make N
    /// concurrent requests. Unknown ids come back as errors in place.
    pub fn ask_many(&mut self, requests: &[(String, String)]) -> Vec<(String, Res<Turn>)> {
        let wanted: BTreeMap<&str, &str> = requests
            .iter()
            .map(|(id, prompt)| (id.as_str(), prompt.as_str()))
            .collect();
        let mut targets: Vec<(String, &str, &mut AgentBox)> = self
            .boxes
            .iter_mut()
            .filter_map(|(id, b)| {
                wanted
                    .get(id.as_str())
                    .map(|prompt| (id.clone(), *prompt, b))
            })
            .collect();

        let mut out: Vec<(String, Res<Turn>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = targets
                .drain(..)
                .map(|(id, prompt, b)| scope.spawn(move || (id, b.ask(prompt))))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| (String::new(), Err("box thread panicked".into())))
                })
                .collect()
        });

        let answered: BTreeSet<&str> = out.iter().map(|(id, _)| id.as_str()).collect();
        let missing: Vec<String> = wanted
            .keys()
            .filter(|id| !answered.contains(*id))
            .map(|id| id.to_string())
            .collect();
        for id in missing {
            let err = Err(format!("no live box `{id}`"));
            out.push((id, err));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_MODEL;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("ask6-runtime-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn runtime(name: &str) -> Runtime {
        Runtime::with_endpoint(Endpoint::dummy(), tmp_dir(name))
    }

    fn spec(label: &str) -> BoxSpec {
        // Context off so a box built in a test never walks the real filesystem.
        let settings = Settings {
            context_enabled: false,
            ..Settings::default()
        };
        BoxSpec::new(label, settings)
    }

    #[test]
    fn boxes_are_send_so_they_can_run_on_their_own_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<AgentBox>();
        assert_send::<Runtime>();
    }

    #[test]
    fn a_hundred_boxes_live_in_one_runtime_with_distinct_ids() {
        let mut rt = runtime("hundred");
        let ids: Vec<String> = (0..100).map(|i| rt.spawn(spec(&format!("box-{i}")))).collect();
        assert_eq!(rt.len(), 100);
        let unique: BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), 100, "box ids collided");
        assert_eq!(rt.active_id(), Some(ids[0].as_str()));
        let _ = std::fs::remove_dir_all(rt.sessions_dir());
    }

    #[test]
    fn each_box_remembers_only_its_own_turns() {
        let mut rt = runtime("isolation");
        let ids: Vec<String> = (0..100).map(|i| rt.spawn(spec(&format!("box-{i}")))).collect();
        // The `#` terminator matters: without it `secret 1` is a substring of
        // `secret 10` and the leak check reports a leak that is not there.
        for (i, id) in ids.iter().enumerate() {
            rt.get_mut(id)
                .unwrap()
                .seed(&format!("secret {i}#"), &format!("noted {i}#"));
        }
        for (i, id) in ids.iter().enumerate() {
            let history = rt.get(id).unwrap().history();
            assert_eq!(history.len(), 2);
            assert_eq!(history[0].content, format!("secret {i}#"));
            assert_eq!(history[1].content, format!("noted {i}#"));
            let joined: String = history.iter().map(|m| m.content.clone()).collect();
            for other in 0..100 {
                if other != i {
                    assert!(
                        !joined.contains(&format!("secret {other}#")),
                        "box {i} leaked box {other}"
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(rt.sessions_dir());
    }

    #[test]
    fn switching_boxes_does_not_move_memory() {
        let mut rt = runtime("switch");
        let a = rt.spawn(spec("a"));
        let b = rt.spawn(spec("b"));
        rt.get_mut(&a).unwrap().seed("mango", "ok");
        rt.switch(&b).unwrap();
        assert_eq!(rt.active_id(), Some(b.as_str()));
        assert!(rt.active_mut().unwrap().history().is_empty());
        rt.switch(&a).unwrap();
        assert_eq!(rt.active_mut().unwrap().history()[0].content, "mango");
        assert!(rt.switch("nope").is_err());
        let _ = std::fs::remove_dir_all(rt.sessions_dir());
    }

    #[test]
    fn close_persists_and_resume_restores_that_memory_only() {
        let dir = tmp_dir("resume");
        let mut rt = Runtime::with_endpoint(Endpoint::dummy(), dir.clone());
        let a = rt.spawn(spec("a"));
        let b = rt.spawn(spec("b"));
        rt.get_mut(&a).unwrap().seed("remember apricot", "apricot noted");
        rt.get_mut(&b).unwrap().seed("remember basil", "basil noted");
        rt.close(&a).unwrap();
        rt.close(&b).unwrap();
        assert_eq!(rt.len(), 0);

        let back = rt.resume(&a, spec("")).unwrap();
        assert_eq!(back, a);
        let history = rt.get(&a).unwrap().history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "remember apricot");
        assert!(!history.iter().any(|m| m.content.contains("basil")));
        assert_eq!(rt.get(&a).unwrap().turns(), 1);

        let fresh = rt.spawn(spec("fresh"));
        assert!(rt.get(&fresh).unwrap().history().is_empty());
        assert!(rt.resume("no-such-session", spec("")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resumed_box_keeps_its_saved_settings() {
        let dir = tmp_dir("resume-settings");
        let mut rt = Runtime::with_endpoint(Endpoint::dummy(), dir.clone());
        let mut s = spec("tuned");
        s.settings.temperature = Some(0.25);
        s.settings.system_prompt = "be terse".into();
        let id = rt.spawn(s);
        rt.get_mut(&id).unwrap().seed("hi", "hey");
        rt.close(&id).unwrap();

        rt.resume(&id, spec("")).unwrap();
        let b = rt.get(&id).unwrap();
        assert_eq!(b.settings().temperature, Some(0.25));
        assert_eq!(b.settings().system_prompt, "be terse");
        assert_eq!(b.settings().model, DEFAULT_MODEL);
        assert_eq!(b.label(), "tuned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_wipes_memory_but_keeps_settings_and_policies() {
        let mut rt = runtime("clear");
        let mut s = spec("c");
        s.settings.temperature = Some(0.4);
        let id = rt.spawn(s.with_input(InputPolicy {
            max_chars: Some(50),
            ..InputPolicy::default()
        }));
        let b = rt.get_mut(&id).unwrap();
        b.seed("one", "two");
        b.clear();
        assert!(b.history().is_empty());
        assert_eq!(b.turns(), 0);
        assert_eq!(b.settings().temperature, Some(0.4));
        assert_eq!(b.input_policy().max_chars, Some(50));
        let _ = std::fs::remove_dir_all(rt.sessions_dir());
    }

    #[test]
    fn input_policy_rejects_before_the_network_and_leaves_no_trace() {
        let mut rt = runtime("input-policy");
        let id = rt.spawn(spec("p").with_input(InputPolicy {
            max_chars: Some(10),
            allow_empty: false,
            deny: vec!["forbidden".into()],
            prefix: None,
        }));
        let b = rt.get_mut(&id).unwrap();

        // A dummy endpoint would fail the request; these never get there.
        let empty = b.ask("   ").unwrap();
        assert!(!empty.accepted());
        assert_eq!(empty.refusal().as_deref(), Some("input policy: empty prompt"));

        let long = b.ask("this prompt is far too long").unwrap();
        assert!(long.refusal().unwrap().contains("input policy allows 10"));

        let denied = b.ask("FORBIDDEN").unwrap();
        assert!(denied.refusal().unwrap().contains("denied phrase"));

        assert!(b.history().is_empty());
        assert_eq!(b.turns(), 0);
        assert_eq!(b.refused(), 3);
        let _ = std::fs::remove_dir_all(rt.sessions_dir());
    }

    #[test]
    fn input_prefix_is_what_gets_sent() {
        let policy = InputPolicy {
            prefix: Some("Answer in one word.".into()),
            ..InputPolicy::default()
        };
        let (verdict, sent) = policy.apply("  capital of France  ");
        assert_eq!(verdict, InputVerdict::Allow);
        assert_eq!(sent, "Answer in one word.\n\ncapital of France");
    }

    #[test]
    fn output_policy_truncates_rejects_and_checks_json() {
        let capped = OutputPolicy {
            max_chars: Some(5),
            ..OutputPolicy::default()
        };
        let (verdict, text) = capped.apply("hello world");
        assert_eq!(verdict, OutputVerdict::Truncated(11));
        assert_eq!(text, "hello");

        let empty = OutputPolicy::default().apply("   ");
        assert!(empty.0.rejected().is_some());

        let denied = OutputPolicy {
            deny: vec!["as an ai".into()],
            ..OutputPolicy::default()
        };
        assert!(denied.apply("As an AI, I cannot").0.rejected().is_some());

        let json = OutputPolicy {
            require_json: true,
            ..OutputPolicy::default()
        };
        assert_eq!(json.apply("{\"a\":1}").0, OutputVerdict::Accept);
        assert!(json.apply("nope").0.rejected().is_some());
    }

    #[test]
    fn output_policy_defaults_follow_the_settings() {
        let mut settings = Settings {
            max_chars: Some(42),
            ..Settings::default()
        };
        settings.json_mode.enabled = true;
        let policy = OutputPolicy::from_settings(&settings);
        assert_eq!(policy.max_chars, Some(42));
        assert!(policy.require_json);
    }

    #[test]
    fn rule_judge_fails_empty_and_echoed_answers() {
        let judge = RuleJudge::default();
        assert!(!judge.judge("q", "").unwrap().pass);
        assert!(!judge.judge("what is 2+2", "What is 2+2").unwrap().pass);
        let good = judge.judge("what is 2+2", "4").unwrap();
        assert!(good.pass);
        assert!(good.score >= 5);
        assert_eq!(judge.name(), "rule");
    }

    #[test]
    fn model_judge_parses_the_first_integer() {
        assert_eq!(first_integer("8"), Some(8));
        assert_eq!(first_integer("Score: 10/10"), Some(10));
        assert_eq!(first_integer("  7 out of ten"), Some(7));
        assert_eq!(first_integer("no number here"), None);
    }

    #[test]
    fn ask_many_reports_unknown_ids_in_place() {
        let mut rt = runtime("ask-many");
        // Every prompt is refused by the input policy, so this exercises the
        // fan-out without a network call.
        let id = rt.spawn(spec("only").with_input(InputPolicy {
            deny: vec!["skip".into()],
            ..InputPolicy::default()
        }));
        let results = rt.ask_many(&[
            (id.clone(), "please skip me".into()),
            ("ghost".into(), "hello".into()),
        ]);
        assert_eq!(results.len(), 2);
        let by_id: BTreeMap<&str, &Res<Turn>> =
            results.iter().map(|(i, r)| (i.as_str(), r)).collect();
        assert!(!by_id[id.as_str()].as_ref().unwrap().accepted());
        assert!(by_id["ghost"].is_err());
        let _ = std::fs::remove_dir_all(rt.sessions_dir());
    }

    #[test]
    fn a_hundred_boxes_fan_out_concurrently() {
        let mut rt = runtime("fanout");
        let ids: Vec<String> = (0..100)
            .map(|i| {
                rt.spawn(spec(&format!("b{i}")).with_input(InputPolicy {
                    // Refuse locally: proves the fan-out itself, no network.
                    max_chars: Some(1),
                    ..InputPolicy::default()
                }))
            })
            .collect();
        let requests: Vec<(String, String)> = ids
            .iter()
            .map(|id| (id.clone(), "a longer prompt".to_string()))
            .collect();
        let results = rt.ask_many(&requests);
        assert_eq!(results.len(), 100);
        assert!(results.iter().all(|(_, r)| r.is_ok()));
        assert!(results
            .iter()
            .all(|(_, r)| !r.as_ref().unwrap().accepted()));
        let _ = std::fs::remove_dir_all(rt.sessions_dir());
    }

    #[test]
    fn save_all_writes_one_file_per_box() {
        let dir = tmp_dir("save-all");
        let mut rt = Runtime::with_endpoint(Endpoint::dummy(), dir.clone());
        for i in 0..25 {
            let id = rt.spawn(spec(&format!("s{i}")));
            rt.get_mut(&id).unwrap().seed(&format!("q{i}"), &format!("a{i}"));
        }
        rt.save_all().unwrap();
        assert_eq!(rt.list_saved().len(), 25);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
