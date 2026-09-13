//! Global + project instruction files (`AGENTS.md` / `CLAUDE.md`).
//!
//! # Where they go on the wire
//!
//! Folded into the **system** message, not a leading user turn.
//! z.ai `POST /api/paas/v4/chat/completions` is OpenAI-compatible and already
//! accepts `role: system` (TASK-026 live `glm-5.3-flash` counted the default
//! system prompt in `prompt_tokens`). GLM treats that slot as sticky
//! instructions for the request. A leading user message would:
//!
//! - break user/assistant alternation some chat templates assume
//! - land in `Agent.history` / saved sessions and be replayed every turn
//! - show up as a fake user bubble in the TUI
//!
//! Each HTTP request therefore sends the assembled block **once**, as
//! `messages[0]`. Conversation history stays user/assistant only, so a
//! long chat does not grow N copies of AGENTS.md.
//!
//! # Discovery
//!
//! Global, first existing regular file wins, in this order (paths relative
//! to `$HOME`):
//!
//! 1. `~/.pi/agent/AGENTS.md`
//! 2. `~/.claude/CLAUDE.md`
//! 3. `~/.config/ask/AGENTS.md`
//!
//! Local: from `cwd` walk up to the git root (a directory that contains
//! `.git`). At each directory `AGENTS.md` wins over `CLAUDE.md`. Several
//! hits are included **outermost-first** (git root, then children, then
//! cwd). No git root → only `cwd` is searched, never `/`.
//!
//! Unreadable, missing, directory, or NUL-containing ("binary") files are
//! skipped. Each file is capped at [`MAX_FILE_CHARS`]; overflow keeps a
//! prefix and appends a truncation marker.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Per-file character cap. Overflow is a prefix plus a truncation marker.
pub const MAX_FILE_CHARS: usize = 32_768;

/// Relative to `$HOME`. First existing regular file wins.
pub const GLOBAL_CANDIDATES: &[&str] = &[
    ".pi/agent/AGENTS.md",
    ".claude/CLAUDE.md",
    ".config/ask/AGENTS.md",
];

const LOCAL_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextScope {
    Global,
    Local,
}

impl ContextScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            ContextScope::Global => "global",
            ContextScope::Local => "local",
        }
    }
}

/// One instruction file as it will be sent, plus the numbers the TUI shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedFile {
    pub path: PathBuf,
    pub scope: ContextScope,
    pub body: String,
    /// Characters of `body` (truncated text + marker, if any).
    pub chars: usize,
    pub truncated: bool,
}

/// Snapshot of what `reload_context` last loaded.
#[derive(Clone, Debug)]
pub struct ContextBundle {
    pub enabled: bool,
    pub cwd: PathBuf,
    pub files: Vec<LoadedFile>,
}

impl ContextBundle {
    pub fn empty(cwd: PathBuf) -> ContextBundle {
        ContextBundle {
            enabled: false,
            cwd,
            files: Vec::new(),
        }
    }

    pub fn load(cwd: &Path, home: &Path, max_file_chars: usize) -> ContextBundle {
        let cwd = absolute(cwd);
        let mut files = Vec::new();
        let mut seen = Vec::new();
        if let Some(path) = discover_global(home) {
            push_loaded(
                &mut files,
                &mut seen,
                &path,
                ContextScope::Global,
                max_file_chars,
            );
        }
        for path in discover_local(&cwd) {
            push_loaded(
                &mut files,
                &mut seen,
                &path,
                ContextScope::Local,
                max_file_chars,
            );
        }
        ContextBundle {
            enabled: true,
            cwd,
            files,
        }
    }

