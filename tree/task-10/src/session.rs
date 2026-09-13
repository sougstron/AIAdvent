//! Chat sessions: `/new` starts one, `/sessions` lists and switches.
//! Persisted as one JSON file per session under `~/.ask6/sessions/` so this
//! app never reads or clobbers the old TUI's `~/.ask/sessions/` chats.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::{ChatMessage, Role};
use crate::branch::BranchStore;
use crate::compress::Compressor;
use crate::config::{Res, Settings};
use crate::facts::FactStore;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
    /// Partial assistant text from a turn stopped mid-stream.
    #[serde(default)]
    pub interrupted: bool,
}

impl From<&ChatMessage> for StoredMessage {
    fn from(m: &ChatMessage) -> Self {
        StoredMessage {
            role: match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
            }
            .to_string(),
            content: m.content.clone(),
            interrupted: false,
        }
    }
}

impl StoredMessage {
    pub fn to_chat_message(&self) -> ChatMessage {
        match self.role.as_str() {
            "assistant" => ChatMessage::assistant(self.content.clone()),
            "system" => ChatMessage {
                role: Role::System,
                content: self.content.clone(),
            },
            _ => ChatMessage::user(self.content.clone()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub messages: Vec<StoredMessage>,
    #[serde(default)]
    pub settings: Settings,
    /// AGENTS.md / CLAUDE.md paths ingested when this session last saved.
    #[serde(default)]
    pub context_files: Vec<String>,
    /// Бегущее summary сжатой истории. `#[serde(default)]` — сессии,
    /// записанные до задачи 9, читаются как «сжатия не было».
    #[serde(default)]
    pub compressor: Compressor,
    /// Key-value память диалога (стратегия `facts`). Сессии до задачи 10
    /// читаются как «фактов нет».
    #[serde(default)]
    facts: FactStore,
    /// Дерево веток (стратегия `branch`). `messages` остаётся плоским путём
    /// активной ветки — так старые сессии, `SessionSummary` и всё, что читает
    /// `messages`, продолжают работать без изменений.
    #[serde(default)]
    tree: BranchStore,
}

/// Per-process counter that makes ids unique when many sessions are created
/// inside the same second — the multi-agent case (see `runtime.rs`). Without
/// it, `{secs}-{pid}` collides and boxes silently overwrite each other's file.
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

impl Session {
    pub fn new(settings: Settings) -> Session {
        let now = now_secs();
        let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        Session {
            id: format!("{now}-{:04x}-{seq:x}", std::process::id() & 0xffff),
            title: "New chat".into(),
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            settings,
            context_files: Vec::new(),
            compressor: Compressor::default(),
            facts: FactStore::default(),
            tree: BranchStore::new(),
        }
    }

    pub fn facts(&self) -> &FactStore {
        &self.facts
    }

    pub fn set_facts(&mut self, facts: FactStore) {
        self.facts = facts;
    }

    pub fn tree(&self) -> &BranchStore {
        &self.tree
    }

    pub fn tree_mut(&mut self) -> &mut BranchStore {
        &mut self.tree
    }

    /// Сессия, записанная до задачи 10, приходит с плоским `messages` и
    /// пустым деревом — собираем из неё единственную ветку `main`. Вызывать
    /// сразу после загрузки с диска.
    pub fn migrate_tree(&mut self) {
        if self.tree.needs_migration() && !self.messages.is_empty() {
            self.tree = BranchStore::from_messages(&self.messages);
            self.tree.store_memory(self.compressor.clone(), self.facts.clone());
        }
    }

    /// Переключиться на другую ветку: память текущей уезжает в дерево,
    /// память целевой поднимается в сессию, а `messages` становится путём
    /// новой ветки. Возвращает её имя.
    pub fn switch_branch(&mut self, selector: &str) -> Res<String> {
        let current = (self.compressor.clone(), self.facts.clone());
        let (compressor, facts) = self.tree.switch_memory(selector, current)?;
        self.compressor = compressor;
        self.facts = facts;
        self.messages = self.tree.path();
        self.updated_at = now_secs();
        Ok(self.tree.active_name().to_string())
    }

    /// Привести дерево в соответствие с `messages` — front-end иногда правит
    /// список напрямую (откат хода после ошибки сети).
    pub fn resync_tree(&mut self) {
        self.tree.resync(&self.messages);
    }

    pub fn history(&self) -> Vec<ChatMessage> {
        self.messages
            .iter()
            .filter(|m| m.role != "system")
            .map(StoredMessage::to_chat_message)
            .collect()
    }

    pub fn push_user(&mut self, content: String) {
        if self.messages.is_empty() {
            let title = title_from(&content);
            if !title.is_empty() {
                self.title = title;
            }
        }
        let msg = StoredMessage {
            role: "user".into(),
            content,
            interrupted: false,
        };
        self.tree.push(msg.clone());
        self.messages.push(msg);
        self.updated_at = now_secs();
    }

    pub fn push_assistant(&mut self, content: String) {
        self.push_assistant_marked(content, false);
    }

    pub fn push_assistant_interrupted(&mut self, content: String) {
        self.push_assistant_marked(content, true);
    }

    fn push_assistant_marked(&mut self, content: String, interrupted: bool) {
        let msg = StoredMessage {
            role: "assistant".into(),
            content,
            interrupted,
        };
        self.tree.push(msg.clone());
        self.messages.push(msg);
        self.updated_at = now_secs();
    }

    pub fn rename(&mut self, title: &str) -> Res<()> {
        let title = title.trim();
        if title.is_empty() {
            return Err("title must not be empty".into());
        }
        self.title = title.to_string();
        self.updated_at = now_secs();
        Ok(())
    }

    /// Copy live agent settings, context paths, and history into this session.
    pub fn capture_from(
        &mut self,
        settings: &Settings,
        context_files: impl IntoIterator<Item = String>,
        history: &[ChatMessage],
        compressor: &Compressor,
        facts: &FactStore,
    ) {
        self.settings = settings.clone();
        self.compressor = compressor.clone();
        self.facts = facts.clone();
        self.context_files = context_files.into_iter().collect();
        let interrupted: Vec<bool> = self.messages.iter().map(|m| m.interrupted).collect();
        self.messages = history.iter().map(StoredMessage::from).collect();
        for (msg, flag) in self.messages.iter_mut().zip(interrupted) {
            msg.interrupted = flag;
        }
        self.resync_tree();
        self.tree.store_memory(self.compressor.clone(), self.facts.clone());
        if self.title == "New chat" {
            if let Some(title) = self
                .messages
                .iter()
                .find(|m| m.role == "user")
                .map(|m| title_from(&m.content))
            {
                if !title.is_empty() {
                    self.title = title;
                }
            }
        }
        self.updated_at = now_secs();
    }

    pub fn save(&self, dir: &Path) -> Res<()> {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = session_file(dir, &self.id)?;
        let data = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".{}.json.tmp", self.id));
        fs::write(&tmp, data).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("cannot replace {}: {e}", path.display())
        })
    }
}

