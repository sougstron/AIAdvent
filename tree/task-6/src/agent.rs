//! The agent is the conversation entity: it owns settings, the system prompt,
//! message history, and (later) injected context files. CLI and TUI talk to
//! this type, not to HTTP.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::api::{
    self, ChatMessage, ChatStream, Endpoint, Outcome, Usage,
};
use crate::config::{Res, Settings};

/// Assistant turn plus the provider metadata the app already displays.
#[derive(Clone, Debug)]
pub struct Reply {
    pub text: String,
    /// Model id the provider actually echoed.
    pub model: String,
    pub finish_reason: Option<String>,
    pub usage: Usage,
    pub reasoning: Option<String>,
    pub latency_ms: u128,
    pub truncated_by_max_chars: bool,
    pub raw: serde_json::Value,
}

impl Reply {
    pub fn from_outcome(outcome: Outcome, max_chars: Option<usize>) -> Reply {
        let (text, cut) = api::enforce_max_chars(outcome.text(), max_chars);
        Reply {
            text,
            model: outcome.model.unwrap_or_default(),
            finish_reason: outcome.finish_reason,
            usage: outcome.usage,
            reasoning: outcome.reasoning,
            latency_ms: outcome.latency_ms,
            truncated_by_max_chars: cut,
            raw: outcome.raw,
        }
    }

    pub fn truncated(&self) -> bool {
        matches!(self.finish_reason.as_deref(), Some("length"))
    }

    pub fn stopped_by_sequence(&self) -> bool {
        matches!(self.finish_reason.as_deref(), Some("stop"))
    }
}

#[derive(Clone)]
pub struct Agent {
    endpoint: Endpoint,
    settings: Settings,
    history: Vec<ChatMessage>,
    context_files: Vec<String>,
}

impl Agent {
    pub fn new(settings: Settings) -> Res<Agent> {
        let mut settings = settings;
        settings.clamp();
        Ok(Agent {
            endpoint: Endpoint::resolve()?,
            settings,
            history: Vec::new(),
            context_files: Vec::new(),
        })
    }

