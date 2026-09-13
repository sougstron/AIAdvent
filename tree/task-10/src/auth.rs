//! Personal, per-machine API keys: the credential store, the resolver, and
//! the live key checks behind "login". Keys never ship with the app — they
//! are picked up from a provider env var or live in `~/.ask6/auth.json`
//! (0600, written atomically). Nothing else is read: files left behind by
//! other local tools are deliberately ignored. A key counts as *connected*
//! only after the provider answered with data derived from that key
//! (`CheckResult::Confirmed`); anything less is either rejected (not saved)
//! or saved `unverified`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::api::{DEFAULT_BASE_URL, LIVE_COMPLETION_MODEL};
use crate::config::Res;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    /// z.ai, plain OpenAI-compatible API (not the coding-plan endpoint).
    Glm,
    DeepSeek,
    OpenRouter,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Glm, Provider::DeepSeek, Provider::OpenRouter];

    pub fn id(self) -> &'static str {
        match self {
            Provider::Glm => "glm",
            Provider::DeepSeek => "deepseek",
            Provider::OpenRouter => "openrouter",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Glm => "GLM (z.ai)",
            Provider::DeepSeek => "DeepSeek",
            Provider::OpenRouter => "OpenRouter",
        }
    }

    pub fn parse(s: &str) -> Res<Provider> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "glm" | "zai" | "z.ai" => Ok(Provider::Glm),
            "deepseek" => Ok(Provider::DeepSeek),
            "openrouter" => Ok(Provider::OpenRouter),
            other => Err(format!(
                "unknown provider `{other}`; supported: {}",
                Provider::ALL.iter().map(|p| p.id()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    /// Chat base for completions. DeepSeek's balance endpoint is NOT under
    /// this path — it lives on the domain root (see [`check`]).
    pub fn default_base_url(self) -> &'static str {
        match self {
            Provider::Glm => DEFAULT_BASE_URL,
            Provider::DeepSeek => "https://api.deepseek.com/v1",
            Provider::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }

    /// Env var that overrides files. Kept first in the resolution order so
    /// CI and one-off runs can override without touching the store.
    pub fn env_var(self) -> &'static str {
        match self {
            Provider::Glm => "ZAI_API_KEY",
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::OpenRouter => "OPENROUTER_API_KEY",
        }
    }

}

/// One stored provider credential. `base_url` overrides the chat base for
/// this provider when set (unset: [`Provider::default_base_url`]).
#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub base_url: Option<String>,
    pub added_at: u64,
    pub last_check: Option<CheckRecord>,
}

/// Result of the most recent live check of an entry, whatever it was.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRecord {
    pub at: u64,
    pub verdict: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub providers: BTreeMap<String, Entry>,
}

/// Manual `Debug`: the key is physically unable to reach a log line or an
/// error message in any form but [`mask`].
impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("key", &mask(&self.key))
            .field("base_url", &self.base_url)
            .field("added_at", &self.added_at)
            .field("last_check", &self.last_check)
            .finish()
    }
}

/// Where a resolved key came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Env,
    Store(PathBuf),
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Source::Env => "env".to_string(),
            Source::Store(p) => format!("store {}", p.display()),
        }
    }
}

/// A key plus the endpoint facts needed to talk to its provider.
#[derive(Clone)]
pub struct Resolved {
    pub key: String,
    pub base_url: String,
    pub source: Source,
}

impl fmt::Debug for Resolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolved")
            .field("key", &mask(&self.key))
            .field("base_url", &self.base_url)
            .field("source", &self.source)
            .finish()
    }
}

/// `~/.ask6/auth.json`, overridable with `$ASK_AUTH_FILE` (tests, sandboxes).
pub fn auth_path() -> PathBuf {
    auth_path_from(std::env::var("ASK_AUTH_FILE").ok(), std::env::var("HOME").ok())
}

pub fn auth_path_from(override_file: Option<String>, home: Option<String>) -> PathBuf {
    if let Some(f) = override_file.filter(|s| !s.is_empty()) {
        return PathBuf::from(f);
    }
    PathBuf::from(home.unwrap_or_else(|| ".".into()))
        .join(".ask6")
        .join("auth.json")
}

