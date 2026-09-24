//! The agent is the conversation entity: it owns settings, the system prompt,
//! message history, and injected AGENTS.md context. CLI and TUI talk to this
//! type, not to HTTP.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::api::{self, ChatMessage, ChatStream, Endpoint, Outcome, Usage};
use crate::compress::{self, Compressor, Policy};
use crate::config::{ContextStrategy, Res, Settings};
use crate::context::{ContextBundle, LoadedFile, MAX_FILE_CHARS};
use crate::facts::{self, FactStore, FactsDelta};
use crate::memory::{self, Layer, MemoryStore};
use crate::invariants::InvariantSet;
use crate::profile::{self, Profile, ProfileSet};
use crate::todo::TaskState;
use crate::session::Session;
use crate::strategy;

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

/// Итог одной свёртки истории — что именно ушло под summary и во что это
/// обошлось. Front-end печатает это в статус, `--verify-compress` считает по
/// нему экономию.
#[derive(Clone, Debug)]
pub struct FoldReport {
    /// Сколько сообщений истории покрыто summary после свёртки.
    pub covered: usize,
    /// Сколько сообщений ушло под summary именно в этой свёртке.
    pub chunk_len: usize,
    /// Символов в свёрнутом чанке.
    pub chunk_chars: usize,
    /// Символов в получившемся summary.
    pub summary_chars: usize,
    /// Во что обошёлся сам вызов свёртки.
    pub usage: Usage,
}

#[derive(Clone)]
pub struct Agent {
    endpoint: Endpoint,
    settings: Settings,
    history: Vec<ChatMessage>,
    cwd: PathBuf,
    home: PathBuf,
    context: ContextBundle,
    /// Бегущее summary истории (см. `compress.rs`). Пустой, пока стратегия
    /// `off` или пока сворачивать нечего.
    compressor: Compressor,
    /// Key-value память диалога (см. `facts.rs`). Пустая, пока стратегия не
    /// `facts`.
    facts: FactStore,
    /// Трёхслойная память агента (см. `memory.rs`): краткосрочная, рабочая,
    /// долговременная. В отличие от фактов, живёт на диске и переживает
    /// перезапуск — поэтому её не сбрасывает `reset`.
    memory: MemoryStore,
    /// Каталог профилей пользователя (см. `profile.rs`). Какой из них
    /// активен, хранится в `settings.profile` — там же, где живут остальные
    /// настройки разговора, поэтому профиль уезжает в сессию и возвращается
    /// из неё вместе с ними.
    profiles: ProfileSet,
    /// Состояние задачи (см. `todo.rs`). Живёт рядом с настройками, а не
    /// внутри них: настройка `todo` — это «вести автомат или нет», а сам
    /// автомат — состояние разговора, и уезжает он в файл сессии.
    todo: TaskState,
    /// Project invariants are loaded from their own file, never serialized
    /// into the dialogue session.
    invariants: InvariantSet,
}

impl Agent {
    pub fn new(settings: Settings) -> Res<Agent> {
        Ok(Agent::with_endpoint(
            Endpoint::for_model(&settings.model)?,
            settings,
        ))
    }

    /// Build an agent on an already-resolved endpoint. This is the constructor
    /// the multi-agent runtime uses: resolving the key once and cloning the
    /// endpoint keeps spawning N boxes off the filesystem N times.
    pub fn with_endpoint(endpoint: Endpoint, settings: Settings) -> Agent {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        Agent::at(endpoint, cwd, home, settings)
    }

    fn at(endpoint: Endpoint, cwd: PathBuf, home: PathBuf, settings: Settings) -> Agent {
        let mut settings = settings;
        settings.clamp();
        let mut agent = Agent {
            endpoint,
            settings,
            history: Vec::new(),
            cwd,
            home,
            context: ContextBundle::empty(PathBuf::from(".")),
            compressor: Compressor::new(),
            facts: FactStore::new(),
            memory: MemoryStore::in_memory(),
            profiles: ProfileSet::in_memory(),
            todo: TaskState::new(),
            invariants: if cfg!(test) {
                InvariantSet::in_memory()
            } else {
                InvariantSet::load_default()
            },
        };
        agent.refresh_context();
        agent
    }