/// One line of `/sessions` output — cheap to build without extra I/O.
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated_at: u64,
    pub turn_count: usize,
    pub model: String,
}

impl SessionSummary {
    fn from_session(s: &Session) -> SessionSummary {
        SessionSummary {
            id: s.id.clone(),
            title: s.title.clone(),
            updated_at: s.updated_at,
            turn_count: s.messages.iter().filter(|m| m.role == "user").count(),
            model: s.settings.model.clone(),
        }
    }

    pub fn line(&self) -> String {
        format!(
            "{:<20} {:<32} {:<16} {:>3} turns  {}",
            self.id,
            truncate_chars(&self.title, 32),
            truncate_chars(&self.model, 16),
            self.turn_count,
            format_updated(self.updated_at),
        )
    }

    pub fn panel_line(&self, selected: bool) -> String {
        let marker = if selected { "▸ " } else { "  " };
        format!(
            "{marker}{:<24} {:<14} {:>3} turns  {}",
            truncate_chars(&self.title, 24),
            truncate_chars(&self.model, 14),
            self.turn_count,
            format_updated(self.updated_at),
        )
    }
}

pub fn sessions_dir() -> PathBuf {
    sessions_dir_from(
        std::env::var("ASK_SESSIONS_DIR").ok(),
        std::env::var("HOME").ok(),
    )
}