/// Reads the store. Missing file = empty store (not an error); a corrupt
/// file is a hard error naming the path — silently ignoring it would hide
/// the user's keys.
pub fn load_from(path: &Path) -> Res<Credentials> {
    if !path.exists() {
        return Ok(Credentials::default());
    }
    warn_if_loose(path);
    let raw =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

/// Writes the store atomically: `auth.json.tmp` is created with mode 0600
/// *before* the key touches disk, then renamed over the target. The parent
/// directory is created 0700.
pub fn save_to(path: &Path, creds: &Credentials) -> Res<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let body = serde_json::to_string_pretty(creds)
        .map_err(|e| format!("cannot serialize credentials: {e}"))?;
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("cannot open {}: {e}", tmp.to_string_lossy()))?;
        f.write_all(body.as_bytes())
            .and_then(|_| f.flush())
            .map_err(|e| format!("cannot write {}: {e}", tmp.to_string_lossy()))?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot rename {} over {}: {e}", tmp.to_string_lossy(), path.display())
    })
}

/// Wider than 0600 on a key file means group/other readable — warn loudly
/// (the file may have been created by another tool), but do not refuse.
fn warn_if_loose(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                eprintln!(
                    "warning: {} is mode {:o}, expected 0600 — other users on this machine may read it",
                    path.display(),
                    mode
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Resolution order, never printing the key:
/// 1. `$<PROVIDER>_API_KEY`
/// 2. `~/.ask6/auth.json` → `providers.<id>.key` (the "login" store)
///
/// Nothing else is read. Files left behind by other local tools
/// (`~/.pi/…`, `~/.omp/…`, opencode) are deliberately ignored — keys reach
/// this app only through login or an env var.
pub fn resolve(provider: Provider) -> Res<Resolved> {
    let env = std::env::var(provider.env_var()).ok();
    match resolve_from(provider, env.as_deref(), Some(&auth_path())) {
        Ok(r) => Ok(r),
        Err(e) => match other_tool_hint(provider, std::env::var("HOME").ok().as_deref()) {
            Some(hint) => Err(format!("{e}\n{hint}")),
            None => Err(e),
        },
    }
}

pub fn resolve_from(
    provider: Provider,
    env_key: Option<&str>,
    store_path: Option<&Path>,
) -> Res<Resolved> {
    if let Some(key) = env_key.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(Resolved {
            key: key.to_string(),
            base_url: provider.default_base_url().into(),
            source: Source::Env,
        });
    }
    if let Some(sp) = store_path {
        let creds = load_from(sp)?;
        if let Some(entry) = creds.providers.get(provider.id()) {
            let key = entry.key.trim();
            if !key.is_empty() {
                return Ok(Resolved {
                    key: key.to_string(),
                    base_url: entry
                        .base_url
                        .clone()
                        .filter(|u| !u.trim().is_empty())
                        .unwrap_or_else(|| provider.default_base_url().into()),
                    source: Source::Store(sp.to_path_buf()),
                });
            }
        }
    }
    Err(missing_key_message(provider))
}

fn missing_key_message(provider: Provider) -> String {
    format!(
        "no {} API key found. Connect one: `ask --login {}` — the key is checked \
live and stored in ~/.ask6/auth.json (0600, per machine). Override for CI: ${}.",
        provider.label(),
        provider.id(),
        provider.env_var()
    )
}

/// Files where other local tools used to keep keys this app once read. They
/// are no longer read; the check is existence-only, so no key material is
/// ever touched — the returned line just points at the migration path.
pub const RETIRED_LEGACY_FILES: &[&str] = &[
    ".pi/agent/auth.json",
    ".omp/agent/auth.json",
    ".local/share/opencode/auth.json",
];

/// One-line migration note for the error message when a provider has no key
/// but a retired other-tool file still exists. `None` when there is nothing
/// to mention. Never opens or parses the file.
fn other_tool_hint(provider: Provider, home: Option<&str>) -> Option<String> {
    let home = home?;
    let home = Path::new(home);
    RETIRED_LEGACY_FILES
        .iter()
        .any(|rel| home.join(rel).exists())
        .then(|| {
            format!(
                "note: a key from another tool's file is no longer read — \
run `ask --login {}` once to store it here",
                provider.id()
            )
        })
}

/// Providers whose key resolves right now (env var or the login store),
/// in [`Provider::ALL`] order. Availability means "a key exists" — not
/// "the provider last confirmed it": an env-only key has no check history
/// and an unreachable provider must not make the picker lie. `--verify-login`
/// is the truth-teller on top of this.
pub fn connected_providers() -> Vec<Provider> {
    Provider::ALL
        .iter()
        .copied()
        .filter(|&p| resolve(p).is_ok())
        .collect()
}

/// `sk-or…9f2c (len 73)` — scheme prefix + last 4 chars only. Short keys are
/// fully withheld.
pub fn mask(key: &str) -> String {
    let n = key.chars().count();
    if n == 0 {
        return "(no key)".into();
    }
    if n < 12 {
        return format!("(key of {n} chars)");
    }
    let head: String = key.chars().take(5).collect();
    let tail: String = key.chars().skip(n - 4).collect();
    format!("{head}…{tail} (len {n})")
}

/// Outcome of a live provider check. `Confirmed` means the provider returned
/// data derived from the key — the only basis for saying "connected".
#[derive(Clone, PartialEq, Eq)]
pub enum CheckResult {
    Confirmed { evidence: String },
    Rejected { http: u16, msg: String },
    Unreachable { msg: String },
}

impl CheckResult {
    pub fn verdict(&self) -> &'static str {
        match self {
            CheckResult::Confirmed { .. } => "confirmed",
            CheckResult::Rejected { .. } => "rejected",
            CheckResult::Unreachable { .. } => "unreachable",
        }
    }

    pub fn evidence(&self) -> String {
        match self {
            CheckResult::Confirmed { evidence } => evidence.clone(),
            CheckResult::Rejected { msg, .. } | CheckResult::Unreachable { msg } => msg.clone(),
        }
    }
}

