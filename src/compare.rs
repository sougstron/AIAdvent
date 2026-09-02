//! The experiment: run the same request under different constraint sets and measure
//! how stable the answers are. This is the "с ограничениями vs без" part of the task.

use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{apply_preset, Res, RunConfig};
use crate::engine::{short_hash, Engine, RunResult};

#[derive(Clone, Debug, Default)]
pub struct PresetReport {
    pub name: String,
    pub blurb: String,
    pub constraints: String,
    pub runs: u32,
    /// Requests that failed at the transport level.
    pub errors: Vec<String>,
    pub json_ok: usize,
    pub schema_ok: usize,
    pub empty_content: usize,
    /// How many different shapes the N runs produced. 1 = perfectly stable.
    pub distinct_keys: usize,
    pub distinct_types: usize,
    pub key_hashes: Vec<String>,
    pub completion_tokens: Vec<u64>,
    pub reasoning_tokens: u64,
    pub finish_reasons: BTreeMap<String, usize>,
    pub item_counts: Vec<usize>,
    pub latency_ms: Vec<u128>,
}

impl PresetReport {
    fn completed(&self) -> usize {
        self.runs as usize - self.errors.len()
    }

    pub fn tokens_summary(&self) -> String {
        summarize(&self.completion_tokens)
    }

    pub fn latency_summary(&self) -> String {
        let v: Vec<u64> = self.latency_ms.iter().map(|m| *m as u64).collect();
        summarize(&v)
    }

    pub fn finish_summary(&self) -> String {
        if self.finish_reasons.is_empty() {
            return "-".into();
        }
        self.finish_reasons
            .iter()
            .map(|(k, n)| format!("{k}×{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn items_summary(&self) -> String {
        if self.item_counts.is_empty() {
            return "-".into();
        }
        let min = self.item_counts.iter().min().unwrap();
        let max = self.item_counts.iter().max().unwrap();
        if min == max {
            format!("{min}")
        } else {
            format!("{min}-{max}")
        }
    }

    /// The headline number: does this mode give the same JSON shape every time?
    pub fn stability(&self) -> String {
        if self.distinct_keys == 0 {
            "n/a".into()
        } else if self.distinct_keys == 1 {
            "1 (stable)".into()
        } else {
            format!("{} (drift)", self.distinct_keys)
        }
    }
}

fn summarize(v: &[u64]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let median = s[s.len() / 2];
    if s[0] == s[s.len() - 1] {
        format!("{}", s[0])
    } else {
        format!("{}/{}/{}", s[0], median, s[s.len() - 1])
    }
}

/// Runs every preset `cfg.runs` times, saving each raw response under `out_dir`.
pub fn compare(
    engine: &Engine,
    base: &RunConfig,
    presets: &[String],
    out_dir: Option<&Path>,
    mut progress: impl FnMut(&str, u32, u32),
) -> Res<Vec<PresetReport>> {
    let mut reports = Vec::new();

    for name in presets {
        let mut cfg = base.clone();
        apply_preset(&mut cfg, name)?;
        let blurb = crate::config::preset(name)?.blurb.to_string();

        let mut report = PresetReport {
            name: name.clone(),
            blurb,
            constraints: cfg.constraints(),
            runs: cfg.runs,
            ..Default::default()
        };
        let mut keys = BTreeSet::new();
        let mut types = BTreeSet::new();

        for i in 0..cfg.runs {
            progress(name, i + 1, cfg.runs);
            match engine.run_once(&cfg) {
                Ok(res) => {
                    accumulate(&mut report, &res, &mut keys, &mut types);
                    if let Some(dir) = out_dir {
                        save_run(dir, name, i, &cfg, &res)?;
                    }
                }
                Err(e) => report.errors.push(e),
            }
        }

        report.distinct_keys = keys.len();
        report.distinct_types = types.len();
        report.key_hashes = keys.iter().map(|k| short_hash(k)).collect();
        reports.push(report);
    }
    Ok(reports)
}

fn accumulate(
    report: &mut PresetReport,
    res: &RunResult,
    keys: &mut BTreeSet<String>,
    types: &mut BTreeSet<String>,
) {
    if res.json_ok() {
        report.json_ok += 1;
    }
    if res.schema_ok() {
        report.schema_ok += 1;
    }
    if res
        .outcome
        .content
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        report.empty_content += 1;
    }
    if let Some(k) = &res.key_signature {
        keys.insert(k.clone());
    }
    if let Some(t) = &res.type_signature {
        types.insert(t.clone());
    }
    report
        .completion_tokens
        .push(res.outcome.usage.completion_tokens);
    report.reasoning_tokens += res.outcome.usage.reasoning_tokens;
    report.latency_ms.push(res.outcome.latency_ms);
    if let Some(n) = res.item_count() {
        report.item_counts.push(n);
    }
    let reason = res
        .outcome
        .finish_reason
        .clone()
        .unwrap_or_else(|| "unknown".into());
    *report.finish_reasons.entry(reason).or_insert(0) += 1;
}

fn save_run(dir: &Path, preset: &str, index: u32, cfg: &RunConfig, res: &RunResult) -> Res<()> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{preset}-{}.json", index + 1));
    let record = json!({
        "preset": preset,
        "run": index + 1,
        "constraints": cfg.constraints(),
        "finish_reason": res.outcome.finish_reason,
        "usage": {
            "prompt_tokens": res.outcome.usage.prompt_tokens,
            "completion_tokens": res.outcome.usage.completion_tokens,
            "reasoning_tokens": res.outcome.usage.reasoning_tokens,
            "total_tokens": res.outcome.usage.total_tokens,
        },
        "latency_ms": res.outcome.latency_ms,
        "json_valid": res.json_ok(),
        "json_error": res.json_error,
        "schema_errors": res.schema_errors,
        "key_signature_hash": res.key_signature.as_deref().map(short_hash),
        "content": res.outcome.content,
        "reasoning": res.outcome.reasoning,
        "parsed": res.json,
    });
    fs::write(
        &path,
        serde_json::to_string_pretty(&record).unwrap_or_default(),
    )
    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(())
}

