//! Chat sessions: `/new` starts one, `/sessions` lists and switches between
//! the ones already on disk. Persisted as one JSON file per session under
//! `~/.ask/sessions/`, so old chats survive restarts.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::{ChatMessage, Role};
use crate::config::{Res, Settings};

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
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

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub messages: Vec<StoredMessage>,
    pub settings: Settings,
}

impl Session {
    pub fn new(settings: Settings) -> Session {
        let now = now_secs();
        Session {
            id: format!("{now}-{:04x}", std::process::id() & 0xffff),
            title: "New chat".into(),
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            settings,
        }
    }

    pub fn history(&self) -> Vec<ChatMessage> {
        self.messages.iter().map(|m| m.to_chat_message()).collect()
    }

    pub fn push_user(&mut self, content: String) {
        if self.messages.is_empty() {
            // Newlines in the title would smear the single-line header, so
            // fold them to spaces.
            self.title = content.replace("\r\n", "\n").replace(['\n', '\r'], " ")
                .chars()
                .take(32)
                .collect();
        }
        self.messages.push(StoredMessage {
            role: "user".into(),
            content,
        });
        self.updated_at = now_secs();
    }

    pub fn push_assistant(&mut self, content: String) {
        self.messages.push(StoredMessage {
            role: "assistant".into(),
            content,
        });
        self.updated_at = now_secs();
    }

    pub fn save(&self, dir: &Path) -> Res<()> {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = dir.join(format!("{}.json", self.id));
        let data = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(&path, data).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

/// One line of `/sessions` output — cheap to build without loading full history.
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated_at: u64,
    pub message_count: usize,
}

pub fn sessions_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ASK_SESSIONS_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ask").join("sessions")
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
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(s) = serde_json::from_str::<Session>(&raw) else {
            continue;
        };
        out.push(SessionSummary {
            id: s.id,
            title: s.title,
            updated_at: s.updated_at,
            message_count: s.messages.len(),
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    out
}

pub fn load_session(dir: &Path, id: &str) -> Res<Session> {
    let path = dir.join(format!("{id}.json"));
    let raw = fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

pub fn delete_session(dir: &Path, id: &str) -> Res<()> {
    let path = dir.join(format!("{id}.json"));
    fs::remove_file(&path).map_err(|e| format!("cannot delete {}: {e}", path.display()))
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
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ask-session-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn save_then_list_then_load_round_trips() {
        let dir = tmp_dir("roundtrip");
        let mut s = Session::new(Settings::default());
        s.push_user("hello there".into());
        s.push_assistant("hi!".into());
        s.save(&dir).unwrap();

        let listed = list_sessions(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "hello there");
        assert_eq!(listed[0].message_count, 2);

        let loaded = load_session(&dir, &s.id).unwrap();
        assert_eq!(loaded.history().len(), 2);
        assert_eq!(loaded.history()[1].content, "hi!");
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
        fs::create_dir_all(&dir).unwrap();
        assert!(delete_session(&dir, "no-such-id").is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