impl fmt::Debug for CheckResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // All fields are provider-derived text, never the key itself; still
        // routed through Display-ish formatting to keep this the one place.
        match self {
            CheckResult::Confirmed { evidence } => write!(f, "Confirmed {{ evidence: {evidence:?} }}"),
            CheckResult::Rejected { http, msg } => write!(f, "Rejected {{ http: {http}, msg: {msg:?} }}"),
            CheckResult::Unreachable { msg } => write!(f, "Unreachable {{ msg: {msg:?} }}"),
        }
    }
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent("ask-login/0.4")
        .build()
}

fn bearer(key: &str) -> String {
    format!("Bearer {}", key.trim())
}

/// Live check of `key` against `provider`'s free, key-derived endpoint:
/// - openrouter: `GET /api/v1/key` (label/limit/usage of that very key),
/// - deepseek:   `GET /user/balance` — note: domain root, not under `/v1`,
/// - glm:        z.ai has no free balance endpoint, so the cheapest
///   key-derived probe is the same call chat uses: `POST /chat/completions`
///   with `max_tokens: 1` (a fraction of a cent).
pub fn check(provider: Provider, key: &str) -> CheckResult {
    let agent = http_agent();
    let result = match provider {
        Provider::OpenRouter => agent
            .get("https://openrouter.ai/api/v1/key")
            .set("Authorization", &bearer(key))
            .call(),
        Provider::DeepSeek => agent
            .get("https://api.deepseek.com/user/balance")
            .set("Authorization", &bearer(key))
            .call(),
        Provider::Glm => {
            let url = format!("{}/chat/completions", provider.default_base_url());
            agent
                .post(&url)
                .set("Authorization", &bearer(key))
                .send_json(json!({
                    "model": LIVE_COMPLETION_MODEL,
                    "messages": [{"role": "user", "content": "ping"}],
                    "max_tokens": 1,
                }))
        }
    };
    match result {
        Ok(resp) => classify(provider, resp.status(), &read_body(resp)),
        Err(ureq::Error::Status(code, resp)) => classify(provider, code, &read_body(resp)),
        Err(e) => CheckResult::Unreachable { msg: format!("network error: {e}") },
    }
}

fn read_body(resp: ureq::Response) -> String {
    resp.into_string().unwrap_or_default()
}

/// Pure classifier — the network-dependent half of [`check`] lives above so
/// this can be tested offline against canned bodies.
pub fn classify(provider: Provider, status: u16, body: &str) -> CheckResult {
    match status {
        401 | 403 => CheckResult::Rejected { http: status, msg: format!("{} rejected the key{}", provider.label(), snippet(body)) },
        429 | 500..=599 => CheckResult::Unreachable { msg: format!("HTTP {status} from {}{}", provider.label(), snippet(body)) },
        200..=299 => confirmed_or_unverifiable(provider, body),
        _ => CheckResult::Rejected { http: status, msg: format!("unexpected HTTP {status} from {}{}", provider.label(), snippet(body)) },
    }
}

