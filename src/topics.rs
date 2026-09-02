//! Topic registry: each topic is a service the CLI pretends to be, and carries its
//! own schema plus prompt wording. Adding a topic is a data change, not a code change.

use serde_json::Value;
use std::fs;

use crate::config::{Format, Res, RunConfig};
use crate::news::NewsItem;

/// The separator the model is told to put between entries in text mode. It exists so
/// `--stop` has a deterministic, meaningful thing to cut on.
pub const ITEM_SEPARATOR: &str = "\n---\n";

pub struct Topic {
    pub name: &'static str,
    pub title: &'static str,
    pub system_prompt: &'static str,
    schema_json: &'static str,
    /// JSON pointer to the array that `--max-items` should cap.
    list_pointer: &'static str,
}

pub const TOPICS: &[Topic] = &[Topic {
    name: "gamedev",
    title: "Game-development news digest",
    system_prompt: concat!(
        "You are the summarisation backend of a game-development news service. ",
        "You receive raw news entries fetched from public APIs and turn them into a digest. ",
        "Use only the material given to you: never invent games, studios or dates. ",
        "If a field is not derivable from the material, use \"unknown\" — except that ",
        "anything sourced from Steam is by definition available on pc. ",
        "hype_score is a 1-10 rating of how much the entry matters to players. ",
        "Be concise: one or two sentences per entry."
    ),
    schema_json: include_str!("../schemas/gamedev.json"),
    list_pointer: "/properties/items",
}];

pub fn get(name: &str) -> Res<&'static Topic> {
    TOPICS.iter().find(|t| t.name == name).ok_or_else(|| {
        format!(
            "unknown topic `{name}` (known: {}, or `none`)",
            names().join(", ")
        )
    })
}

pub fn names() -> Vec<&'static str> {
    TOPICS.iter().map(|t| t.name).collect()
}

impl Topic {
    /// The schema actually sent to the provider, with `--max-items` folded in.
    pub fn schema(&self, cfg: &RunConfig) -> Res<Value> {
        let raw = match &cfg.schema_file {
            Some(path) => fs::read_to_string(path)
                .map_err(|e| format!("cannot read schema file {path}: {e}"))?,
            None => self.schema_json.to_string(),
        };
        let mut schema: Value =
            serde_json::from_str(&raw).map_err(|e| format!("schema is not valid JSON: {e}"))?;

        if let Some(n) = cfg.max_items {
            // maxItems constrains the answer semantically: it cuts whole entries instead
            // of chopping the last one mid-word the way a token budget does.
            if let Some(list) = schema.pointer_mut(self.list_pointer) {
                if let Some(obj) = list.as_object_mut() {
                    obj.insert("maxItems".into(), Value::from(n));
                }
            }
        }
        Ok(schema)
    }

    /// Builds the user message: the fetched material plus format-specific instructions.
    pub fn user_prompt(&self, cfg: &RunConfig, items: &[NewsItem]) -> String {
        let mut p = String::new();

        let window = describe_window(cfg.since_hours);
        p.push_str(&format!(
            "Build a game-development news digest for the period: {window}.\n"
        ));
        p.push_str(&format!(
            "Current UTC time: {}.\n",
            crate::news::fmt_date(crate::news::now_unix())
        ));

        if !cfg.question.is_empty() {
            p.push_str(&format!("Extra focus from the user: {}\n", cfg.question));
        }

        if items.is_empty() {
            match cfg.source {
                crate::config::Source::None => p.push_str(
                    "\nNo material was fetched (news source disabled). \
                     Produce a plausible digest of 5 well-known industry items instead.\n",
                ),
                _ => p.push_str(
                    "\nNo recent items in the selected time window. \
                     Produce a plausible digest of 5 well-known industry items instead.\n",
                ),
            }
        } else {
            p.push_str(&format!(
                "\nSource material — {} entries fetched from public APIs:\n\n",
                items.len()
            ));
            for it in items {
                p.push_str(&it.as_prompt_line());
                p.push('\n');
            }
        }

        if let Some(n) = cfg.max_items {
            p.push_str(&format!("\nInclude at most {n} entries.\n"));
        }

        match cfg.format {
            // Text mode gets an explicit separator so a stop sequence has a real target.
            Format::Text => {
                p.push_str(
                    "\nWrite the digest as plain text. First line: a one-line headline. \
                     Then one block per news entry. Put this delimiter *between entries* \
                     (never after the headline, never after the last entry):",
                );
                p.push_str(ITEM_SEPARATOR);
                p.push_str("Do not use JSON.");
            }
            // Deliberately no key list here: this mode exists to show keys drifting.
            Format::JsonObject => p.push_str(
                "\nAnswer with a single JSON object describing the digest. \
                 Output JSON only, no prose and no code fences.",
            ),
            // The schema is enforced by the API, so the prompt only needs the intent.
            Format::JsonSchema => {
                p.push_str(
                    "\nAnswer with the digest as a JSON object matching the required schema.",
                );
            }
        }
        p
    }
}

fn describe_window(hours: u64) -> String {
    if hours % 24 == 0 && hours >= 24 {
        format!("last {} days", hours / 24)
    } else {
        format!("last {hours} hours")
    }
}