pub fn sessions_dir_from(override_dir: Option<String>, home: Option<String>) -> PathBuf {
    if let Some(dir) = override_dir.filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    PathBuf::from(home.unwrap_or_else(|| ".".into()))
        .join(".ask6")
        .join("sessions")
}

pub fn list_sessions(dir: &Path) -> Vec<SessionSummary> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match load_session_file(&path) {
            Ok(s) => out.push(SessionSummary::from_session(&s)),
            Err(e) => {
                eprintln!("warning: skipping corrupt session file {}: {e}", path.display());
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    out
}

pub fn load_session(dir: &Path, id: &str) -> Res<Session> {
    load_session_file(&session_file(dir, id)?)
}

pub fn continue_last(dir: &Path) -> Res<Session> {
    let list = list_sessions(dir);
    let Some(first) = list.first() else {
        return Err("no saved sessions".into());
    };
    load_session(dir, &first.id)
}

pub fn delete_session(dir: &Path, id: &str) -> Res<()> {
    let path = session_file(dir, id)?;
    fs::remove_file(&path).map_err(|e| format!("cannot delete {}: {e}", path.display()))
}

pub fn rename_session(dir: &Path, id: &str, title: &str) -> Res<Session> {
    let mut s = load_session(dir, id)?;
    s.rename(title)?;
    s.save(dir)?;
    Ok(s)
}

pub fn format_updated(secs: u64) -> String {
    let (y, m, d, hh, mm) = unix_to_utc(secs);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

fn load_session_file(path: &Path) -> Res<Session> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut session: Session =
        serde_json::from_str(&raw).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    // Файл до задачи 10 знает только плоский `messages` — дерево строим на
    // лету, одной линейной веткой.
    session.migrate_tree();
    Ok(session)
}

fn session_file(dir: &Path, id: &str) -> Res<PathBuf> {
    if !is_safe_id(id) {
        return Err(format!("invalid session id `{id}`"));
    }
    Ok(dir.join(format!("{id}.json")))
}

fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && !id.contains(['/', '\\', '\0'])
        && Path::new(id)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn title_from(content: &str) -> String {
    content
        .replace("\r\n", "\n")
        .replace(['\n', '\r'], " ")
        .chars()
        .take(32)
        .collect()
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.pop();
        out.push('…');
    }
    out
}