fn confirmed_or_unverifiable(provider: Provider, body: &str) -> CheckResult {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return CheckResult::Unreachable { msg: "HTTP 200 but the body was not JSON".into() },
    };
    let evidence = match provider {
        // Only key-derived fields count: usage and the echoed model id.
        Provider::Glm => {
            let model = v.get("model").and_then(Value::as_str);
            let prompt = v.pointer("/usage/prompt_tokens").and_then(Value::as_u64);
            match (model, prompt) {
                (Some(m), Some(n)) => Some(format!("chat/completions 200: model={m}, prompt_tokens={n}")),
                _ => None,
            }
        }
        Provider::DeepSeek => {
            let info = v.pointer("/balance_infos/0");
            match info.and_then(|i| i.get("total_balance")).and_then(Value::as_str) {
                Some(balance) => {
                    let currency = info.and_then(|i| i.get("currency")).and_then(Value::as_str).unwrap_or("?");
                    Some(format!("balance 200: {balance} {currency}"))
                }
                None => None,
            }
        }
        Provider::OpenRouter => {
            let data = v.get("data");
            let label = data.and_then(|d| d.get("label")).and_then(Value::as_str).unwrap_or("?");
            let usage = data.and_then(|d| d.get("usage"));
            let limit = data.and_then(|d| d.get("limit"));
            if data.is_some() && (usage.is_some() || limit.is_some()) {
                Some(format!(
                    "key 200: label={label}, usage={}, limit={}",
                    short_json(usage),
                    short_json(limit)
                ))
            } else {
                None
            }
        }
    };
    match evidence {
        Some(e) => CheckResult::Confirmed { evidence: e },
        None => CheckResult::Unreachable {
            msg: "HTTP 200 but no key-derived data in the response".into(),
        },
    }
}
/// Short provider-quote for messages: a JSON `error.message` when present,
/// else the head of the body. Provider responses never contain the key.
fn snippet(body: &str) -> String {
    let quoted = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let text = quoted.unwrap_or_else(|| body.trim().to_string());
    if text.is_empty() {
        return String::new();
    }
    let mut s: String = text.chars().take(120).collect();
    if text.chars().count() > 120 {
        s.push('…');
    }
    format!(": {s}")
}

fn short_json(v: Option<&Value>) -> String {
    match v {
        None => "?".into(),
        Some(Value::Null) => "null".into(),
        Some(other) => {
            let s = other.to_string();
            let mut out: String = s.chars().take(40).collect();
            if s.chars().count() > 40 {
                out.push('…');
            }
            out
        }
    }
}

/// Connect: check live, save on Confirmed/Unreachable, refuse on Rejected.
/// Returns `(verdict, saved)`. Unreachable keys are saved with an explicit
/// `unreachable` record — "connected" is claimed only for Confirmed.
pub fn connect(provider: Provider, key: &str) -> Res<(CheckResult, bool)> {
    let key = key.trim();
    if key.is_empty() {
        return Err("empty key — nothing to connect".into());
    }
    let verdict = check(provider, key);
    if matches!(verdict, CheckResult::Rejected { .. }) {
        return Ok((verdict, false));
    }
    let path = auth_path();
    let mut creds = load_from(&path)?;
    creds.version = 1;
    creds.providers.insert(
        provider.id().to_string(),
        Entry {
            key: key.to_string(),
            base_url: None,
            added_at: now_secs(),
            last_check: Some(CheckRecord {
                at: now_secs(),
                verdict: verdict.verdict().into(),
                evidence: verdict.evidence(),
            }),
        },
    );
    save_to(&path, &creds)?;
    Ok((verdict, true))
}

/// Removes the provider's key from the local store. An env var is outside
/// its reach — callers say so.
pub fn disconnect(provider: Provider) -> Res<bool> {
    let path = auth_path();
    let mut creds = load_from(&path)?;
    if creds.providers.remove(provider.id()).is_none() {
        return Ok(false);
    }
    save_to(&path, &creds)?;
    Ok(true)
}

