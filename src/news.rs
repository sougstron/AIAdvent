//! Real-world material for the digest: Steam's news API and Hacker News.
//! Both are keyless and return JSON, so the CLI stays dependency-light.

use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{Res, Source};

/// Curated appids whose Steam news feeds are actual game-development traffic
/// (patch notes, roadmap posts, launch announcements).
const STEAM_APPS: &[(u32, &str)] = &[
    (730, "Counter-Strike 2"),
    (570, "Dota 2"),
    (440, "Team Fortress 2"),
    (1091500, "Cyberpunk 2077"),
    (892970, "Valheim"),
    (1245620, "Elden Ring"),
    (578080, "PUBG: Battlegrounds"),
    (252490, "Rust"),
    (322330, "Don't Starve Together"),
    (1172470, "Apex Legends"),
];

#[derive(Clone, Debug)]
pub struct NewsItem {
    pub source: String,
    /// Game or project the item belongs to; empty for generic industry news.
    pub subject: String,
    pub title: String,
    pub url: String,
    pub author: String,
    pub published: i64,
    pub summary: String,
}

impl NewsItem {
    /// Compact line fed to the model. Keeping it uniform keeps prompt tokens predictable.
    pub fn as_prompt_line(&self) -> String {
        let date = fmt_date(self.published);
        let subject = if self.subject.is_empty() {
            String::new()
        } else {
            format!(" | subject: {}", self.subject)
        };
        format!(
            "- [{}] {}{} | date: {} | author: {} | url: {}\n  {}",
            self.source,
            self.title,
            subject,
            date,
            if self.author.is_empty() {
                "n/a"
            } else {
                &self.author
            },
            self.url,
            self.summary
        )
    }
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn fmt_date(ts: i64) -> String {
    match chrono::DateTime::from_timestamp(ts, 0) {
        Some(dt) => dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        None => "unknown".into(),
    }
}

/// Fetches items newer than `since_hours`, newest first, capped at `limit`.
pub fn fetch(
    source: Source,
    since_hours: u64,
    limit: usize,
    query: Option<&str>,
) -> Res<Vec<NewsItem>> {
    let cutoff = now_unix() - (since_hours as i64) * 3600;
    let mut items = Vec::new();
    let mut errors = Vec::new();

    if matches!(source, Source::Steam | Source::Both) {
        match fetch_steam(cutoff) {
            Ok(mut v) => items.append(&mut v),
            Err(e) => errors.push(format!("steam: {e}")),
        }
    }
    if matches!(source, Source::Hn | Source::Both) {
        match fetch_hn(cutoff, query.unwrap_or("game development")) {
            Ok(mut v) => items.append(&mut v),
            Err(e) => errors.push(format!("hn: {e}")),
        }
    }

    if items.is_empty() && !errors.is_empty() {
        return Err(errors.join("; "));
    }
    if let Some(query) = query.filter(|q| !q.trim().is_empty()) {
        let terms: Vec<String> = query.split_whitespace().map(|s| s.to_lowercase()).collect();
        items.retain(|item| {
            if item.source != "steam" {
                return true; // HN already applied the query server-side.
            }
            let haystack =
                format!("{} {} {}", item.subject, item.title, item.summary).to_lowercase();
            terms.iter().any(|term| haystack.contains(term))
        });
    }
    items.sort_by(|a, b| b.published.cmp(&a.published));
    items.truncate(limit);
    Ok(items)
}

fn fetch_steam(cutoff: i64) -> Res<Vec<NewsItem>> {
    let mut out = Vec::new();
    let mut last_err = None;
    for (appid, game) in STEAM_APPS {
        let url = format!(
            "https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/\
             ?appid={appid}&count=5&maxlength=700&format=json"
        );
        let body: Value = match ureq::get(&url)
            .timeout(std::time::Duration::from_secs(20))
            .call()
        {
            Ok(r) => match r.into_json() {
                Ok(v) => v,
                Err(e) => {
                    last_err = Some(e.to_string());
                    continue;
                }
            },
            Err(e) => {
                last_err = Some(e.to_string());
                continue;
            }
        };
        let entries = body
            .get("appnews")
            .and_then(|a| a.get("newsitems"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for e in entries {
            let published = e.get("date").and_then(Value::as_i64).unwrap_or(0);
            if published < cutoff {
                continue;
            }
            out.push(NewsItem {
                source: "steam".into(),
                subject: (*game).into(),
                title: str_field(&e, "title"),
                url: str_field(&e, "url"),
                author: str_field(&e, "author"),
                published,
                summary: clean_markup(&str_field(&e, "contents")),
            });
        }
    }
    if out.is_empty() {
        if let Some(e) = last_err {
            return Err(e);
        }
    }
    Ok(out)
}

fn fetch_hn(cutoff: i64, query: &str) -> Res<Vec<NewsItem>> {
    let body: Value = ureq::get("https://hn.algolia.com/api/v1/search_by_date")
        .query("tags", "story")
        .query("hitsPerPage", "25")
        .query("numericFilters", &format!("created_at_i>{cutoff}"))
        .query("query", query)
        .timeout(std::time::Duration::from_secs(20))
        .call()
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;

    let hits = body
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for h in hits {
        let published = h.get("created_at_i").and_then(Value::as_i64).unwrap_or(0);
        if published < cutoff {
            continue;
        }
        let title = str_field(&h, "title");
        if title.is_empty() {
            continue;
        }
        let story_text = clean_markup(&str_field(&h, "story_text"));
        out.push(NewsItem {
            source: "hackernews".into(),
            subject: String::new(),
            title,
            url: str_field(&h, "url"),
            author: str_field(&h, "author"),
            published,
            summary: truncate(&story_text, 700),
        });
    }
    Ok(out)
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Steam posts are BBCode + HTML; HN carries HTML entities. Flatten both to plain text
/// so the prompt stays small and the model does not echo markup into the JSON.
fn clean_markup(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut depth_tag = false;
    for ch in input.chars() {
        match ch {
            '[' | '<' => depth_tag = true,
            ']' | '>' => depth_tag = false,
            _ if !depth_tag => out.push(ch),
            _ => {}
        }
    }
    let out = out
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace('\\', " ");

    // Collapse runs of whitespace into single spaces — multi-line BBCode wastes tokens.
    let collapsed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&collapsed, 700)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}
