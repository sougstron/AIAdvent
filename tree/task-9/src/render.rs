//! Turning a raw model reply into what the user actually sees — shared by
//! the one-shot CLI path and the chat TUI so both render JSON mode the
//! same way: `Key: value` lines, not a JSON dump.

use serde_json::Value;

/// Models sometimes wrap JSON in a code fence even in strict schema mode.
pub fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        return rest.trim_start_matches('\n').trim_end().trim_end_matches("```").trim();
    }
    t
}

/// Renders a JSON object as `Key: value` lines, matching the flat format the
/// spec describes (`Title / Game / Publisher / Summary`) instead of a raw
/// JSON dump.
pub fn flatten_json(v: &Value) -> String {
    match v.as_object() {
        Some(map) => map
            .iter()
            .map(|(k, val)| format!("{k}: {}", scalar(val)))
            .collect::<Vec<_>>()
            .join("\n"),
        None => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(scalar).collect::<Vec<_>>().join(", "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Parses `text` as JSON (tolerating code fences) and flattens it, or
/// returns the original text plus an error note when it doesn't parse.
pub fn render_json_reply(text: &str) -> (String, Option<String>) {
    match serde_json::from_str::<Value>(strip_fences(text)) {
        Ok(v) => (flatten_json(&v), None),
        Err(e) => (text.to_string(), Some(format!("not valid JSON: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flatten_json_renders_key_value_lines() {
        let v = json!({"title": "Big Patch", "summary": "fixed bugs"});
        let out = flatten_json(&v);
        assert!(out.contains("title: Big Patch"));
        assert!(out.contains("summary: fixed bugs"));
    }

    #[test]
    fn strip_fences_removes_json_code_fence() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn render_json_reply_reports_parse_errors() {
        let (text, err) = render_json_reply("not json");
        assert_eq!(text, "not json");
        assert!(err.is_some());
    }
}