/// Persists a fresh check verdict on an existing store entry. Returns false
/// when the key resolves from env — nothing is copied into the store
/// behind the user's back.
pub fn record_check(provider: Provider, verdict: &CheckResult) -> Res<bool> {
    let path = auth_path();
    let mut creds = load_from(&path)?;
    let Some(entry) = creds.providers.get_mut(provider.id()) else {
        return Ok(false);
    };
    entry.last_check = Some(CheckRecord {
        at: now_secs(),
        verdict: verdict.verdict().into(),
        evidence: verdict.evidence(),
    });
    save_to(&path, &creds)?;
    Ok(true)
}

/// Re-resolve and live-check a provider's configured key, recording the
/// verdict when it comes from the store.
pub fn recheck(provider: Provider) -> Res<CheckResult> {
    let resolved = resolve(provider)?;
    let verdict = check(provider, &resolved.key);
    record_check(provider, &verdict)?;
    Ok(verdict)
}

/// One row of `--keys` / the TUI `/login` panel. `source`/`masked` are set
/// when a key resolves from anywhere; `last_check` only when it resolves
/// from the store (history exists iff the key is in the store).
#[derive(Clone)]
pub struct ProviderStatus {
    pub provider: Provider,
    pub source: Option<Source>,
    pub masked: Option<String>,
    pub last_check: Option<CheckRecord>,
}

impl ProviderStatus {
    pub fn connected(&self) -> bool {
        self.source.is_some()
    }
}