    /// System prompt, then global file, then local files, each tagged with
    /// its source path. Empty / disabled → the system prompt unchanged.
    pub fn assemble(&self, system_prompt: &str) -> String {
        if !self.enabled || self.files.is_empty() {
            return system_prompt.to_string();
        }
        let mut out = String::new();
        if !system_prompt.is_empty() {
            out.push_str(system_prompt);
        }
        for file in &self.files {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("<agent-instructions scope=\"");
            out.push_str(file.scope.as_str());
            out.push_str("\" source=\"");
            out.push_str(&file.path.display().to_string());
            out.push_str("\">\n");
            out.push_str(&file.body);
            out.push_str("\n</agent-instructions>");
        }
        out
    }
}

fn push_loaded(
    files: &mut Vec<LoadedFile>,
    seen: &mut Vec<PathBuf>,
    path: &Path,
    scope: ContextScope,
    max_file_chars: usize,
) {
    let key = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if seen.iter().any(|p| p == &key) {
        return;
    }
    if let Some(loaded) = load_file(path, scope, max_file_chars) {
        seen.push(key);
        files.push(loaded);
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn discover_global(home: &Path) -> Option<PathBuf> {
    for rel in GLOBAL_CANDIDATES {
        let path = home.join(rel);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn discover_local(cwd: &Path) -> Vec<PathBuf> {
    let dirs = local_search_dirs(cwd);
    let mut found = Vec::new();
    for dir in &dirs {
        if let Some(path) = pick_local(dir) {
            found.push(path);
        }
    }
    found.reverse();
    found
}

/// Innermost-first (cwd → git root). No `.git` → cwd only.
fn local_search_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = absolute(cwd);
    let mut found_git = false;
    loop {
        if current.join(".git").exists() {
            found_git = true;
        }
        dirs.push(current.clone());
        if found_git {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current.as_path() && !parent.as_os_str().is_empty() => {
                current = parent.to_path_buf();
            }
            _ => break,
        }
    }
    if found_git {
        dirs
    } else {
        dirs.truncate(1);
        dirs
    }
}

fn pick_local(dir: &Path) -> Option<PathBuf> {
    for name in LOCAL_NAMES {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn load_file(path: &Path, scope: ContextScope, max_chars: usize) -> Option<LoadedFile> {
    if !path.is_file() {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    let limit = (max_chars.saturating_mul(4) as u64).saturating_add(1);
    let mut buf = Vec::new();
    file.take(limit).read_to_end(&mut buf).ok()?;
    if buf.contains(&0) {
        return None;
    }
    let text = String::from_utf8(buf).ok()?;
    let total = text.chars().count();
    let truncated = total > max_chars;
    let mut body = if truncated {
        text.chars().take(max_chars).collect()
    } else {
        text
    };
    if truncated {
        body.push_str(&format!("\n\n[truncated to {max_chars} characters]"));
    }
    let chars = body.chars().count();
    Some(LoadedFile {
        path: path.to_path_buf(),
        scope,
        body,
        chars,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Scratch {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("ask-ctx-{}-{n}-{label}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(&path, body).unwrap();
        path
    }

    fn git_repo(root: &Path) {
        fs::create_dir_all(root.join(".git")).unwrap();
    }

    #[test]
    fn global_first_match_wins_in_documented_order() {
        let home = Scratch::new("global-order");
        write(home.path(), ".pi/agent/AGENTS.md", "PI_MARK");
        write(home.path(), ".claude/CLAUDE.md", "CLAUDE_MARK");
        write(home.path(), ".config/ask/AGENTS.md", "ASK_MARK");
        let cwd = Scratch::new("global-order-cwd");
        git_repo(cwd.path());
        let bundle = ContextBundle::load(cwd.path(), home.path(), MAX_FILE_CHARS);
        assert_eq!(bundle.files.len(), 1);
        assert_eq!(bundle.files[0].scope, ContextScope::Global);
        assert_eq!(bundle.files[0].body, "PI_MARK");
        assert!(!bundle.assemble("SYS").contains("CLAUDE_MARK"));
        assert!(!bundle.assemble("SYS").contains("ASK_MARK"));
    }

    #[test]
    fn global_falls_through_when_earlier_candidates_missing() {
        let home = Scratch::new("global-fall");
        write(home.path(), ".config/ask/AGENTS.md", "ASK_MARK");
        let cwd = Scratch::new("global-fall-cwd");
        git_repo(cwd.path());
        let bundle = ContextBundle::load(cwd.path(), home.path(), MAX_FILE_CHARS);
        assert_eq!(bundle.files.len(), 1);
        assert_eq!(bundle.files[0].body, "ASK_MARK");
    }

    #[test]
    fn local_walks_up_to_git_root_outermost_first() {
        let root = Scratch::new("walk");
        git_repo(root.path());
        write(root.path(), "AGENTS.md", "OUTER_MARK");
        write(root.path(), "sub/mid/AGENTS.md", "INNER_MARK");
        let home = Scratch::new("walk-home");
        let cwd = root.path().join("sub/mid");
        let bundle = ContextBundle::load(&cwd, home.path(), MAX_FILE_CHARS);
        let bodies: Vec<&str> = bundle.files.iter().map(|f| f.body.as_str()).collect();
        assert_eq!(bodies, ["OUTER_MARK", "INNER_MARK"]);
        assert!(bundle.files.iter().all(|f| f.scope == ContextScope::Local));
        let assembled = bundle.assemble("SYS");
        let outer = assembled.find("OUTER_MARK").unwrap();
        let inner = assembled.find("INNER_MARK").unwrap();
        assert!(outer < inner);
    }

    #[test]
    fn local_does_not_walk_past_git_root() {
        let tmp = Scratch::new("git-stop");
        write(tmp.path(), "outside/AGENTS.md", "OUTSIDE_MARK");
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        write(&repo, "AGENTS.md", "INREPO_MARK");
        let home = Scratch::new("git-stop-home");
        let bundle = ContextBundle::load(&repo, home.path(), MAX_FILE_CHARS);
        let assembled = bundle.assemble("SYS");
        assert!(assembled.contains("INREPO_MARK"));
        assert!(!assembled.contains("OUTSIDE_MARK"));
    }

    #[test]
    fn no_git_root_searches_cwd_only() {
        let tmp = Scratch::new("no-git");
        write(tmp.path(), "AGENTS.md", "PARENT_MARK");
        let cwd = tmp.path().join("leaf");
        fs::create_dir_all(&cwd).unwrap();
        write(&cwd, "AGENTS.md", "CWD_MARK");
        let home = Scratch::new("no-git-home");
        let bundle = ContextBundle::load(&cwd, home.path(), MAX_FILE_CHARS);
        assert_eq!(bundle.files.len(), 1);
        assert_eq!(bundle.files[0].body, "CWD_MARK");
    }

    #[test]
    fn local_claude_md_is_fallback_when_agents_md_absent() {
        let root = Scratch::new("claude-fb");
        git_repo(root.path());
        write(root.path(), "CLAUDE.md", "CLAUDE_LOCAL");
        write(root.path(), "src/AGENTS.md", "AGENTS_LOCAL");
        let home = Scratch::new("claude-fb-home");
        let bundle = ContextBundle::load(&root.path().join("src"), home.path(), MAX_FILE_CHARS);
        let bodies: Vec<&str> = bundle.files.iter().map(|f| f.body.as_str()).collect();
        assert_eq!(bodies, ["CLAUDE_LOCAL", "AGENTS_LOCAL"]);
    }

    #[test]
    fn agents_md_wins_over_claude_md_in_the_same_directory() {
        let root = Scratch::new("same-dir");
        git_repo(root.path());
        write(root.path(), "AGENTS.md", "AGENTS_WINS");
        write(root.path(), "CLAUDE.md", "CLAUDE_LOSES");
        let home = Scratch::new("same-dir-home");
        let bundle = ContextBundle::load(root.path(), home.path(), MAX_FILE_CHARS);
        assert_eq!(bundle.files.len(), 1);
        assert_eq!(bundle.files[0].body, "AGENTS_WINS");
    }

    #[test]
    fn missing_files_yield_empty_bundle() {
        let cwd = Scratch::new("missing-cwd");
        git_repo(cwd.path());
        let home = Scratch::new("missing-home");
        let bundle = ContextBundle::load(cwd.path(), home.path(), MAX_FILE_CHARS);
        assert!(bundle.files.is_empty());
        assert!(bundle.enabled);
        assert_eq!(bundle.cwd, cwd.path());
        assert_eq!(bundle.assemble("SYS_ONLY"), "SYS_ONLY");
    }

    #[test]
    fn truncation_caps_body_and_notes_it() {
        let root = Scratch::new("trunc");
        git_repo(root.path());
        write(root.path(), "AGENTS.md", "abcdefghijklmnopqrstuvwxyz");
        let home = Scratch::new("trunc-home");
        let bundle = ContextBundle::load(root.path(), home.path(), 8);
        assert_eq!(bundle.files.len(), 1);
        let f = &bundle.files[0];
        assert!(f.truncated);
        assert!(f.body.starts_with("abcdefgh"));
        assert!(f.body.contains("[truncated to 8 characters]"));
        assert_eq!(f.chars, f.body.chars().count());
        assert!(!f.body.contains("ijklmnopqrstuvwxyz"));
    }

    #[test]
    fn assembled_prompt_contains_both_sources_exactly_once() {
        let home = Scratch::new("both-home");
        write(home.path(), ".pi/agent/AGENTS.md", "GLOBAL_ONCE");
        let root = Scratch::new("both-cwd");
        git_repo(root.path());
        write(root.path(), "AGENTS.md", "LOCAL_ONCE");
        let bundle = ContextBundle::load(root.path(), home.path(), MAX_FILE_CHARS);
        let assembled = bundle.assemble("SYS_HEAD");
        assert_eq!(assembled.matches("GLOBAL_ONCE").count(), 1);
        assert_eq!(assembled.matches("LOCAL_ONCE").count(), 1);
        assert_eq!(assembled.matches("SYS_HEAD").count(), 1);
        let sys = assembled.find("SYS_HEAD").unwrap();
        let global = assembled.find("GLOBAL_ONCE").unwrap();
        let local = assembled.find("LOCAL_ONCE").unwrap();
        assert!(sys < global && global < local);
        assert!(assembled.contains("scope=\"global\""));
        assert!(assembled.contains("scope=\"local\""));
        let global_path = home.path().join(".pi/agent/AGENTS.md");
        let local_path = root.path().join("AGENTS.md");
        assert!(assembled.contains(&global_path.display().to_string()));
        assert!(assembled.contains(&local_path.display().to_string()));
    }

    #[test]
    fn disabled_bundle_does_not_inject_files() {
        let root = Scratch::new("off");
        git_repo(root.path());
        write(root.path(), "AGENTS.md", "SHOULD_NOT_APPEAR");
        let home = Scratch::new("off-home");
        let mut bundle = ContextBundle::load(root.path(), home.path(), MAX_FILE_CHARS);
        assert!(!bundle.files.is_empty());
        bundle.enabled = false;
        assert_eq!(bundle.assemble("SYS"), "SYS");
    }

    #[test]
    fn skips_binary_and_directory_without_panic() {
        let root = Scratch::new("skip");
        git_repo(root.path());
        fs::write(root.path().join("AGENTS.md"), b"hello\0world").unwrap();
        let nested = root.path().join("sub");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(nested.join("AGENTS.md")).unwrap();
        let home = Scratch::new("skip-home");
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(home.path().join(".claude/CLAUDE.md"), b"bin\0ary").unwrap();
        let bundle = ContextBundle::load(&nested, home.path(), MAX_FILE_CHARS);
        assert!(bundle.files.is_empty());
        let _ = bundle.assemble("SYS");
    }
}