/// Renders the comparison as a Markdown table (readable both in a terminal and a README).
pub fn render_table(reports: &[PresetReport]) -> String {
    let header = [
        "preset",
        "constraints",
        "JSON ok",
        "schema ok",
        "shapes",
        "items",
        "completion tok (min/med/max)",
        "finish_reason",
    ];
    let mut rows: Vec<Vec<String>> = vec![header.iter().map(|s| s.to_string()).collect()];

    for r in reports {
        let done = r.completed();
        rows.push(vec![
            r.name.clone(),
            r.constraints.clone(),
            format!("{}/{}", r.json_ok, done),
            format!("{}/{}", r.schema_ok, done),
            r.stability(),
            r.items_summary(),
            r.tokens_summary(),
            r.finish_summary(),
        ]);
    }

    let cols = rows[0].len();
    let widths: Vec<usize> = (0..cols)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();

    let mut out = String::new();
    for (n, row) in rows.iter().enumerate() {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}", w = w))
            .collect();
        out.push_str(&format!("| {} |\n", line.join(" | ")));
        if n == 0 {
            let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            out.push_str(&format!("| {} |\n", sep.join(" | ")));
        }
    }
    out
}

/// Full report: table plus the per-preset detail worth keeping.
pub fn render_report(engine: &Engine, base: &RunConfig, reports: &[PresetReport]) -> String {
    let mut out = String::new();
    out.push_str("# Response-control comparison\n\n");
    out.push_str(&format!("- model: `{}`\n", engine.endpoint.model));
    out.push_str(&format!("- endpoint: `{}`\n", engine.endpoint.base_url));
    out.push_str(&format!("- topic: `{}`\n", base.topic));
    out.push_str(&format!("- material: {}\n", engine.news_note));
    out.push_str(&format!("- runs per preset: {}\n\n", base.runs));
    out.push_str(&render_table(reports));
    out.push_str("\n## Presets\n\n");
    for r in reports {
        out.push_str(&format!("### `{}` — {}\n\n", r.name, r.blurb));
        out.push_str(&format!("- constraints: `{}`\n", r.constraints));
        out.push_str(&format!(
            "- distinct key shapes: {} ({})\n",
            r.distinct_keys,
            if r.key_hashes.is_empty() {
                "no JSON parsed".to_string()
            } else {
                r.key_hashes.join(", ")
            }
        ));
        out.push_str(&format!("- distinct type shapes: {}\n", r.distinct_types));
        out.push_str(&format!(
            "- reasoning tokens total: {}\n",
            r.reasoning_tokens
        ));
        out.push_str(&format!(
            "- latency ms (min/med/max): {}\n",
            r.latency_summary()
        ));
        if r.empty_content > 0 {
            out.push_str(&format!(
                "- **empty content in {} run(s)** — generation stopped before any visible token\n",
                r.empty_content
            ));
        }
        for e in &r.errors {
            out.push_str(&format!("- request error: {e}\n"));
        }
        out.push('\n');
    }
    out
}

/// `runs/<timestamp>` — where the raw responses of one comparison go.
pub fn default_out_dir() -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    PathBuf::from("runs").join(stamp)
}
