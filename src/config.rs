//! The single knob-set shared by the CLI parser, the env vars and the TUI.

use std::fmt;

pub type Res<T> = Result<T, String>;

/// How hard we constrain the shape of the answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// No constraint at all — plain prose, whatever the model feels like.
    Text,
    /// `response_format: json_object` — valid JSON, but the keys are the model's choice.
    JsonObject,
    /// `response_format: json_schema` with `strict: true` — the shape is ours.
    JsonSchema,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Text, Format::JsonObject, Format::JsonSchema];

    pub fn label(self) -> &'static str {
        match self {
            Format::Text => "text",
            Format::JsonObject => "json",
            Format::JsonSchema => "schema",
        }
    }

    pub fn parse(s: &str) -> Res<Format> {
        match s {
            "text" => Ok(Format::Text),
            "json" | "json_object" => Ok(Format::JsonObject),
            "schema" | "json_schema" => Ok(Format::JsonSchema),
            other => Err(format!("unknown format `{other}` (text|json|schema)")),
        }
    }

    /// True when the answer is supposed to parse as JSON.
    pub fn expects_json(self) -> bool {
        !matches!(self, Format::Text)
    }

    pub fn cycle(self, delta: i32) -> Format {
        let i = Format::ALL.iter().position(|f| *f == self).unwrap_or(0) as i32;
        let n = Format::ALL.len() as i32;
        Format::ALL[(i + delta).rem_euclid(n) as usize]
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Where the real-world material for the digest comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Steam news API (`ISteamNews`), a curated list of appids.
    Steam,
    /// Hacker News via the Algolia search API.
    Hn,
    Both,
    /// No fetch — the model makes the content up. Useful to isolate format effects.
    None,
}

impl Source {
    pub const ALL: [Source; 4] = [Source::Steam, Source::Hn, Source::Both, Source::None];

    pub fn label(self) -> &'static str {
        match self {
            Source::Steam => "steam",
            Source::Hn => "hn",
            Source::Both => "both",
            Source::None => "none",
        }
    }

    pub fn parse(s: &str) -> Res<Source> {
        match s {
            "steam" => Ok(Source::Steam),
            "hn" => Ok(Source::Hn),
            "both" | "all" => Ok(Source::Both),
            "none" | "off" => Ok(Source::None),
            other => Err(format!("unknown source `{other}` (steam|hn|both|none)")),
        }
    }

    pub fn cycle(self, delta: i32) -> Source {
        let i = Source::ALL.iter().position(|s| *s == self).unwrap_or(0) as i32;
        let n = Source::ALL.len() as i32;
        Source::ALL[(i + delta).rem_euclid(n) as usize]
    }
}

/// Everything one generation run needs. Built identically by CLI, env and TUI.
#[derive(Clone, Debug)]
pub struct RunConfig {
    /// Topic key from the registry, or `none` for a raw pass-through question.
    pub topic: String,
    pub format: Format,
    /// Hard generation budget. Careful: it also counts reasoning tokens.
    pub max_tokens: Option<u32>,
    /// Soft, semantic length limit: `maxItems` on the list inside the schema.
    pub max_items: Option<usize>,
    pub stop: Vec<String>,
    /// Reasoning on/off. Off is the default — see the note in README.
    pub thinking: bool,
    pub runs: u32,
    pub temperature: Option<f32>,
    /// Freeform user question / focus hint.
    pub question: String,
    /// Path to a schema file overriding the topic's built-in one.
    pub schema_file: Option<String>,
    // --- news fetching ---
    pub source: Source,
    pub since_hours: u64,
    pub limit: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            topic: "gamedev".into(),
            format: Format::JsonSchema,
            max_tokens: None,
            max_items: None,
            stop: Vec::new(),
            thinking: false,
            runs: 1,
            temperature: None,
            question: String::new(),
            schema_file: None,
            source: Source::Steam,
            since_hours: 168,
            limit: 12,
        }
    }
}