    #[cfg(test)]
    pub fn dummy() -> Agent {
        Agent {
            endpoint: Endpoint::dummy(),
            settings: Settings::default(),
            history: Vec::new(),
            context_files: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn dummy_with(settings: Settings) -> Agent {
        let mut settings = settings;
        settings.clamp();
        Agent {
            endpoint: Endpoint::dummy(),
            settings,
            history: Vec::new(),
            context_files: Vec::new(),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    pub fn set_history(&mut self, history: Vec<ChatMessage>) {
        self.history = history;
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.context_files.clear();
    }

    pub fn context_files(&self) -> &[String] {
        &self.context_files
    }

    /// One-shot turn: append the user message, call the provider, append the
    /// assistant reply. On transport failure the user message is rolled back.
    pub fn ask(&mut self, prompt: &str) -> Res<Reply> {
        self.history.push(ChatMessage::user(prompt));
        match self.complete(&self.history.clone()) {
            Ok(reply) => {
                self.history.push(ChatMessage::assistant(reply.text.clone()));
                Ok(reply)
            }
            Err(e) => {
                self.history.pop();
                Err(e)
            }
        }
    }

    /// Request/response for an explicit history (TUI session, `/personas`).
    /// Does not mutate the agent's own history.
    pub fn complete(&self, history: &[ChatMessage]) -> Res<Reply> {
        let outcome = self.complete_outcome(history)?;
        Ok(Reply::from_outcome(outcome, self.settings.max_chars))
    }

    fn system_for_request(&self) -> String {
        let mut system = self.settings.system_prompt.clone();
        for path in self.context_files() {
            if system.is_empty() {
                system = format!("Context file: {path}");
            } else {
                system = format!("{system}\n\nContext file: {path}");
            }
        }
        system
    }

    pub fn complete_outcome(&self, history: &[ChatMessage]) -> Res<Outcome> {
        let mut settings = self.settings.clone();
        settings.clamp();
        let schema = settings
            .json_mode
            .enabled
            .then(|| settings.json_mode.schema.clone());
        api::chat(
            &self.endpoint,
            &settings,
            &self.system_for_request(),
            history,
            schema.as_ref(),
        )
    }

    /// Streaming variant the TUI already knows how to drain. Same body as
    /// [`Self::complete`]; JSON mode stays on the blocking path in the TUI.
    pub fn stream(
        &self,
        history: &[ChatMessage],
        cancel: Option<Arc<AtomicBool>>,
    ) -> Res<ChatStream> {
        let mut settings = self.settings.clone();
        settings.clamp();
        api::chat_stream(
            &self.endpoint,
            &settings,
            &self.system_for_request(),
            history,
            None,
            cancel,
        )
    }

    /// One-off call with a replacement system prompt, no history mutation
    /// (`/json edit`, `/personas`).
    pub fn complete_with_system(&self, system: &str, history: &[ChatMessage]) -> Res<Outcome> {
        let mut settings = self.settings.clone();
        settings.clamp();
        settings.json_mode.enabled = false;
        settings.max_chars = None;
        api::chat(&self.endpoint, &settings, system, history, None)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Effort, DEFAULT_MODEL};

    #[test]
    fn new_clamps_and_starts_empty() {
        let agent = Agent::dummy_with(Settings {
            effort: Effort::None,
            temperature: Some(2.0),
            ..Settings::default()
        });
        assert_eq!(agent.settings().effort, Effort::Low);
        assert_eq!(agent.settings().temperature, Some(1.0));
        assert!(agent.history().is_empty());
        assert_eq!(agent.settings().model, DEFAULT_MODEL);
    }

    #[test]
    fn reset_clears_history_not_settings() {
        let mut agent = Agent::dummy();
        agent.set_history(vec![ChatMessage::user("hi")]);
        agent.settings_mut().temperature = Some(0.3);
        agent.reset();
        assert!(agent.history().is_empty());
        assert!(agent.context_files().is_empty());
        assert_eq!(agent.settings().temperature, Some(0.3));
    }

    #[test]
    fn settings_mut_is_the_runtime_lever() {
        let mut agent = Agent::dummy();
        agent.settings_mut().top_p = Some(0.5);
        assert!((agent.settings().top_p.unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn complete_refuses_expensive_models_before_http() {
        let agent = Agent::dummy_with(Settings {
            model: "glm-5.3".into(),
            ..Settings::default()
        });
        let err = agent
            .complete(&[ChatMessage::user("hi")])
            .unwrap_err();
        assert!(err.contains("glm-5.3-flash"));
        assert!(err.contains("expensive"));
    }

    #[test]
    fn ask_rolls_back_history_when_the_call_cannot_proceed() {
        let mut agent = Agent::dummy_with(Settings {
            model: "glm-5".into(),
            ..Settings::default()
        });
        assert!(agent.ask("hello").is_err());
        assert!(agent.history().is_empty());
    }

    #[test]
    fn reply_from_outcome_caps_chars_and_keeps_metadata() {
        let outcome = Outcome {
            content: Some("hello world".into()),
            reasoning: Some("think".into()),
            finish_reason: Some("stop".into()),
            model: Some("glm-5.3-flash".into()),
            usage: Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
                reasoning_tokens: 0,
                total_tokens: 3,
            },
            raw: serde_json::Value::Null,
            latency_ms: 9,
        };
        let reply = Reply::from_outcome(outcome, Some(5));
        assert_eq!(reply.text, "hello");
        assert!(reply.truncated_by_max_chars);
        assert_eq!(reply.model, "glm-5.3-flash");
        assert!(reply.stopped_by_sequence());
        assert_eq!(reply.usage.total_tokens, 3);
        assert!(reply.raw.is_null());
    }
}
