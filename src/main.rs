use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process;

const DEFAULT_BASE_URL: &str = "https://yolo-auto.com/v1";
const DEFAULT_MODEL: &str = "qwen3.8-27b";

#[derive(Deserialize)]
struct PiModels {
    providers: std::collections::BTreeMap<String, PiProvider>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiProvider {
    #[serde(default)]
    api_key: Option<String>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    content: String,
}

/// API key resolution order: $YOLO_API_KEY, then ~/.pi/agent/models.json (Yolo-Auto provider).
fn resolve_api_key() -> Result<String, String> {
    if let Ok(key) = env::var("YOLO_API_KEY") {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    let path = pi_models_path()?;
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    let models: PiModels = serde_json::from_str(&raw)
        .map_err(|e| format!("cannot parse {}: {}", path.display(), e))?;
    models
        .providers
        .get("Yolo-Auto")
        .and_then(|p| p.api_key.clone())
        .ok_or_else(|| format!("no Yolo-Auto apiKey found in {}", path.display()))
}

fn pi_models_path() -> Result<PathBuf, String> {
    env::var("HOME")
        .map(|h| PathBuf::from(h).join(".pi/agent/models.json"))
        .map_err(|_| "HOME not set".to_string())
}

fn main() {
    // Question: all CLI args joined, or stdin if none given.
    let question = {
        let args: Vec<String> = env::args().skip(1).collect();
        if args.is_empty() {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .unwrap_or_else(|e| panic!("cannot read stdin: {e}"));
            let q = buf.trim().to_string();
            if q.is_empty() {
                eprintln!("ask — send a question to the LLM");
                eprintln!("usage: ask <question...>   (or pipe via stdin)");
                process::exit(2);
            }
            q
        } else {
            args.join(" ")
        }
    };

    let api_key = match resolve_api_key() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(1);
        }
    };
    let base_url = env::var("YOLO_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
    let model = env::var("YOLO_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());

    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": question }],
    });

    let url = format!("{base_url}/chat/completions");
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Content-Type", "application/json")
        .send_json(body)
        .unwrap_or_else(|e| {
            eprintln!("request failed: {e}");
            process::exit(1);
        });

    let chat: ChatResponse = resp
        .into_json()
        .unwrap_or_else(|e| {
            eprintln!("bad response: {e}");
            process::exit(1);
        });

    match chat.choices.first() {
        Some(c) => println!("{}", c.message.content.trim()),
        None => {
            eprintln!("no choices in response");
            process::exit(1);
        }
    }
}