pub fn status_all() -> Vec<ProviderStatus> {
    let path = auth_path();
    let creds = load_from(&path).unwrap_or_default();
    Provider::ALL
        .iter()
        .map(|&p| {
            let stored = creds.providers.get(p.id()).and_then(|e| e.last_check.clone());
            match resolve(p) {
                Ok(r) => ProviderStatus {
                    provider: p,
                    source: Some(r.source),
                    masked: Some(mask(&r.key)),
                    last_check: stored,
                },
                Err(_) => ProviderStatus { provider: p, source: None, masked: None, last_check: stored },
            }
        })
        .collect()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ask6-auth-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn write_file(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn store_with(path: &Path, id: &str, key: &str) {
        let mut creds = Credentials { version: 1, ..Default::default() };
        creds.providers.insert(
            id.to_string(),
            Entry { key: key.into(), base_url: None, added_at: 0, last_check: None },
        );
        save_to(path, &creds).unwrap();
    }

    // --- resolution order: env → store, and only that ---

    #[test]
    fn env_wins_over_store_for_every_provider() {
        let dir = tmp("env-wins");
        let store = dir.join("store.json");
        for p in Provider::ALL {
            store_with(&store, p.id(), "from-store");
            let r = resolve_from(p, Some("from-env"), Some(&store)).unwrap();
            assert_eq!(r.key, "from-env");
            assert_eq!(r.source, Source::Env);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn store_is_used_when_env_is_unset() {
        let dir = tmp("store-only");
        let store = dir.join("store.json");
        for p in Provider::ALL {
            store_with(&store, p.id(), "from-store");
            let r = resolve_from(p, None, Some(&store)).unwrap();
            assert_eq!(r.key, "from-store");
            assert_eq!(r.source, Source::Store(store.clone()));
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The user's literal ask: keys left in other tools' files must never be
    /// picked up again. All three retired files hold valid-looking keys and
    /// the resolver still refuses to touch them.
    #[test]
    fn no_key_is_read_from_other_tools_files() {
        let dir = tmp("retired-files");
        let home = dir.join("home");
        write_file(&home.join(".pi/agent/auth.json"), r#"{"zai-coding-cn":{"key":"glm-from-pi"},"deepseek":{"key":"ds-from-pi"},"openrouter":{"key":"or-from-pi"}}"#);
        write_file(&home.join(".omp/agent/auth.json"), r#"{"zai-coding-cn":{"key":"glm-from-omp"}}"#);
        write_file(
            &home.join(".local/share/opencode/auth.json"),
            r#"{"openrouter":{"key":"or-from-opencode"}}"#,
        );
        for p in Provider::ALL {
            let err = resolve_from(p, None, None).unwrap_err();
            assert!(err.contains(&format!("no {} API key", p.label())), "{err}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The hint exists so a pi-only user is told *why* they went keyless and
    /// what to do — without the app ever reading the file's contents.
    #[test]
    fn missing_key_error_names_login_and_store() {
        let dir = tmp("missing");
        let err = resolve_from(Provider::Glm, None, Some(&dir.join("no.json"))).unwrap_err();
        assert!(err.contains("--login glm"));
        assert!(err.contains("~/.ask6/auth.json"));
        assert!(err.contains("ZAI_API_KEY"));
        assert!(!err.contains(".pi/"));
        let err = resolve_from(Provider::OpenRouter, None, None).unwrap_err();
        assert!(err.contains("--login openrouter"));
        assert!(err.contains("OPENROUTER_API_KEY"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_tool_hint_fires_on_file_existence_only() {
        let dir = tmp("hint");
        let home = dir.join("home");
        assert!(other_tool_hint(Provider::Glm, None).is_none());
        assert!(other_tool_hint(Provider::Glm, Some(dir.to_str().unwrap())).is_none());
        // A file nobody reads any more, but it exists → the hint names login.
        write_file(&home.join(".pi/agent/auth.json"), "whatever");
        let hint = other_tool_hint(Provider::DeepSeek, Some(home.to_str().unwrap())).unwrap();
        assert!(hint.contains("--login deepseek"), "{hint}");
        // No parsing: a corrupt file still only produces the hint text.
        write_file(&home.join(".omp/agent/auth.json"), "{not json");
        let hint = other_tool_hint(Provider::Glm, Some(home.to_str().unwrap())).unwrap();
        assert!(hint.contains("--login glm"), "{hint}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_env_key_is_ignored() {
        let dir = tmp("empty-env");
        let store = dir.join("store.json");
        store_with(&store, "glm", "from-store");
        let r = resolve_from(Provider::Glm, Some("   "), Some(&store)).unwrap();
        assert_eq!(r.key, "from-store");
        fs::remove_dir_all(&dir).unwrap();
    }

    // --- store round-trip, permissions, atomicity ---

    #[test]
    fn store_round_trip_and_corrupt_file_error() {
        let dir = tmp("roundtrip");
        let path = dir.join("auth.json");
        let mut creds = Credentials { version: 1, ..Default::default() };
        creds.providers.insert(
            "openrouter".into(),
            Entry {
                key: "sk-or-0000000099".into(),
                base_url: None,
                added_at: 42,
                last_check: Some(CheckRecord {
                    at: 43,
                    verdict: "confirmed".into(),
                    evidence: "key 200".into(),
                }),
            },
        );
        save_to(&path, &creds).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.providers["openrouter"].key, "sk-or-0000000099");
        assert_eq!(loaded.providers["openrouter"].last_check.as_ref().unwrap().verdict, "confirmed");

        fs::write(&path, "{{{").unwrap();
        let err = load_from(&path).unwrap_err();
        assert!(err.contains("cannot parse"));
        assert!(err.contains(path.display().to_string().as_str()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_enforces_0700_dir_0600_file_and_leaves_no_tmp() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("perms");
        let path = dir.join("nested").join("auth.json");
        let mut creds = Credentials::default();
        creds.providers.insert(
            "glm".into(),
            Entry { key: "k".into(), base_url: None, added_at: 0, last_check: None },
        );
        save_to(&path, &creds).unwrap();
        let dir_mode = fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        assert!(!Path::new(&tmp).exists(), ".tmp leftover after save");
        fs::remove_dir_all(&dir).unwrap();
    }

    // --- masking ---

    #[test]
    fn mask_hides_everything_but_prefix_and_tail() {
        // Assembled at runtime so the hardcoded-key guard above stays honest.
        let key = format!("sk-or-{}", "a".repeat(41));
        let m = mask(&key);
        assert_eq!(m, "sk-or…aaaa (len 47)");
        assert!(!m.contains("aaaaa"));
        // A short key shows nothing of itself.
        assert_eq!(mask("short"), "(key of 5 chars)");
        assert_eq!(mask(""), "(no key)");
    }

    #[test]
    fn debug_impls_are_masked() {
        let key = format!("sk-or-{}", "b".repeat(41));
        let entry = Entry {
            key: key.clone(),
            base_url: None,
            added_at: 0,
            last_check: None,
        };
        let dbg = format!("{entry:?}");
        assert!(dbg.contains("sk-or…bbbb (len 47)"), "{dbg}");
        assert!(!dbg.contains("bbbbbbbbbb"));
        let resolved = Resolved {
            key,
            base_url: "https://x".into(),
            source: Source::Env,
        };
        let dbg = format!("{resolved:?}");
        assert!(dbg.contains("sk-or…bbbb (len 47)"), "{dbg}");
        assert!(!dbg.contains("bbbbbbbbbb"));
    }

    // --- classify: offline verdicts on canned provider bodies ---

    const GLM_OK: &str = r#"{"model":"glm-5.3-flash","usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;
    const DS_OK: &str = r#"{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"92.51"}]}"#;
    const OR_OK: &str = r#"{"data":{"label":"dev","usage":1.25,"limit":20.0,"is_free_tier":false}}"#;

    #[test]
    fn classify_confirms_only_on_key_derived_evidence() {
        match classify(Provider::Glm, 200, GLM_OK) {
            CheckResult::Confirmed { evidence } => {
                assert!(evidence.contains("glm-5.3-flash"));
                assert!(evidence.contains("prompt_tokens=3"));
            }
            other => panic!("glm should confirm, got {other:?}"),
        }
        match classify(Provider::DeepSeek, 200, DS_OK) {
            CheckResult::Confirmed { evidence } => assert!(evidence.contains("92.51 CNY")),
            other => panic!("deepseek should confirm, got {other:?}"),
        }
        match classify(Provider::OpenRouter, 200, OR_OK) {
            CheckResult::Confirmed { evidence } => {
                assert!(evidence.contains("label=dev"));
                assert!(evidence.contains("limit=20.0"));
            }
            other => panic!("openrouter should confirm, got {other:?}"),
        }
    }

    #[test]
    fn classify_200_without_evidence_is_not_confirmed() {
        for (p, body) in [
            (Provider::Glm, r#"{"object":"list","data":[]}"#), // e.g. a public /models answer
            (Provider::DeepSeek, r#"{}"#),
            (Provider::OpenRouter, r#"{"data":{"label":"x"}}"#), // no usage/limit
        ] {
            match classify(p, 200, body) {
                CheckResult::Unreachable { msg } => assert!(msg.contains("no key-derived data")),
                other => panic!("{:?} 200 without evidence must not confirm: {other:?}", p.id()),
            }
        }
        // And a non-JSON 200 body is the same story, not a confirmation.
        assert!(matches!(
            classify(Provider::Glm, 200, "<html>ok</html>"),
            CheckResult::Unreachable { .. }
        ));
    }

    #[test]
    fn classify_rejects_and_unreachables() {
        for p in Provider::ALL {
            match classify(p, 401, r#"{"error":{"message":"Invalid API key"}}"#) {
                CheckResult::Rejected { http, msg } => {
                    assert_eq!(http, 401);
                    assert!(msg.contains("Invalid API key"), "{msg}");
                }
                other => panic!("{:?} 401 must reject: {other:?}", p.id()),
            }
            assert!(matches!(classify(p, 403, ""), CheckResult::Rejected { .. }));
            assert!(matches!(classify(p, 502, ""), CheckResult::Unreachable { .. }));
            assert!(matches!(classify(p, 429, ""), CheckResult::Unreachable { .. }));
            match classify(p, 400, r#"{"error":{"message":"bad model"}}"#) {
                CheckResult::Rejected { http, msg } => {
                    assert_eq!(http, 400);
                    assert!(msg.contains("bad model"));
                }
                other => panic!("{:?} 400 must reject loudly: {other:?}", p.id()),
            }
        }
    }

    // --- the user's literal ask: keys are not baked into the app ---

    #[test]
    fn no_api_key_literals_are_baked_into_sources() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0;
        for entry in fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            for (lineno, line) in text.lines().enumerate() {
                let mut from = 0;
                while let Some(pos) = line[from..].find("sk-") {
                    let start = from + pos + 3;
                    let run = line[start..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                        .count();
                    assert!(
                        run < 20,
                        "{}:{} looks like a hardcoded API key ({} chars after `sk-`)",
                        path.display(),
                        lineno + 1,
                        run
                    );
                    from = start;
                }
            }
            checked += 1;
        }
        assert!(checked >= 8, "expected to scan the real src/ tree, saw {checked} files");
    }
}