impl RunConfig {
    /// One-line description of the active constraints, for reports and the TUI.
    pub fn constraints(&self) -> String {
        let mut parts = vec![format!("format={}", self.format)];
        match self.max_tokens {
            Some(n) => parts.push(format!("max_tokens={n}")),
            None => parts.push("max_tokens=off".into()),
        }
        if let Some(n) = self.max_items {
            parts.push(format!("max_items={n}"));
        }
        if self.stop.is_empty() {
            parts.push("stop=off".into());
        } else {
            parts.push(format!("stop={}", render_stops(&self.stop)));
        }
        parts.push(format!(
            "think={}",
            if self.thinking { "on" } else { "off" }
        ));
        parts.join("  ")
    }
}

/// Escapes control characters so stop sequences stay readable in one line of output.
pub fn render_stops(stop: &[String]) -> String {
    let shown: Vec<String> = stop
        .iter()
        .map(|s| format!("\"{}\"", s.replace('\n', "\\n").replace('\t', "\\t")))
        .collect();
    shown.join(",")
}

/// A named bundle of settings — the "переключалка" between demo modes.
pub struct Preset {
    pub name: &'static str,
    pub blurb: &'static str,
    apply: fn(&mut RunConfig),
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "baseline",
        blurb: "no constraints at all: free prose, thinking on",
        apply: |c| {
            c.format = Format::Text;
            c.max_tokens = None;
            c.max_items = None;
            c.stop.clear();
            c.thinking = true;
        },
    },
    Preset {
        name: "json-soft",
        blurb: "json_object: valid JSON, but the model picks the keys",
        apply: |c| {
            c.format = Format::JsonObject;
            c.max_tokens = None;
            c.max_items = None;
            c.stop.clear();
            c.thinking = true;
        },
    },
    Preset {
        name: "strict",
        blurb: "json_schema + strict, thinking off: the shape is ours",
        apply: |c| {
            c.format = Format::JsonSchema;
            c.max_tokens = None;
            c.max_items = None;
            c.stop.clear();
            c.thinking = false;
        },
    },
    Preset {
        name: "capped",
        blurb: "strict + max_tokens=300: shows the truncation failure mode",
        apply: |c| {
            c.format = Format::JsonSchema;
            c.max_tokens = Some(300);
            c.max_items = None;
            c.stop.clear();
            c.thinking = false;
        },
    },
    Preset {
        name: "capped-items",
        blurb: "strict + maxItems=3: the semantic way to shorten the answer",
        apply: |c| {
            c.format = Format::JsonSchema;
            c.max_tokens = None;
            c.max_items = Some(3);
            c.stop.clear();
            c.thinking = false;
        },
    },
    Preset {
        name: "stopped",
        blurb: "text + stop on the item separator: cuts after the first entry",
        apply: |c| {
            c.format = Format::Text;
            c.max_tokens = Some(600);
            c.max_items = None;
            c.stop = vec!["\n---\n".into()];
            c.thinking = false;
        },
    },
    Preset {
        name: "strict-all",
        blurb: "all three controls at once: schema + budget + stop",
        apply: |c| {
            c.format = Format::JsonSchema;
            // ~1200 tokens are needed for a 7-item digest; 5 items fit in ~900.
            // 700 (and the plan's 400) truncate like `capped` — keep the budget
            // above that so this preset shows the knobs combining *successfully*.
            c.max_tokens = Some(1600);
            c.max_items = Some(5);
            c.stop = vec!["\n\n\n".into()];
            c.thinking = false;
        },
    },
];

pub fn preset(name: &str) -> Res<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name).ok_or_else(|| {
        format!(
            "unknown preset `{name}` (known: {})",
            preset_names().join(", ")
        )
    })
}

pub fn preset_names() -> Vec<&'static str> {
    PRESETS.iter().map(|p| p.name).collect()
}

pub fn apply_preset(cfg: &mut RunConfig, name: &str) -> Res<()> {
    (preset(name)?.apply)(cfg);
    Ok(())
}