    #[cfg(test)]
    pub fn dummy() -> Agent {
        Agent::dummy_at(
            PathBuf::from("/nonexistent-ask-cwd"),
            PathBuf::from("/nonexistent-ask-home"),
            Settings::default(),
        )
    }

    #[cfg(test)]
    pub fn dummy_with(settings: Settings) -> Agent {
        Agent::dummy_at(
            PathBuf::from("/nonexistent-ask-cwd"),
            PathBuf::from("/nonexistent-ask-home"),
            settings,
        )
    }

    #[cfg(test)]
    pub fn dummy_at(cwd: PathBuf, home: PathBuf, settings: Settings) -> Agent {
        Agent::at(Endpoint::dummy(), cwd, home, settings)
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    pub fn invariants(&self) -> &InvariantSet {
        &self.invariants
    }


    /// Swaps the transport endpoint after the fact — the keyless TUI boot
    /// starts on `Endpoint::unusable()` and upgrades once `/login` connects
    /// a key. History, settings and context are untouched.
    pub fn set_endpoint(&mut self, endpoint: Endpoint) {
        self.endpoint = endpoint;
    }

    /// Whether the agent can reach a provider at all right now.
    pub fn has_api_key(&self) -> bool {
        self.endpoint.has_key()
    }

    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    pub fn set_history(&mut self, history: Vec<ChatMessage>) {
        self.history = history;
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.compressor.reset();
        self.facts.reset();
        // Память слоёв НЕ чистим: краткосрочный слой снимет владелец через
        // `memory_mut().set_session(...)`, а рабочий и долговременный
        // переживают новый чат — в этом их смысл.
    }

    /// Restore a saved chat: settings snapshot, message history, and the
    /// context-ingestion toggle. Instruction files are re-read from disk.
    pub fn resume(&mut self, session: &Session) {
        self.settings = session.settings.clone();
        self.settings.clamp();
        self.history = session.history();
        // Summary сохраняется вместе с сессией: без него возобновлённый чат
        // потерял бы всё, что уже было свёрнуто, а `covered` уехал бы на
        // историю, которую никто не описывал.
        self.compressor = session.compressor.clone();
        self.facts = session.facts().clone();
        self.refresh_context();
    }

    pub fn compressor(&self) -> &Compressor {
        &self.compressor
    }

    pub fn set_compressor(&mut self, compressor: Compressor) {
        self.compressor = compressor;
    }

    pub fn facts(&self) -> &FactStore {
        &self.facts
    }

    pub fn set_facts(&mut self, facts: FactStore) {
        self.facts = facts;
    }

    pub fn memory(&self) -> &MemoryStore {
        &self.memory
    }

    pub fn memory_mut(&mut self) -> &mut MemoryStore {
        &mut self.memory
    }

    /// Состояние задачи. Пустое, пока `/todo start` (или первое сообщение
    /// при включённой тудушке) его не завело.
    pub fn todo(&self) -> &TaskState {
        &self.todo
    }


    pub fn set_todo(&mut self, state: TaskState) {
        self.todo = state;
    }

    /// Уезжает ли блок состояния в `system`: настройка включена И задача
    /// заведена. Выключенная тудушка не стоит ни одного токена.
    pub fn todo_on_wire(&self) -> bool {
        self.settings.todo && self.todo.active()
    }

    pub fn profiles(&self) -> &ProfileSet {
        &self.profiles
    }

    /// Активный профиль. Неизвестный id — `None`: чужой голос молча не
    /// подставляется (см. `ProfileSet::active`).
    pub fn profile(&self) -> Option<&Profile> {
        self.profiles.get(&self.settings.profile)
    }

    /// Выбрать профиль по имени (`off` выключает). Запоминается и в
    /// настройках, и в файле каталога.
    pub fn set_profile(&mut self, id: &str) -> Res<()> {
        self.profiles.set_active(id)?;
        self.settings.profile = self.profiles.active_id().to_string();
        Ok(())
    }

    /// Подцепить каталог с диска. Если в настройках профиль ещё не выбран, а
    /// файл помнит выбор — он и применяется: персонализация должна пережить
    /// перезапуск, иначе это не профиль, а настроение сессии.
    pub fn set_profiles(&mut self, profiles: ProfileSet) {
        self.profiles = profiles;
        if self.settings.profile.is_empty() || self.settings.profile == profile::OFF {
            self.settings.profile = self.profiles.active_id().to_string();
        } else {
            // Выбор из настроек (сессия, `--profile`) сильнее файла, но
            // файл он не переписывает: это выбор на этот запуск.
            self.profiles.adopt_active(&self.settings.profile.clone());
        }
    }

    /// Что долговременная память знает про пользователя: записи с префиксом
    /// `профиль.` / `profile.`. Это вторая половина блока персонализации —
    /// то, что агент запомнил сам, а не то, что человек выбрал руками.
    pub fn known_about_user(&self) -> Vec<(String, String)> {
        self.memory
            .records(Layer::Long)
            .iter()
            .filter(|r| {
                let k = r.key.trim().to_lowercase();
                k.starts_with("профиль.") || k.starts_with("profile.")
            })
            .map(|r| (r.key.trim().to_string(), r.value.trim().to_string()))
            .collect()
    }

    pub fn set_memory(&mut self, memory: MemoryStore) {
        self.memory = memory;
    }

    /// Текущая стратегия управления контекстом.
    pub fn strategy(&self) -> ContextStrategy {
        self.settings.context_strategy
    }

    /// Переключить стратегию. Смена на `off` не выбрасывает уже накопленное
    /// summary — обратное включение продолжает с того же места.
    pub fn set_strategy(&mut self, strategy: ContextStrategy) {
        self.settings.context_strategy = strategy;
    }

    /// Что реально уйдёт на провод для этой истории — решает один диспетчер
    /// (`strategy::apply`), а не сам агент.
    pub fn wire_history<'a>(&self, history: &'a [ChatMessage]) -> &'a [ChatMessage] {
        strategy::apply(
            self.settings.context_strategy,
            history,
            &self.compressor,
            self.settings.keep_recent,
        )
    }

    /// Строка состояния текущей стратегии для футера и `/strategy show`.
    pub fn strategy_status(&self, history: &[ChatMessage], branch_line: Option<&str>) -> String {
        strategy::status(
            self.settings.context_strategy,
            history,
            &self.compressor,
            &self.facts,
            &self.memory,
            self.settings.keep_recent,
            branch_line,
        )
    }

    /// Обновить key-value память по последним репликам.
    ///
    /// Запрос идёт своим вызовом с теми же послаблениями, что и свёртка
    /// (`fold_history`): без JSON-схемы, без stop-строк и без лимита токенов,
    /// иначе `/stop` обрежет JSON операций на полуслове. Разбор толерантный:
    /// неразобранный ответ оставляет старые факты и записывается в
    /// `last_error`, но **никогда** не роняет ход.
    pub fn update_facts(&mut self, history: &[ChatMessage]) -> Res<FactsDelta> {
        if self.settings.context_strategy != ContextStrategy::Facts {
            return Ok(FactsDelta::default());
        }
        let recent = facts::recent_slice(history);
        if recent.is_empty() {
            return Ok(FactsDelta::default());
        }
        let prompt = facts::extract_prompt(&self.facts, recent);
        let mut settings = self.settings.clone();
        settings.clamp();
        settings.json_mode.enabled = false;
        settings.max_chars = None;
        settings.budget_tokens = None;
        settings.stop.clear();
        let outcome = api::chat(
            &self.endpoint,
            &settings,
            facts::FACTS_SYSTEM,
            &[ChatMessage::user(prompt)],
            None,
        )?;
        self.facts.note_extraction();
        match facts::parse_ops(outcome.text()) {
            Ok(ops) => Ok(self.facts.apply_ops(&ops)),
            Err(e) => {
                self.facts.note_error(e.clone());
                Err(format!("ответ экстрактора не разобран: {e}"))
            }
        }
    }

    /// Разложить последние реплики по трём слоям памяти.
    ///
    /// Устроено как `update_facts`, с одной разницей: экстрактор называет
    /// слой, а окончательное решение принимает маршрутизатор
    /// (`memory::route`). Ошибка разбора оставляет память как была и не
    /// роняет ход.
    pub fn update_memory(&mut self, history: &[ChatMessage]) -> Res<memory::Delta> {
        if self.settings.context_strategy != ContextStrategy::Memory {
            return Ok(memory::Delta::default());
        }
        let recent = memory::recent_slice(history);
        if recent.is_empty() {
            return Ok(memory::Delta::default());
        }
        let prompt = memory::extract_prompt(&self.memory, recent);
        let mut settings = self.settings.clone();
        settings.clamp();
        settings.json_mode.enabled = false;
        settings.max_chars = None;
        settings.budget_tokens = None;
        settings.stop.clear();
        let outcome = api::chat(
            &self.endpoint,
            &settings,
            memory::MEMORY_SYSTEM,
            &[ChatMessage::user(prompt)],
            None,
        )?;
        self.memory.note_extraction();
        match memory::parse_ops(outcome.text()) {
            Ok(ops) => Ok(self.memory.apply_ops(&ops)),
            Err(e) => {
                self.memory.note_error(e.clone());
                Err(format!("ответ экстрактора памяти не разобран: {e}"))
            }
        }
    }

    /// Свернуть отставшую часть истории, если пора.
    ///
    /// Возвращает `Ok(None)`, когда стратегия `off` или целый чанк ещё не
    /// накопился — в этом случае ни одного запроса не делается. Вызывать
    /// нужно **перед** отправкой очередного хода: front-end владеет историей,
    /// а свёртка меняет состояние агента.
    pub fn fold_history(&mut self, history: &[ChatMessage]) -> Res<Option<FoldReport>> {
        if !self.settings.context_strategy.enabled() {
            return Ok(None);
        }
        let policy = Policy::from_settings(&self.settings);
        let Some(target) = self.compressor.due(history.len(), policy) else {
            return Ok(None);
        };
        let from = self.compressor.covered().min(history.len());
        let target = target.min(history.len());
        let chunk = &history[from..target];
        if chunk.is_empty() {
            return Ok(None);
        }

        let prompt = compress::fold_prompt(self.compressor.summary(), chunk);
        // Свёртка идёт своим запросом: без JSON-схемы, без stop-строк и без
        // пользовательского лимита токенов — иначе `/stop` или budget=16
        // обрежут summary на полуслове, и в истории останется огрызок.
        let mut settings = self.settings.clone();
        settings.clamp();
        settings.json_mode.enabled = false;
        settings.max_chars = None;
        settings.budget_tokens = None;
        settings.stop.clear();
        let outcome = api::chat(
            &self.endpoint,
            &settings,
            compress::SUMMARY_SYSTEM,
            &[ChatMessage::user(prompt)],
            None,
        )?;
        let summary = outcome.text().trim().to_string();
        if summary.is_empty() {
            return Err("свёртка вернула пустое summary".into());
        }
        let chunk_chars = compress::chars_of(chunk);
        self.compressor.apply(summary.clone(), target, chunk_chars);
        Ok(Some(FoldReport {
            covered: target,
            chunk_len: chunk.len(),
            chunk_chars,
            summary_chars: summary.chars().count(),
            usage: outcome.usage,
        }))
    }

    pub fn context(&self) -> &ContextBundle {
        &self.context
    }

    pub fn context_files(&self) -> &[LoadedFile] {
        &self.context.files
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn refresh_context(&mut self) {
        self.context = if self.settings.context_enabled {
            ContextBundle::load(&self.cwd, &self.home, MAX_FILE_CHARS)
        } else {
            ContextBundle::empty(self.cwd.clone())
        };
    }

    /// Re-read global + local instruction files from disk.
    pub fn reload_context(&mut self) -> &ContextBundle {
        self.refresh_context();
        &self.context
    }

    /// Working-directory change recomputes the local walk.
    pub fn set_cwd(&mut self, cwd: impl AsRef<Path>) {
        self.cwd = cwd.as_ref().to_path_buf();
        self.refresh_context();
    }

    /// Toggle recomputes (or drops) the loaded files.
    pub fn set_context_enabled(&mut self, enabled: bool) {
        self.settings.context_enabled = enabled;
        self.refresh_context();
    }

    /// One-shot turn: append the user message, call the provider, append the
    /// assistant reply. On transport failure the user message is rolled back.
    ///
    /// This is the agent's own stateful API. The front-ends drive
    /// `complete`/`stream` against an explicit history instead, because both
    /// keep the conversation in a `Session` (see `runtime::AgentBox`) — but
    /// the method stays: it is the shape "agent owns request and response"
    /// that `Agent` exists to provide, and it is covered by tests below.
    #[allow(dead_code)]
    pub fn ask(&mut self, prompt: &str) -> Res<Reply> {
        self.history.push(ChatMessage::user(prompt));
        match self.complete(&self.history.clone()) {
            Ok(reply) => {
                self.history
                    .push(ChatMessage::assistant(reply.text.clone()));
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
        let max_chars = self.effective_settings().max_chars;
        let outcome = self.complete_outcome(history)?;
        Ok(Reply::from_outcome(outcome, max_chars))
    }

    /// Настройки одного запроса: клампнутые плюс то, что профиль добавляет
    /// **сам**, без отдельной команды. Сейчас это потолок ответа: профиль
    /// просит быть кратким словами, а клиент режет по факту (тот же приём,
    /// что и у `max_chars` в настройках — лимит-просьба гарантией не
    /// является). Явно выставленный `max_chars` профиль не перебивает.
    pub(crate) fn effective_settings(&self) -> Settings {
        let mut settings = self.settings.clone();
        settings.clamp();
        if settings.max_chars.is_none() {
            settings.max_chars = self.profile().and_then(|p| p.max_chars);
        }
        settings
    }

    /// System prompt + AGENTS.md files. Sent as `role: system` (see
    /// `context.rs`), never pushed onto `history`.
    pub(crate) fn system_for_request(&self) -> String {
        // Профиль подключается к КАЖДОМУ запросу и идёт первым: он задаёт
        // голос, всё остальное — содержание. Пока системный промпт остался
        // дефолтным («ты — полезный ассистент…»), профиль его заменяет: два
        // описания роли подряд — это спор в одном сообщении. Свой
        // системный промпт профиль не трогает, они складываются.
        let prompt = match (
            self.profile(),
            self.settings.system_prompt.trim() == crate::config::DEFAULT_SYSTEM_PROMPT,
        ) {
            (Some(_), true) => String::new(),
            _ => self.settings.system_prompt.clone(),
        };
        let base = self.context.assemble(&prompt);
        let block = self.profile().map(|p| p.block(&self.known_about_user()));
        let base = match block {
            Some(block) if base.trim().is_empty() => block,
            Some(block) => format!("{block}\n\n{base}"),
            None => base,
        };
        // Sticky-слоты стратегии (summary, факты) уезжают сюда же, рядом с
        // AGENTS.md. Какие именно — решает `strategy::apply`.
        let mut blocks = strategy::blocks(
            self.settings.context_strategy,
            &self.compressor,
            &self.facts,
            &self.memory,
        );
        // Инварианты — отдельный system-блок. Он идёт перед состоянием
        // задачи, чтобы task-state по-прежнему оставался последним.
        if self.settings.invariants {
            if let Some(block) = self.invariants.block() {
                blocks.push(block);
            }
        }
        // Состояние задачи идёт последним — ближе всего к сообщениям: это
        // не роль и не знания, а «где мы сейчас». Выключенная тудушка или
        // пустое состояние не добавляют в запрос ничего.
        if self.todo_on_wire() {
            blocks.push(self.todo.block());
        }
        if blocks.is_empty() {
            return base;
        }
        let joined = blocks.join("\n\n");
        if base.trim().is_empty() {
            joined
        } else {
            format!("{base}\n\n{joined}")
        }
    }

    pub fn complete_outcome(&self, history: &[ChatMessage]) -> Res<Outcome> {
        let settings = self.effective_settings();
        let schema = settings
            .json_mode
            .enabled
            .then(|| settings.json_mode.schema.clone());
        api::chat(
            &self.endpoint,
            &settings,
            &self.system_for_request(),
            self.wire_history(history),
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
        let settings = self.effective_settings();
        api::chat_stream(
            &self.endpoint,
            &settings,
            &self.system_for_request(),
            self.wire_history(history),
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
    fn resume_restores_history_and_settings() {
        let settings = Settings {
            model: "glm-5".into(),
            system_prompt: "be terse".into(),
            context_enabled: false,
            effort: Effort::High,
            temperature: Some(0.3),
            top_p: Some(0.5),
            top_k: Some(20),
            budget_tokens: Some(128),
            ..Settings::default()
        };
        let mut session = Session::new(settings);
        session.push_user("hello".into());
        session.push_assistant("hi".into());
        session.context_files = vec!["/tmp/AGENTS.md".into()];
        let mut agent = Agent::dummy();
        agent.settings_mut().temperature = Some(0.7);
        agent.set_history(vec![ChatMessage::user("stale")]);
        agent.resume(&session);
        assert_eq!(agent.settings().model, "glm-5");
        assert_eq!(agent.settings().system_prompt, "be terse");
        assert!(!agent.settings().context_enabled);
        assert_eq!(agent.settings().effort, Effort::High);
        assert_eq!(agent.settings().temperature, Some(0.3));
        assert_eq!(agent.settings().top_p, Some(0.5));
        assert_eq!(agent.settings().top_k, Some(20));
        assert_eq!(agent.settings().budget_tokens, Some(128));
        assert_eq!(agent.history().len(), 2);
        assert_eq!(agent.history()[0].content, "hello");
        assert_eq!(agent.history()[1].content, "hi");
        assert!(agent.context_files().is_empty());
    }

    #[test]
    fn compression_off_sends_everything_even_with_a_summary() {
        let mut agent = Agent::dummy();
        let history: Vec<ChatMessage> = (0..12)
            .map(|i| ChatMessage::user(format!("m{i}")))
            .collect();
        agent.set_compressor({
            let mut c = crate::compress::Compressor::new();
            c.apply("СВОДКА".into(), 6, 100);
            c
        });
        assert_eq!(agent.strategy(), ContextStrategy::Off);
        assert_eq!(agent.wire_history(&history).len(), 12);
        assert!(!agent.system_for_request().contains("СВОДКА"));
        // Включение — и та же история едет обрезанной, а summary уезжает в system.
        agent.set_strategy(ContextStrategy::Summary);
        let wire = agent.wire_history(&history);
        assert_eq!(wire.len(), 6);
        assert_eq!(wire[0].content, "m6");
        let system = agent.system_for_request();
        assert!(system.contains("СВОДКА"));
        assert_eq!(system.matches("СВОДКА").count(), 1);
    }

    #[test]
    fn fold_does_nothing_until_a_chunk_is_due() {
        let mut agent = Agent::dummy_with(Settings {
            context_strategy: ContextStrategy::Summary,
            keep_recent: 6,
            summarize_every: 10,
            ..Settings::default()
        });
        // Коротко — свёртка не нужна, и запроса не будет (у dummy его некуда
        // и отправить: сеть здесь означала бы ошибку, а не Ok(None)).
        let short: Vec<ChatMessage> = (0..10)
            .map(|i| ChatMessage::user(format!("m{i}")))
            .collect();
        assert!(agent.fold_history(&short).unwrap().is_none());
        // Стратегия off — no-op даже на длинной истории.
        let long: Vec<ChatMessage> = (0..40)
            .map(|i| ChatMessage::user(format!("m{i}")))
            .collect();
        agent.set_strategy(ContextStrategy::Off);
        assert!(agent.fold_history(&long).unwrap().is_none());
    }

    #[test]
    fn reset_drops_the_summary_too() {
        let mut agent = Agent::dummy_with(Settings {
            context_strategy: ContextStrategy::Summary,
            ..Settings::default()
        });
        let mut c = crate::compress::Compressor::new();
        c.apply("СВОДКА".into(), 6, 100);
        agent.set_compressor(c);
        agent.reset();
        assert!(agent.compressor().is_empty());
        assert!(!agent.system_for_request().contains("СВОДКА"));
    }

    #[test]
    fn resume_restores_the_summary_with_the_history() {
        let mut session = Session::new(Settings {
            context_strategy: ContextStrategy::Summary,
            ..Settings::default()
        });
        session.push_user("hello".into());
        session.push_assistant("hi".into());
        session.compressor.apply("СВОДКА".into(), 1, 5);
        let mut agent = Agent::dummy();
        agent.resume(&session);
        assert_eq!(agent.compressor().covered(), 1);
        assert!(agent.system_for_request().contains("СВОДКА"));
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
        let err = agent.complete(&[ChatMessage::user("hi")]).unwrap_err();
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

    fn write_tree(root: &std::path::Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn scratch(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("ask-agent-ctx-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Профиль подключается к каждому запросу и, пока системный промпт
    /// дефолтный, заменяет его собой — ровно то, ради чего он отдельная
    /// настройка, а не приписка к промпту.
    #[test]
    fn profile_replaces_the_default_prompt_and_rides_every_request() {
        let mut agent = Agent::dummy_with(Settings::default());
        let plain = agent.system_for_request();
        assert!(plain.contains(crate::config::DEFAULT_SYSTEM_PROMPT));
        assert!(!plain.contains(crate::profile::BLOCK_HEAD));

        agent.set_profile("gopnik").unwrap();
        let system = agent.system_for_request();
        assert!(system.contains(crate::profile::BLOCK_HEAD));
        assert!(system.contains("братан"));
        assert!(
            !system.contains(crate::config::DEFAULT_SYSTEM_PROMPT),
            "два описания роли подряд — это спор в одном сообщении"
        );
        // Каждый запрос, а не только первый.
        assert_eq!(agent.system_for_request(), system);
    }

    /// Свой системный промпт профиль не съедает: они складываются.
    #[test]
    fn custom_system_prompt_survives_the_profile() {
        let mut agent = Agent::dummy_with(Settings {
            system_prompt: "Отвечай только фактами.".into(),
            ..Settings::default()
        });
        agent.set_profile("chemist").unwrap();
        let system = agent.system_for_request();
        assert!(system.contains("Отвечай только фактами."));
        assert!(system.contains("Гипотеза:"));
        assert_eq!(system.matches(crate::profile::BLOCK_HEAD).count(), 1);
    }

    /// Долговременная память про пользователя (`профиль.*`) доезжает в тот же
    /// блок — это и есть «учитывает автоматически».
    #[test]
    fn long_term_profile_facts_ride_inside_the_profile_block() {
        let mut agent = Agent::dummy_with(Settings::default());
        agent.set_profile("tutor").unwrap();
        let mut store = MemoryStore::in_memory();
        store.apply_ops(&[
            memory::Op::Upsert {
                key: "профиль.имя".into(),
                value: "Евгений".into(),
                layer: Some(Layer::Long),
            },
            // Рабочий слой к персонализации отношения не имеет.
            memory::Op::Upsert {
                key: "задача.срок".into(),
                value: "две недели".into(),
                layer: Some(Layer::Working),
            },
        ]);
        agent.set_memory(store);
        assert_eq!(
            agent.known_about_user(),
            vec![("профиль.имя".to_string(), "Евгений".to_string())]
        );
        let system = agent.system_for_request();
        let (block, _) = crate::profile::split_block(&system);
        assert!(block.unwrap().contains("профиль.имя: Евгений"));
    }

    /// Потолок ответа из профиля применяется сам, но явный `max_chars`
    /// не перебивает.
    #[test]
    fn profile_max_chars_applies_unless_the_user_set_one() {
        let mut agent = Agent::dummy_with(Settings::default());
        assert!(agent.effective_settings().max_chars.is_none());
        agent.set_profile("gopnik").unwrap();
        assert_eq!(agent.effective_settings().max_chars, Some(600));
        agent.settings_mut().max_chars = Some(50);
        assert_eq!(agent.effective_settings().max_chars, Some(50));
    }

    /// Неизвестное имя профиля — ошибка, а не тихая подмена голоса.
    #[test]
    /// Выключенная тудушка не стоит ни одного токена, включённая — уезжает
    /// одним блоком и последней, после блоков стратегии.
    fn todo_block_rides_only_when_enabled_and_active() {
        let mut agent = Agent::dummy_with(Settings {
            system_prompt: "Отвечай только фактами.".into(),
            ..Settings::default()
        });
        let mut state = crate::todo::TaskState::new();
        state.start("посчитать буквы").unwrap();
        state.advance("критерий: совпадение с пересчётом").unwrap();
        agent.set_todo(state);

        // Настройка выключена — блока нет, сколько бы состояния ни накопилось.
        assert!(!agent.todo_on_wire());
        assert!(!agent
            .system_for_request()
            .contains(crate::todo::BLOCK_HEAD));

        agent.settings_mut().todo = true;
        let system = agent.system_for_request();
        assert_eq!(system.matches(crate::todo::BLOCK_HEAD).count(), 1);
        assert!(system.contains("stage=\"plan\""));
        assert!(system.contains("критерий: совпадение с пересчётом"));
        // Состояние идёт последним — ближе всего к сообщениям.
        let (block, rest) = crate::todo::split_block(&system);
        assert!(block.is_some());
        assert!(!rest.contains(crate::todo::BLOCK_HEAD));
        assert!(system.trim_end().ends_with(crate::todo::BLOCK_END));

        // Пустое состояние при включённой настройке — тоже без блока.
        agent.set_todo(crate::todo::TaskState::new());
        assert!(!agent.todo_on_wire());
        assert!(!agent
            .system_for_request()
            .contains(crate::todo::BLOCK_HEAD));
    }

    #[test]
    fn invariant_block_is_separate_and_precedes_task_state() {
        let mut agent = Agent::dummy();
        let system = agent.system_for_request();
        assert_eq!(system.matches(crate::invariants::BLOCK_HEAD).count(), 1);
        assert!(!agent.history().iter().any(|m| m.content.contains(crate::invariants::BLOCK_HEAD)));

        let mut state = crate::todo::TaskState::new();
        state.start("проверить порядок").unwrap();
        agent.set_todo(state);
        agent.settings_mut().todo = true;
        let system = agent.system_for_request();
        let inv = system.find(crate::invariants::BLOCK_HEAD).unwrap();
        let todo = system.find(crate::todo::BLOCK_HEAD).unwrap();
        assert!(inv < todo);
        assert!(system.trim_end().ends_with(crate::todo::BLOCK_END));

        agent.settings_mut().invariants = false;
        assert!(!agent.system_for_request().contains(crate::invariants::BLOCK_HEAD));
    }

    #[test]
    fn unknown_profile_is_an_error_and_off_clears_the_block() {
        let mut agent = Agent::dummy_with(Settings::default());
        assert!(agent.set_profile("нет-такого").is_err());
        assert!(agent.profile().is_none());
        agent.set_profile("chemist").unwrap();
        assert!(agent.profile().is_some());
        agent.set_profile(crate::profile::OFF).unwrap();
        assert!(agent.profile().is_none());
        assert!(!agent
            .system_for_request()
            .contains(crate::profile::BLOCK_HEAD));
    }

    #[test]
    fn system_prompt_contains_global_and_local_exactly_once() {
        let home = scratch("home");
        write_tree(&home, ".pi/agent/AGENTS.md", "AGENT_GLOBAL");
        let cwd = scratch("cwd");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_tree(&cwd, "AGENTS.md", "AGENT_LOCAL");
        let agent = Agent::dummy_at(cwd.clone(), home.clone(), Settings::default());
        assert_eq!(agent.cwd(), cwd.as_path());
        assert_eq!(agent.context().cwd, cwd);
        assert!(agent.context().enabled);
        let system = agent.system_for_request();
        assert_eq!(system.matches("AGENT_GLOBAL").count(), 1);
        assert_eq!(system.matches("AGENT_LOCAL").count(), 1);
        assert_eq!(agent.context_files().len(), 2);
        assert!(agent.history().is_empty());
        let again = agent.system_for_request();
        assert_eq!(again.matches("AGENT_GLOBAL").count(), 1);
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn toggle_and_cwd_recompute_context() {
        let home = scratch("tog-home");
        write_tree(&home, ".pi/agent/AGENTS.md", "G");
        let a = scratch("tog-a");
        std::fs::create_dir_all(a.join(".git")).unwrap();
        write_tree(&a, "AGENTS.md", "LOCAL_A");
        let b = scratch("tog-b");
        std::fs::create_dir_all(b.join(".git")).unwrap();
        write_tree(&b, "AGENTS.md", "LOCAL_B");
        let mut agent = Agent::dummy_at(a.clone(), home.clone(), Settings::default());
        assert!(agent.system_for_request().contains("LOCAL_A"));
        agent.set_cwd(&b);
        let system = agent.system_for_request();
        assert!(system.contains("LOCAL_B"));
        assert!(!system.contains("LOCAL_A"));
        agent.set_context_enabled(false);
        assert!(!agent.settings().context_enabled);
        assert!(agent.context_files().is_empty());
        assert!(!agent.system_for_request().contains("LOCAL_B"));
        agent.set_context_enabled(true);
        assert!(agent.system_for_request().contains("LOCAL_B"));
        write_tree(&b, "AGENTS.md", "LOCAL_B_RELOADED");
        agent.reload_context();
        assert!(agent.system_for_request().contains("LOCAL_B_RELOADED"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }
}