/// UTC civil date from a Unix timestamp. Howard Hinnant's `civil_from_days`.
fn unix_to_utc(secs: u64) -> (i32, u32, u32, u32, u32) {
    let z = secs / 86_400 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let rem = secs % 86_400;
    (
        y as i32,
        m as u32,
        d as u32,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
    )
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Effort, DEFAULT_MODEL};
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Сессии, записанные до задачи 9, не знают ни о стратегии, ни о
    /// summary — они обязаны читаться как «сжатия не было».
    #[test]
    fn pre_task9_session_files_still_load() {
        let raw = r#"{"id":"1","title":"t","created_at":1,"updated_at":2,
            "messages":[{"role":"user","content":"hi"}],
            "settings":{"model":"glm-5.3-flash","system_prompt":"p","effort":"Low",
                "json_mode":{"enabled":false,"schema":{}},"max_chars":null,"stop":[],
                "temperature":null}}"#;
        let s: Session = serde_json::from_str(raw).unwrap();
        assert!(s.compressor.is_empty());
        assert_eq!(s.settings.context_strategy, crate::config::ContextStrategy::Off);
        assert_eq!(s.settings.keep_recent, crate::config::DEFAULT_KEEP_RECENT);
        assert_eq!(s.settings.summarize_every, crate::config::DEFAULT_SUMMARIZE_EVERY);
    }

    /// Сессия, записанная до задачи 10, не знает ни о фактах, ни о дереве
    /// веток: она обязана читаться и превращаться в одну линейную ветку
    /// `main` с той же самой историей.
    #[test]
    fn pre_task10_session_migrates_to_a_single_branch() {
        let dir = tmp_dir("task10-migration");
        std::fs::create_dir_all(&dir).unwrap();
        let raw = r#"{"id":"legacy-1","title":"t","created_at":1,"updated_at":2,
            "messages":[{"role":"user","content":"hi"},
                        {"role":"assistant","content":"yo"},
                        {"role":"user","content":"ещё"}],
            "settings":{"model":"glm-5.3-flash","system_prompt":"p","effort":"Low",
                "json_mode":{"enabled":false,"schema":{}},"max_chars":null,"stop":[],
                "temperature":null}}"#;
        std::fs::write(dir.join("legacy-1.json"), raw).unwrap();
        let s = load_session(&dir, "legacy-1").unwrap();
        assert!(s.facts().is_empty());
        assert_eq!(s.tree().branches().len(), 1);
        assert_eq!(s.tree().active_name(), crate::branch::MAIN_BRANCH);
        // Путь ветки побайтово равен плоской истории файла.
        assert_eq!(s.tree().path(), s.messages);
        let history = s.history();
        assert_eq!(history.len(), 3);
        assert_eq!(history[2].content, "ещё");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ветки должны переживать запись на диск вместе со своей памятью.
    #[test]
    fn branches_and_facts_survive_a_round_trip() {
        let dir = tmp_dir("branch-roundtrip");
        let mut s = Session::new(Settings::default());
        s.push_user("общий".into());
        s.push_assistant("ответ".into());
        s.tree_mut().checkpoint(None).unwrap();
        s.tree_mut().fork("alpha", None).unwrap();
        s.switch_branch("alpha").unwrap();
        s.push_user("ALPHA?".into());
        let mut f = FactStore::new();
        f.set("секрет", "ALPHA");
        s.set_facts(f);
        s.capture_from(&Settings::default(), Vec::<String>::new(), &s.clone().history(), &Compressor::new(), s.clone().facts());
        s.save(&dir).unwrap();

        let mut back = load_session(&dir, &s.id).unwrap();
        assert_eq!(back.tree().branches().len(), 2);
        assert_eq!(back.tree().active_name(), "alpha");
        assert_eq!(back.facts().get("секрет"), Some("ALPHA"));
        // Возврат в main не тащит за собой ни реплики, ни факты alpha.
        back.switch_branch("main").unwrap();
        assert_eq!(back.messages.len(), 2);
        assert!(back.facts().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Summary — часть памяти сессии: без него возобновлённый чат ссылается
    /// на `covered` сообщений, которых уже никто не описывает.
    #[test]
    fn summary_survives_a_save_load_round_trip() {
        let dir = tmp_dir("compressor-roundtrip");
        let mut s = Session::new(Settings::default());
        s.push_user("hi".into());
        s.push_assistant("yo".into());
        let mut c = crate::compress::Compressor::new();
        c.apply("СВОДКА".into(), 1, 42);
        let mut f = FactStore::new();
        f.set("цель", "стенд");
        s.capture_from(&Settings::default(), Vec::<String>::new(), &s.history(), &c, &f);
        s.save(&dir).unwrap();
        let back = load_session(&dir, &s.id).unwrap();
        assert_eq!(back.compressor.summary(), "СВОДКА");
        assert_eq!(back.compressor.covered(), 1);
        assert_eq!(back.compressor.folded_chars(), 42);
        assert_eq!(back.facts().get("цель"), Some("стенд"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ask6-session-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn with_temp_home<R>(label: &str, f: impl FnOnce(&Path) -> R) -> R {
        let home = tmp_dir(label);
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_home = std::env::var("HOME").ok();
        let old_override = std::env::var("ASK_SESSIONS_DIR").ok();
        std::env::remove_var("ASK_SESSIONS_DIR");
        std::env::set_var("HOME", &home);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&home)));
        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match old_override {
            Some(v) => std::env::set_var("ASK_SESSIONS_DIR", v),
            None => std::env::remove_var("ASK_SESSIONS_DIR"),
        }
        match result {
            Ok(r) => r,
            Err(p) => std::panic::resume_unwind(p),
        }
    }

    #[test]
    fn save_then_list_then_load_round_trips() {
        let dir = tmp_dir("roundtrip");
        let mut s = Session::new(Settings::default());
        s.push_user("hello there".into());
        s.push_assistant("hi!".into());
        s.context_files = vec!["/tmp/AGENTS.md".into()];
        s.save(&dir).unwrap();

        let listed = list_sessions(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "hello there");
        assert_eq!(listed[0].turn_count, 1);
        assert_eq!(listed[0].model, DEFAULT_MODEL);

        let loaded = load_session(&dir, &s.id).unwrap();
        assert_eq!(loaded.history().len(), 2);
        assert_eq!(loaded.history()[1].content, "hi!");
        assert_eq!(loaded.context_files, vec!["/tmp/AGENTS.md"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_against_temp_home() {
        with_temp_home("home-round", |home| {
            let dir = sessions_dir();
            assert_eq!(dir, home.join(".ask6").join("sessions"));
            let mut s = Session::new(Settings::default());
            s.push_user("from home".into());
            s.push_assistant("ok".into());
            s.save(&dir).unwrap();
            let listed = list_sessions(&dir);
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].title, "from home");
            let loaded = load_session(&dir, &s.id).unwrap();
            assert_eq!(loaded.history()[0].content, "from home");
        });
    }

    #[test]
    fn settings_round_trip_preserves_snapshot() {
        let dir = tmp_dir("settings");
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
        let mut s = Session::new(settings.clone());
        s.push_user("ping".into());
        s.save(&dir).unwrap();
        let loaded = load_session(&dir, &s.id).unwrap();
        assert_eq!(loaded.settings.model, "glm-5");
        assert_eq!(loaded.settings.system_prompt, "be terse");
        assert!(!loaded.settings.context_enabled);
        assert_eq!(loaded.settings.effort, Effort::High);
        assert_eq!(loaded.settings.temperature, Some(0.3));
        assert_eq!(loaded.settings.top_p, Some(0.5));
        assert_eq!(loaded.settings.top_k, Some(20));
        assert_eq!(loaded.settings.budget_tokens, Some(128));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_fields_default_for_compat() {
        let raw = r#"{"id":"legacy-1","title":"old chat","messages":[{"role":"user","content":"hi"}]}"#;
        let s: Session = serde_json::from_str(raw).unwrap();
        assert_eq!(s.id, "legacy-1");
        assert_eq!(s.settings.model, DEFAULT_MODEL);
        assert!(s.settings.context_enabled);
        assert!(s.context_files.is_empty());
        assert!(!s.messages[0].interrupted);
        assert_eq!(s.created_at, 0);
    }

    #[test]
    fn corrupt_file_is_skipped_with_valid_neighbors() {
        let dir = tmp_dir("corrupt");
        fs::write(dir.join("partial.json"), "{").unwrap();
        fs::write(dir.join("garbage.json"), "not json").unwrap();
        let mut s = Session::new(Settings::default());
        s.push_user("keep me".into());
        s.save(&dir).unwrap();
        let listed = list_sessions(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "keep me");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_is_newest_first() {
        let dir = tmp_dir("order");
        let mut older = Session::new(Settings::default());
        older.id = "older".into();
        older.push_user("old".into());
        older.updated_at = 100;
        older.save(&dir).unwrap();
        let mut newer = Session::new(Settings::default());
        newer.id = "newer".into();
        newer.push_user("new".into());
        newer.updated_at = 200;
        newer.save(&dir).unwrap();
        let listed = list_sessions(&dir);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "newer");
        assert_eq!(listed[1].id, "older");
        let last = continue_last(&dir).unwrap();
        assert_eq!(last.id, "newer");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn title_is_set_from_first_user_message_only() {
        let mut s = Session::new(Settings::default());
        s.push_user("first message here".into());
        s.push_user("second one".into());
        assert_eq!(s.title, "first message here");
    }

    #[test]
    fn title_folds_newlines_to_spaces() {
        let mut s = Session::new(Settings::default());
        s.push_user("first line\nsecond line\r\nthird".into());
        assert_eq!(s.title, "first line second line third");
    }

    #[test]
    fn delete_session_removes_it_from_the_list() {
        let dir = tmp_dir("delete");
        let mut s = Session::new(Settings::default());
        s.push_user("goodbye chat".into());
        s.save(&dir).unwrap();
        assert_eq!(list_sessions(&dir).len(), 1);

        delete_session(&dir, &s.id).unwrap();
        assert_eq!(list_sessions(&dir).len(), 0);
        assert!(load_session(&dir, &s.id).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_session_missing_file_errors() {
        let dir = tmp_dir("delete-missing");
        assert!(delete_session(&dir, "no-such-id").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_updates_title_on_disk() {
        let dir = tmp_dir("rename");
        let mut s = Session::new(Settings::default());
        s.push_user("original title here".into());
        s.save(&dir).unwrap();
        rename_session(&dir, &s.id, "  better name  ").unwrap();
        let loaded = load_session(&dir, &s.id).unwrap();
        assert_eq!(loaded.title, "better name");
        assert!(rename_session(&dir, &s.id, "   ").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn interrupted_flag_round_trips() {
        let dir = tmp_dir("interrupted");
        let mut s = Session::new(Settings::default());
        s.push_user("go".into());
        s.push_assistant_interrupted("partial answer".into());
        s.save(&dir).unwrap();
        let loaded = load_session(&dir, &s.id).unwrap();
        assert!(loaded.messages[1].interrupted);
        assert_eq!(loaded.history()[1].content, "partial answer");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_traversal_id_is_rejected() {
        let dir = tmp_dir("safe-id");
        assert!(load_session(&dir, "../secret").is_err());
        assert!(load_session(&dir, "a/b").is_err());
        assert!(load_session(&dir, "").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sessions_dir_prefers_override_then_ask6_under_home() {
        let home = PathBuf::from("/tmp/ask6-home");
        assert_eq!(
            sessions_dir_from(None, Some(home.display().to_string())),
            home.join(".ask6").join("sessions")
        );
        assert_eq!(
            sessions_dir_from(Some("/custom/sessions".into()), Some(home.display().to_string())),
            PathBuf::from("/custom/sessions")
        );
    }

    #[test]
    fn many_sessions_in_one_process_get_distinct_ids() {
        let ids: BTreeSet<String> = (0..100)
            .map(|_| Session::new(Settings::default()).id)
            .collect();
        assert_eq!(ids.len(), 100, "session ids collided inside one process");
        assert!(ids.iter().all(|id| is_safe_id(id)));
    }

    #[test]
    fn distinct_ids_mean_distinct_files_on_disk() {
        let dir = tmp_dir("hundred-files");
        for i in 0..100 {
            let mut s = Session::new(Settings::default());
            s.push_user(format!("box {i}"));
            s.save(&dir).unwrap();
        }
        assert_eq!(list_sessions(&dir).len(), 100);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unix_epoch_formats_as_utc() {
        assert_eq!(format_updated(0), "1970-01-01 00:00");
        assert_eq!(format_updated(1_700_000_000), "2023-11-14 22:13");
    }

    #[test]
    fn continue_last_errors_when_empty() {
        let dir = tmp_dir("empty-continue");
        assert_eq!(continue_last(&dir).unwrap_err(), "no saved sessions");
        let _ = fs::remove_dir_all(&dir);
    }
}
