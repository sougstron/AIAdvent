//! The shared core: fetch material once, then generate + judge answers.
//! CLI, `--compare` and the TUI all go through here so they cannot drift apart.

use serde_json::Value;
use std::collections::BTreeSet;

use crate::api::{self, Endpoint, Outcome};
use crate::config::{Format, Res, RunConfig};
use crate::news::{self, NewsItem};
use crate::topics::{self, Topic};

/// One generation plus everything we measure about it.
#[derive(Clone, Debug)]
pub struct RunResult {
    pub outcome: Outcome,
    /// The answer parsed as JSON, when it parsed.
    pub json: Option<Value>,
    pub json_error: Option<String>,
    /// Empty when the answer satisfies the topic schema (or when no schema applies).
    pub schema_errors: Vec<String>,
    /// Sorted set of JSON paths — identical across runs means a stable shape.
    pub key_signature: Option<String>,
    /// Same, with the value type appended to each path.
    pub type_signature: Option<String>,
}

impl RunResult {
    pub fn json_ok(&self) -> bool {
        self.json.is_some()
    }

    pub fn schema_ok(&self) -> bool {
        self.json.is_some() && self.schema_errors.is_empty()
    }

    /// Number of entries in the topic's list, when the answer has one.
    pub fn item_count(&self) -> Option<usize> {
        self.json
            .as_ref()
            .and_then(|j| j.get("items"))
            .and_then(Value::as_array)
            .map(Vec::len)
    }
}

pub struct Engine {
    pub endpoint: Endpoint,
    /// `None` for the pass-through `--topic none` mode.
    pub topic: Option<&'static Topic>,
    pub news: Vec<NewsItem>,
    /// Human-readable note about what the fetch produced, shown in reports and the TUI.
    pub news_note: String,
}

impl Engine {
    /// Resolves the endpoint and fetches source material once, up front. Repeated runs
    /// then differ only in generation parameters — otherwise the comparison is unfair.
    pub fn new(cfg: &RunConfig) -> Res<Engine> {
        let endpoint = Endpoint::resolve()?;
        let topic = if cfg.topic == "none" {
            None
        } else {
            Some(topics::get(&cfg.topic)?)
        };

        let (news, news_note) = match (topic, cfg.source) {
            (None, _) | (_, crate::config::Source::None) => {
                (Vec::new(), "no fetch (model-generated content)".to_string())
            }
            (Some(_), source) => match news::fetch(source, cfg.since_hours, cfg.limit) {
                Ok(items) => {
                    let note = format!(
                        "{} items from {} over the last {}h",
                        items.len(),
                        source.label(),
                        cfg.since_hours
                    );
                    (items, note)
                }
                // A dead news API should degrade the demo, not kill it.
                Err(e) => (Vec::new(), format!("fetch failed ({e}); model-generated")),
            },
        };

        Ok(Engine {
            endpoint,
            topic,
            news,
            news_note,
        })
    }

    /// System + user messages for a given config, without calling the API.
    pub fn prompt(&self, cfg: &RunConfig) -> (String, String) {
        match self.topic {
            Some(t) => (t.system_prompt.to_string(), t.user_prompt(cfg, &self.news)),
            None => (String::new(), cfg.question.clone()),
        }
    }

    fn schema(&self, cfg: &RunConfig) -> Res<Option<Value>> {
        match self.topic {
            Some(t) if cfg.format == Format::JsonSchema => Ok(Some(t.schema(cfg)?)),
            _ => Ok(None),
        }
    }

    pub fn run_once(&self, cfg: &RunConfig) -> Res<RunResult> {
        let (system, user) = self.prompt(cfg);
        let schema = self.schema(cfg)?;
        let outcome = api::chat(&self.endpoint, cfg, &system, &user, schema.as_ref())?;
        Ok(self.judge(cfg, outcome))
    }

    /// Measures an answer: does it parse, does it match the schema, what shape is it.
    pub fn judge(&self, cfg: &RunConfig, outcome: Outcome) -> RunResult {
        let mut json = None;
        let mut json_error = None;

        if cfg.format.expects_json() {
            let text = outcome.text();
            if text.is_empty() {
                json_error = Some("empty content".into());
            } else {
                match serde_json::from_str::<Value>(strip_fences(text)) {
                    Ok(v) => json = Some(v),
                    Err(e) => json_error = Some(e.to_string()),
                }
            }
        }

        // Validate against the topic schema whenever we have one, even in json/text mode:
        // that is exactly what makes the comparison meaningful.
        let mut schema_errors = Vec::new();
        if let (Some(t), Some(v)) = (self.topic, json.as_ref()) {
            match t
                .schema(cfg)
                .and_then(|s| jsonschema::validator_for(&s).map_err(|e| format!("bad schema: {e}")))
            {
                Ok(validator) => {
                    schema_errors = validator
                        .iter_errors(v)
                        .take(5)
                        .map(|e| format!("{}: {}", e.instance_path(), e))
                        .collect();
                }
                Err(e) => schema_errors.push(e),
            }
        }

        let (key_signature, type_signature) = match json.as_ref() {
            Some(v) => (Some(signature(v, false)), Some(signature(v, true))),
            None => (None, None),
        };

        RunResult {
            outcome,
            json,
            json_error,
            schema_errors,
            key_signature,
            type_signature,
        }
    }
}

/// Models in `json_object` mode sometimes still wrap the answer in a code fence.
fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        return rest
            .trim_start_matches('\n')
            .trim_end()
            .trim_end_matches("```")
            .trim();
    }
    t
}

/// Shape fingerprint: every path in the document, array indices collapsed to `[]`,
/// so two answers with different item counts still count as the same shape.
pub fn signature(v: &Value, with_types: bool) -> String {
    let mut paths = BTreeSet::new();
    walk(v, "$", &mut paths, with_types);
    paths.into_iter().collect::<Vec<_>>().join("|")
}

fn walk(v: &Value, path: &str, out: &mut BTreeSet<String>, with_types: bool) {
    match v {
        Value::Object(map) => {
            out.insert(entry(path, "object", with_types));
            for (k, val) in map {
                walk(val, &format!("{path}.{k}"), out, with_types);
            }
        }
        Value::Array(items) => {
            out.insert(entry(path, "array", with_types));
            for item in items {
                walk(item, &format!("{path}[]"), out, with_types);
            }
        }
        other => {
            let ty = match other {
                Value::String(_) => "string",
                Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
                Value::Number(_) => "number",
                Value::Bool(_) => "boolean",
                _ => "null",
            };
            out.insert(entry(path, ty, with_types));
        }
    }
}

fn entry(path: &str, ty: &str, with_types: bool) -> String {
    if with_types {
        format!("{path}:{ty}")
    } else {
        path.to_string()
    }
}

/// Short, human-comparable form of a signature.
pub fn short_hash(s: &str) -> String {
    // FNV-1a: we only need "are these two the same", not cryptographic strength.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:06x}", hash & 0xff_ffff)
}
