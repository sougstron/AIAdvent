//! Argument parsing and the non-interactive entry points: one-shot questions,
//! `--sessions`, `--verify`, and the login flows (`--login`, `--keys`,
//! `--logout`, `--verify-login`) that manage the machine's own API keys.

use clap::Parser;
use serde_json::Value;
use std::io::{IsTerminal, Read};

use crate::api::{self, Endpoint};
use crate::auth::{self, CheckResult, Provider};
use crate::billing;
use crate::config::{self, Effort, JsonMode, Res, Settings};
use crate::isolation;
use crate::mcp;
use crate::mcp_agent;
use crate::mcp_server;
use crate::orchestra;
use crate::rag;
use crate::ragqa;
use crate::memory;
use crate::profile;
use crate::render;
use crate::runtime::{BoxSpec, Runtime};
use crate::scheduler;
use crate::toolchain;
use crate::session;
use crate::verify;

#[derive(Parser, Debug)]
#[command(
    name = "ask",
    about = "A ChatGPT-style chat client for the terminal, over an OpenAI-compatible endpoint",
    long_about = "With no arguments, opens the interactive chat TUI: sessions, reasoning \
                  effort, JSON-schema output, a character length cap and a verifiable stop \
                  condition. With a question, sends it once and prints the answer.",
    after_help = "Examples:\n  \
        ask                                   open the chat TUI\n  \
        ask \"what is the capital of France?\"   one-shot question\n  \
        ask --login                           connect an API key (glm/deepseek/openrouter)\n  \
        printf %s \"$KEY\" | ask --login glm --key-stdin   connect a key from a script\n  \
        ask --keys                            show configured keys and their sources\n  \
        ask --models                          catalog grouped by provider (live / paid / no key)\n  \
        ask --verify-login                    live-recheck every configured key\n  \
        ask --verify                          prove z.ai levers with causal signatures\n  \
        ask --verify-billing                  show whether spend is metered or plan quota\n  \
        ask --strategy summary                history compression: keep the last N, summarize the rest\n  \
        ask --verify-compress                 prove compression saves tokens without losing facts\n  \
        ask --verify-context all              prove window / facts / branch strategies live\n  \
        ask --strategy memory                 three memory layers: short / working / long\n  \
        ask --verify-memory all               prove the memory layers are separate and load-bearing\n  \
        ask --profile chemist \"...\"            answer through a user profile (style / format / limits)\n  \
        ask --profiles                        list the profile catalog\n  \
        ask --verify-profile all              prove the profile reaches the wire and changes the voice\n  \
        ask --todo \"...\"                      run the answer through the task state machine\n  \
        ask --verify-todo all                 prove the task state machine holds and survives a pause\n  \
        ask --verify-lifecycle all            prove the task lifecycle is gated: no execute before an approved plan\n  \
        ask --mcp-tools [URL]                 connect to an MCP server and list its tools (default: DeepWiki)\n  \
        ask --mcp-serve --repo .              run the own git MCP server on 127.0.0.1:8765/mcp\n  \
        ask --mcp-call git_log --mcp-args '{\"limit\":3}'   call one MCP tool directly, no model\n  \
        ask --mcp http://127.0.0.1:8765/mcp \"кто автор последнего коммита?\"   agent answers via MCP tools\n  \
        ask --verify-mcp                      prove the agent calls the MCP tool and uses its result\n  \
        ask --scheduler                       24/7 scheduler: MCP server + jobs + Telegram (TELEGRAM_BOT_TOKEN)\n  \
        ask --mcp http://127.0.0.1:8766/mcp \"напомни через минуту\"   agent over the scheduler MCP tools\n  \
        ask --verify-scheduler                prove schedule / aggregate / reminder work through MCP\n  \
        ask --pipeline \"MCP\"                   search → summarize → saveToFile over the own pipeline MCP server\n  \
        ask --pipeline квазар --pipeline-source wikipedia --pipeline-file kvazar.md\n  \
        ask --pipeline-serve                  run the pipeline MCP server on 127.0.0.1:8767/mcp\n  \
        ask --verify-pipeline [offline|live|all]   prove the chain runs and hands data over intact\n  \
        ask --orchestra-demo /tmp/triage-demo  make a small git repo with TODO markers by several authors\n  \
        ask --triage --pipeline-root /tmp/triage-demo   TODO triage across 4 MCP servers: git, pipeline, tracker, notify\n  \
        ask --review --scope tree/task-20/src   review the latest commit there across the same 4 servers\n  \
        ask --verify-orchestra [offline|live|all]  prove routing and call order of the multi-server flow\n  \
        ask --rag-index docs                  chunk docs/ two ways, embed via Ollama, save rag/index.sqlite, compare\n  \
        ask --rag-index docs --chunk-strategy fixed --chunk-size 500 --chunk-overlap 100\n  \
        ask --rag-compare docs                re-print the chunking comparison from the saved index\n  \
        ask --rag \"what is HyDE?\"            answer with sources + verbatim quotes, or \"не знаю\" (task 24)\n  \
        ask --rag-eval                        20 control questions: plain / base / sim / llm / rewrite / full → rag/eval.md\n  \
        ask --rag-tune                        top-K and threshold sweeps for the second stage → rag/tune.md\n  \
        ask --rag-cite-eval                   sources + verbatim quotes + \"I don't know\" on 10+8 questions → rag/cite.md\n  \
        ask --rag-chat                        mini-chat: history + RAG + sources every turn + task memory (task 25)\n  \
        ask --rag-chat-eval                   two 10–15-message scenarios: sources kept, goal held → rag/chat.md\n  \
        ask --rag --rag-rewrite --rag-filter both \"q\"   rewrite + similarity threshold + LLM reranker (task 23)\n  \
        ask --strategy window --keep-recent 6 send only the last N messages\n  \
        ask --sessions                        list saved chat sessions\n  \
        ask --resume ID                       resume a saved session\n  \
        ask --continue                        resume the most recent session"
)]
pub struct Cli {
    /// Question for a one-shot answer. With none, opens the chat TUI.
    #[arg(trailing_var_arg = true)]
    pub question: Vec<String>,

    /// Model id from the catalog. Its provider must be connected (env var or
    /// `ask --login`); the request goes to that provider's endpoint. Paid ids
    /// are refused at send time by the money guard.
    #[arg(long)]
    pub model: Option<String>,

    /// Reasoning effort: none | low | medium | high.
    #[arg(long, env = "ASK_EFFORT")]
    pub effort: Option<String>,

    /// Turn on structured JSON output for this one-shot call.
    #[arg(long)]
    pub json: bool,

    /// Flat schema for --json: comma-separated string field names.
    #[arg(long, value_delimiter = ',')]
    pub json_fields: Vec<String>,

    /// Full JSON Schema file for --json, overriding --json-fields.
    #[arg(long)]
    pub json_schema_file: Option<String>,

    /// Hard character cap on the visible answer. Enforced client-side.
    #[arg(long, env = "ASK_MAX_CHARS")]
    pub max_chars: Option<usize>,

    /// Stop-condition token budget (counts reasoning + visible tokens).
    /// Sent as `max_tokens`.
    #[arg(long, env = "ASK_BUDGET_TOKENS")]
    pub budget_tokens: Option<u32>,

    /// Alias for --budget-tokens: cap on generated tokens.
    #[arg(long, env = "ASK_MAX_TOKENS")]
    pub max_tokens: Option<u32>,

    /// Stop sequence; repeatable, max 4. `\n`/`\t` escapes are understood.
    #[arg(long = "stop", env = "ASK_STOP", value_delimiter = ',')]
    pub stop: Vec<String>,

    /// Sampling temperature, 0.0 to 1.0 (z.ai documented range).
    #[arg(long)]
    pub temperature: Option<f32>,

    /// Nucleus sampling cutoff, 0.0 to 1.0. Unset: provider default.
    #[arg(long)]
    pub top_p: Option<f32>,

    /// Top-k cutoff; -1 disables it. Unset: provider default (which damps
    /// the visible effect of --temperature).
    // `allow_hyphen_values` so `--top-k -1` reads as a value, not a flag.
    #[arg(long, allow_hyphen_values = true)]
    pub top_k: Option<i32>,

    /// Context-management strategy: `off` (whole history), `summary`
    /// (running summary + last --keep-recent), `window` (last --keep-recent
    /// only), `facts` (key-value memory + last --keep-recent), `branch`
    /// (the active conversation branch) or `memory` (three memory layers in
    /// the system prompt + last --keep-recent). `--compress` is the old name.
    #[arg(
        long = "strategy",
        visible_alias = "compress",
        env = "ASK_COMPRESS",
        value_name = "STRATEGY"
    )]
    pub compress: Option<String>,

    /// With summary/window/facts: how many recent messages stay verbatim.
    #[arg(long, value_name = "N")]
    pub keep_recent: Option<usize>,

    /// With --strategy summary: how many messages one summary fold covers.
    #[arg(long, value_name = "N")]
    pub summarize_every: Option<usize>,

    /// Live proof that history compression saves tokens and keeps the facts
    /// from the folded part. Exits after printing.
    #[arg(long)]
    pub verify_compress: bool,

    /// Live proof for the other context strategies: `window`, `facts`,
    /// `branch` or `all`. Each verdict is a causal signature, not a text
    /// diff. Exits after printing.
    #[arg(long, value_name = "WHICH")]
    pub verify_context: Option<String>,

    /// Live proof for the three-layer memory model: `routing` (what goes
    /// into which layer, and what that leaves on disk), `influence` (one
    /// layer at a time, on the answers), `isolation` (switching tasks) or
    /// `all`. With --offline, `routing` skips its live half. Exits after
    /// printing.
    #[arg(long, value_name = "WHICH")]
    pub verify_memory: Option<String>,

    /// User profile applied to every request: an id from the catalog
    /// (`--profiles`) or `off`. The profile carries style, format and
    /// limits, and replaces the default "helpful assistant" prompt while
    /// that prompt is still the default one.
    #[arg(long, value_name = "ID")]
    pub profile: Option<String>,

    /// List the profile catalog (built-ins plus your own) and exit.
    #[arg(long)]
    pub profiles: bool,

    /// Task state machine (`todo.rs`): run the answer through the
    /// study → plan → execute → validate → report ladder instead of one
    /// plain turn. Off by default.
    #[arg(long)]
    pub todo: bool,

    /// Who signs off the plan before implementation starts: `manual`
    /// (a human runs `/todo approve`) or `auto` (signed automatically and
    /// logged as `approved-by=auto`). The gate itself is never skipped.
    /// Default: `manual` in the TUI, `auto` for a non-interactive
    /// `ask --todo "..."`, where there is nobody to ask.
    #[arg(long, value_name = "manual|auto")]
    pub approve: Option<String>,

    /// Live proof for the controlled task lifecycle (task 15): `machine`
    /// (the whole transition table offline — every illegal transition must
    /// be refused *and* leave the run byte-identical), `gate` (live: the
    /// model is told to skip the plan and the run must stay on `plan`),
    /// `resume` (a run picked back up from disk, not from memory),
    /// `cleanup` (where the run lives on disk and when it is removed),
    /// `offline` or `all`. Exits after printing.
    #[arg(long, value_name = "WHICH")]
    pub verify_lifecycle: Option<String>,

    /// Connect to an MCP server over Streamable HTTP (initialize +
    /// notifications/initialized) and print the tools it exposes
    /// (`tools/list`). Without a URL, the public DeepWiki server is used.
    /// Exits after printing.
    #[arg(long, value_name = "URL", num_args = 0..=1, default_missing_value = mcp::DEFAULT_URL)]
    pub mcp_tools: Option<String>,

    /// Run the own MCP server (task 17) around the git repository `--repo`
    /// on 127.0.0.1:`--mcp-port`, in the foreground, logging every request.
    #[arg(long)]
    pub mcp_serve: bool,

    /// Repository served by `--mcp-serve`.
    #[arg(long, value_name = "PATH", default_value = ".")]
    pub repo: String,

    /// Port for `--mcp-serve`.
    #[arg(long, value_name = "PORT", default_value_t = crate::mcp_server::DEFAULT_PORT)]
    pub mcp_port: u16,

    /// Answer the question as an agent that can call the tools of this MCP
    /// server (`tools/list` → model function calling → `tools/call`).
    #[arg(long, value_name = "URL")]
    pub mcp: Option<String>,

    /// Call one MCP tool directly (no model) on `--mcp` URL, or the local
    /// git server if `--mcp` is not given, and print the result.
    #[arg(long, value_name = "TOOL")]
    pub mcp_call: Option<String>,

    /// JSON arguments for `--mcp-call`.
    #[arg(long, value_name = "JSON", default_value = "{}")]
    pub mcp_args: String,

    /// Causal proof for task 17: a throwaway repository whose last commit
    /// carries a random codename, served by the own MCP server; Confirmed
    /// only if the agent calls the tool and answers with the codename while
    /// the same question without tools does not.
    #[arg(long)]
    pub verify_mcp: bool,

    /// Run the scheduler daemon (task 18) in the foreground: MCP server
    /// `ask-scheduler-mcp` on 127.0.0.1:`--sched-port`, the job loop, and —
    /// with `TELEGRAM_BOT_TOKEN` set — the Telegram bot, where every message
    /// goes to the agent with the scheduler's tools.
    #[arg(long)]
    pub scheduler: bool,

    /// SQLite database of the scheduler (jobs, samples, notifications).
    /// Default: `~/.ask6/scheduler.db`.
    #[arg(long, value_name = "PATH")]
    pub sched_db: Option<String>,

    /// Port of the scheduler MCP server.
    #[arg(long, value_name = "PORT", default_value_t = crate::scheduler::DEFAULT_PORT)]
    pub sched_port: u16,

    /// Telegram bot token for `--scheduler`.
    #[arg(long, env = "TELEGRAM_BOT_TOKEN", hide_env_values = true, value_name = "TOKEN")]
    pub telegram_token: Option<String>,

    /// Telegram chat that receives notifications. Without it, the first
    /// chat that sends /start is bound as the owner.
    #[arg(long, env = "TELEGRAM_CHAT_ID", value_name = "ID")]
    pub telegram_chat: Option<i64>,

    /// Only send to `--telegram-chat`, never call `getUpdates` — for a bot
    /// token that another process already polls (polling it here would
    /// steal that process's updates). Incoming chat is off.
    #[arg(long, env = "TELEGRAM_SEND_ONLY")]
    pub telegram_send_only: bool,

    /// HTTP proxy for the scheduler's weather and Telegram requests (the
    /// model endpoint stays direct), e.g. `http://192.168.0.128:8118`.
    #[arg(long, env = "ASK_SCHED_PROXY", value_name = "URL")]
    pub sched_proxy: Option<String>,

    /// Causal proof for task 18: the schedule on a fake clock (offline),
    /// aggregate numbers that exist only in SQLite reached by the agent over
    /// MCP (with a no-tools control), and a natural-language reminder that
    /// becomes a job and fires when due.
    #[arg(long)]
    pub verify_scheduler: bool,

    /// Run the automatic pipeline of task 19 for this query: `search` →
    /// `summarize` → `saveToFile`, each a `tools/call` to the own pipeline
    /// MCP server, with every step and hand-off printed.
    #[arg(long, value_name = "QUERY")]
    pub pipeline: Option<String>,

    /// Where `search` looks: `files` (under `--pipeline-root`) or `wikipedia`.
    #[arg(long, value_name = "SOURCE", default_value = "files")]
    pub pipeline_source: String,

    /// Folder searched by `search` with source `files`.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub pipeline_root: String,

    /// Folder `saveToFile` writes into. Default: `~/.ask6/pipeline/`.
    #[arg(long, value_name = "DIR")]
    pub pipeline_out: Option<String>,

    /// File name for the result. Default: `pipeline-<query>.md`.
    #[arg(long, value_name = "NAME")]
    pub pipeline_file: Option<String>,

    /// Run the pipeline MCP server on 127.0.0.1:`--pipeline-port` in the
    /// foreground, logging every request.
    #[arg(long)]
    pub pipeline_serve: bool,

    #[arg(long, value_name = "PORT", default_value_t = crate::toolchain::DEFAULT_PORT)]
    pub pipeline_port: u16,

    /// Causal proof for task 19: `offline` (the automatic chain on a corpus
    /// with a random codename, hand-offs checked by digest, two controls
    /// that must fail), `live` (the model assembles the chain from one
    /// sentence) or `all`.
    #[arg(long, value_name = "WHICH", num_args = 0..=1, default_missing_value = "all")]
    pub verify_pipeline: Option<String>,

    /// Task 20: TODO triage across four MCP servers — `search` (pipeline) →
    /// `git_log` per file (git) → `issue_create` per marker and
    /// `issue_list` (tracker) → `saveToFile` (pipeline) → `notify_send`
    /// (notify). The repository is `--pipeline-root`. Optional marker,
    /// default `TODO:`.
    #[arg(long, value_name = "MARKER", num_args = 0..=1, default_missing_value = "TODO:")]
    pub triage: Option<String>,

    /// Task 20: review of a commit across the four servers — `git_log` →
    /// `git_show` → `git_log` per changed file (git) → `issue_create` per
    /// file on its reviewer and `issue_list` (tracker) → `saveToFile`
    /// (pipeline) → `notify_send` (notify). Optional commit; default the
    /// latest one (in `--scope`, if given). The repository is the one
    /// `--pipeline-root` is in.
    #[arg(long, value_name = "REV", num_args = 0..=1, default_missing_value = "")]
    pub review: Option<String>,

    /// Folder for `--triage` (where to search) and `--review` (whose latest
    /// commit), relative to the repository root.
    #[arg(long, value_name = "DIR")]
    pub scope: Option<String>,

    /// File name of the triage report in `~/.ask6/pipeline/`.
    #[arg(long, value_name = "NAME", default_value = "triage.md")]
    pub triage_file: String,

    /// Create a small git repository with `TODO:` markers by several authors
    /// in DIR (must not exist or be empty) — a playground for the triage.
    #[arg(long, value_name = "DIR")]
    pub orchestra_demo: Option<String>,

    /// Causal proof for task 20: `offline` (the automatic flow over four
    /// servers, checked against each server's own log, with four controls
    /// that must fail), `live` (the model assembles the flow from one
    /// sentence) or `all`.
    #[arg(long, value_name = "WHICH", num_args = 0..=1, default_missing_value = "all")]
    pub verify_orchestra: Option<String>,

    /// Task 21: index the documents in DIR (`.pdf` via pdftotext, `.md`,
    /// `.txt`) — chunk, embed with a local Ollama model, save to `--rag-db`,
    /// then print the comparison of the chunking strategies.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "docs")]
    pub rag_index: Option<String>,

    /// Re-print the strategy comparison from the saved index (no
    /// re-chunking; only the probe questions are embedded). DIR holds
    /// `questions.json`.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "docs")]
    pub rag_compare: Option<String>,

    /// Task 22: answer with RAG — the question is embedded by Ollama, the
    /// nearest chunks of `--rag-db` go to the LLM together with it. In the
    /// chat this is the starting value of the `rag` setting (`/rag on|off`).
    #[arg(long)]
    pub rag: bool,

    /// How many chunks RAG adds to the question (1…12, default 4).
    #[arg(long, value_name = "N")]
    pub rag_k: Option<usize>,

    /// Which strategy of the index RAG retrieves from: `fixed` (default
    /// since task 23) or `structure`.
    #[arg(long, value_name = "structure|fixed")]
    pub rag_strategy: Option<String>,

    /// Task 23: rewrite the question into a search query with the LLM
    /// before retrieval (searches both, keeps the better score per chunk).
    #[arg(long)]
    pub rag_rewrite: bool,

    /// Task 23: second stage after retrieval — `off`, `sim` (similarity
    /// threshold), `llm` (LLM reranker with a threshold) or `both`.
    #[arg(long, value_name = "off|sim|llm|both")]
    pub rag_filter: Option<String>,

    /// Candidates retrieved before the filter (top-K before; `--rag-k` is
    /// top-K after). Default 20.
    #[arg(long, value_name = "N")]
    pub rag_pool: Option<usize>,

    /// Similarity threshold: z-score of a chunk's cosine among all chunks.
    #[arg(long, value_name = "Z")]
    pub rag_min_sim: Option<f32>,

    /// Reranker threshold: LLM relevance score 0–10.
    #[arg(long, value_name = "0..10")]
    pub rag_min_llm: Option<u8>,

    /// Task 24: "I don't know" threshold — the best chunk's reranker score
    /// below it means no answer and a request to clarify.
    #[arg(long, value_name = "0..10")]
    pub rag_idk_llm: Option<u8>,

    /// The same when the reranker did not run: z-score of the best chunk.
    #[arg(long, value_name = "Z")]
    pub rag_idk_z: Option<f32>,

    /// Task 24: grounded answers on ten control questions (one per document),
    /// the five without an answer and three vague ones: sources, verbatim
    /// quotes, numbers and meaning checked → `rag/cite.md`.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "docs")]
    pub rag_cite_eval: Option<String>,

    /// Task 25: the mini-chat with RAG + task memory — interactive: one
    /// line is one turn, every answer comes with sources, the task state
    /// (goal / clarifications / constraints / terms) is kept across turns.
    /// Commands inside: `/state`, `/reset`, `/mem on|off`, `/quit`.
    #[arg(long)]
    pub rag_chat: bool,

    /// Task 25: replay the long scenarios of `DIR/chat-scenarios.json`
    /// (two dialogues of 10–15 messages) through the same mini-chat and
    /// check sources on every answer, goal retention and the task memory →
    /// `rag/chat.md`.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "docs")]
    pub rag_chat_eval: Option<String>,

    /// Task 25: task memory of the RAG chat (`chatmem.rs`). On by default;
    /// `off` is the control for the eval and for the chat.
    #[arg(long, value_name = "on|off")]
    pub chatmem: Option<String>,

    /// Task 22/23: run the control questions `DIR/control.json` in every
    /// mode, score every answer against its expectation, write `rag/eval.md`.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "docs")]
    pub rag_eval: Option<String>,

    /// Modes for `--rag-eval`: `plain`, `base` (task 22's top-k), `sim`,
    /// `llm`, `rewrite`, `full` (rewrite + sim + llm), `mcp`.
    #[arg(long, value_name = "MODES", default_value = "plain,base,sim,llm,rewrite,full")]
    pub rag_eval_modes: String,

    /// Task 23: tune the second stage on the control questions without
    /// generating answers — recall of top-K before, similarity and reranker
    /// threshold sweeps → `rag/tune.md`.
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "docs")]
    pub rag_tune: Option<String>,

    /// Only these control question ids, e.g. `1,9`.
    #[arg(long, value_name = "IDS", value_delimiter = ',')]
    pub rag_eval_only: Vec<usize>,

    /// Chunking strategy for `--rag-index`: `fixed`, `structure` or `both`.
    #[arg(long, value_name = "fixed|structure|both", default_value = "both")]
    pub chunk_strategy: String,

    /// Fixed strategy: chunk length in characters.
    #[arg(long, value_name = "CHARS", default_value_t = 1000)]
    pub chunk_size: usize,

    /// Fixed strategy: characters shared by neighbouring chunks.
    #[arg(long, value_name = "CHARS", default_value_t = 200)]
    pub chunk_overlap: usize,

    /// Structure strategy: a longer section is split at sentence ends.
    #[arg(long, value_name = "CHARS", default_value_t = 4000)]
    pub struct_max: usize,

    /// Embedding model served by Ollama.
    #[arg(long, value_name = "MODEL", default_value = rag::DEFAULT_MODEL)]
    pub embed_model: String,

    /// Ollama base URL.
    #[arg(long, value_name = "URL", env = "OLLAMA_URL", default_value = rag::DEFAULT_URL)]
    pub ollama_url: String,

    /// SQLite file of the index; `chunks-*.jsonl` and `comparison.md` go
    /// next to it.
    #[arg(long, value_name = "FILE", default_value = rag::DEFAULT_DB)]
    pub rag_db: String,

    /// Live proof for the task state machine: `machine` (legal transitions
    /// pass, illegal ones are refused — no network), `wire` (what the state
    /// puts into the system message — no network), `resume` (pause, then
    /// continue without re-explaining anything), `ladder` (the whole ladder
    /// on a task with a machine-checkable answer), `offline` or `all`.
    /// Exits after printing.
    #[arg(long, value_name = "WHICH")]
    pub verify_todo: Option<String>,

    /// Project invariants: `on` (default) or `off`.
    #[arg(long, value_name = "on|off", default_value = "on")]
    pub invariants: String,

    /// JSON file containing editable project invariants.
    #[arg(long, value_name = "FILE", env = "ASK_INVARIANTS_FILE")]
    pub inv_file: Option<String>,

    /// Offline causal proof: `wire`, `validate`, `retry`, `refuse`, or `all`.
    #[arg(long, value_name = "WHICH")]
    pub verify_invariants: Option<String>,

    /// Live proof for personalization: `wire` (what the profile puts into
    /// the system message — no network), `voice` (same question, two
    /// profiles, each answer carrying its own machine-checkable signature),
    /// `auto` (what the long-term `профиль.*` memory adds by itself) or
    /// `all`. Exits after printing.
    #[arg(long, value_name = "WHICH")]
    pub verify_profile: Option<String>,

    /// Where the memory layers live: `<dir>/short`, `<dir>/working`,
    /// `<dir>/long`. Default: the snapshot's own `memory/` next to the
    /// binary, else `~/.ask6/memory`. Same as ASK_MEMORY_DIR.
    #[arg(long, value_name = "DIR", env = "ASK_MEMORY_DIR")]
    pub memory_dir: Option<String>,

    /// Model to run --verify-context / --verify-compress against. Any id the
    /// money guard allows live (glm-5.3-flash, deepseek-flash, OpenRouter
    /// `:free`). Default: glm-5.3-flash.
    #[arg(long, value_name = "ID")]
    pub verify_model: Option<String>,

    /// Check live whether spend goes to the metered API or the GLM Coding
    /// Plan subscription, print the verdict and exit.
    #[arg(long, visible_alias = "billing")]
    pub verify_billing: bool,

    /// Run the live z.ai lever proof (glm-5.3-flash only) and exit.
    #[arg(long, visible_alias = "verify-stop")]
    pub verify: bool,

    /// Prove agent-box isolation: 100 boxes in one process, then a live
    /// secret-token recall across separate sessions. Exits after printing.
    #[arg(long, visible_alias = "verify-boxes")]
    pub verify_isolation: bool,

    /// With --verify-isolation, run only the checks that need no network.
    #[arg(long)]
    pub offline: bool,

    /// List saved chat sessions and exit.
    #[arg(long)]
    pub sessions: bool,

    /// Resume a saved session by id (TUI, or one-shot if a question is given).
    #[arg(long)]
    pub resume: Option<String>,

    /// Resume the most recently updated session.
    #[arg(long = "continue")]
    pub continue_last: bool,

    /// Print the full API response instead of just the answer.
    #[arg(long)]
    pub raw: bool,

    /// Never print the usage/finish_reason line.
    #[arg(long)]
    pub quiet: bool,

    /// Print the model catalog grouped by provider, marking each id live,
    /// refused (paid), or hidden behind a missing key. Exits after printing.
    #[arg(long)]
    pub models: bool,

    /// With --models: live-list each connected provider's /models endpoint
    /// and diff it against the static catalog (drift report, no auto-edit).
    #[arg(long)]
    pub live: bool,

    /// Show every provider's key state: source (env/store), masked
    /// key, and the last live check. Exits after printing.
    #[arg(long)]
    pub keys: bool,

    /// Connect a provider's API key: hidden prompt (or --key-stdin), live
    /// check against the provider, then 0600 storage in ~/.ask6/auth.json.
    /// Value is a provider id (glm | deepseek | openrouter); bare --login
    /// opens an interactive picker. A rejected key is not saved (exit 1).
    #[arg(long, num_args = 0..=1, value_name = "PROVIDER")]
    pub login: Option<String>,

    /// With --login: read the key from stdin instead of the hidden prompt
    /// (for scripts and CI).
    #[arg(long)]
    pub key_stdin: bool,

    /// Remove a provider's key from the local store. An env var, if any,
    /// stays — unset it yourself.
    #[arg(long, value_name = "PROVIDER")]
    pub logout: Option<String>,

    /// Live-recheck every configured key against its provider and print the
    /// verdicts with evidence.
    #[arg(long)]
    pub verify_login: bool,
}

impl Cli {
    pub fn to_settings(&self) -> Res<Settings> {
        let mut s = Settings::default();
        if let Some(m) = &self.model {
            // Three-way: unknown id / known but keyless provider / usable.
            // The second branch is the only place a parse-time check reads
            // real key state; `config::find_model` + the provider list keep
            // the logic pure apart from that one call.
            match config::find_model(m) {
                None => return Err(config::catalog_error(m)),
                Some(cm) => {
                    let connected = auth::connected_providers();
                    if !connected.contains(&cm.provider) {
                        return Err(format!(
                            "model `{m}` needs a {} key: run `ask --login {}` (or set ${})",
                            cm.provider.label(),
                            cm.provider.id(),
                            cm.provider.env_var()
                        ));
                    }
                    s.model = m.clone();
                }
            }
        }
        if let Some(e) = &self.effort {
            s.effort = Effort::parse(e)?;
        }
        // Профиль проверяем по тому же каталогу, который увидит агент:
        // неизвестное имя должно падать здесь, а не молча уезжать в `off`.
        if let Some(p) = &self.profile {
            // Проверяем по тому же каталогу, который увидит агент, но файл
            // не трогаем: флаг действует на один запуск, а `active` в файле
            // — это то, что человек выбрал в TUI.
            s.profile = profile::ProfileSet::open(memory::memory_root()).resolve(p)?;
        }
        if self.todo {
            s.todo = true;
        }
        if self.rag {
            s.rag = true;
        }
        if let Some(k) = self.rag_k {
            if !(1..=crate::ragqa::MAX_K).contains(&k) {
                return Err(format!("--rag-k: от 1 до {}, получено {k}", crate::ragqa::MAX_K));
            }
            s.rag_k = k;
        }
        if let Some(st) = &self.rag_strategy {
            if !matches!(st.as_str(), "structure" | "fixed") {
                return Err(format!("--rag-strategy: structure или fixed, получено `{st}`"));
            }
            s.rag_strategy = st.clone();
        }
        if self.rag_rewrite {
            s.rag_rewrite = true;
        }
        if let Some(f) = &self.rag_filter {
            s.rag_filter = crate::rerank::Filter::parse(f).map_err(|e| format!("--rag-filter: {e}"))?.name().into();
        }
        if let Some(n) = self.rag_pool {
            if !(s.rag_k..=crate::rerank::MAX_POOL).contains(&n) {
                return Err(format!("--rag-pool: от --rag-k ({}) до {}, получено {n}", s.rag_k, crate::rerank::MAX_POOL));
            }
            s.rag_pool = n;
        }
        if let Some(z) = self.rag_min_sim {
            s.rag_min_sim = z;
        }
        if let Some(n) = self.rag_min_llm {
            if n > 10 {
                return Err(format!("--rag-min-llm: от 0 до 10, получено {n}"));
            }
            s.rag_min_llm = n;
        }
        if let Some(n) = self.rag_idk_llm {
            if n > 10 {
                return Err(format!("--rag-idk-llm: от 0 до 10, получено {n}"));
            }
            s.rag_idk_llm = n;
        }
        if let Some(z) = self.rag_idk_z {
            s.rag_idk_z = z;
        }
        if let Some(v) = &self.chatmem {
            s.chatmem = match v.as_str() {
                "on" => true,
                "off" => false,
                other => return Err(format!("--chatmem: ожидалось on или off, получено `{other}`")),
            };
        }
        // Кто подписывает план. Без флага: в TUI ждём человека, а в
        // неинтерактивном заходе спросить некого — подпись ставит `auto`,
        // и это видно в журнале строкой `approved-by=auto`. Гейт при этом
        // проходится в обоих случаях.
        s.approve = match &self.approve {
            Some(v) => crate::run::ApprovePolicy::parse(v)?,
            None if self.todo && !self.question.is_empty() => crate::run::ApprovePolicy::Auto,
            None => crate::run::ApprovePolicy::Manual,
        };
        s.invariants = match self.invariants.as_str() {
            "on" => true,
            "off" => false,
            other => return Err(format!("--invariants: ожидалось on или off, получено `{other}`")),
        };
        if let Some(path) = &self.json_schema_file {
            let raw =
                std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
            let schema: Value =
                serde_json::from_str(&raw).map_err(|e| format!("cannot parse {path}: {e}"))?;
            s.json_mode = JsonMode {
                enabled: true,
                schema,
            };
        } else if !self.json_fields.is_empty() {
            s.json_mode = JsonMode {
                enabled: true,
                schema: config::flat_string_schema(&self.json_fields),
            };
        } else if self.json {
            s.json_mode.enabled = true;
        }
        if let Some(n) = self.max_chars {
            s.max_chars = Some(n);
        }
        if let Some(n) = self.max_tokens.or(self.budget_tokens) {
            s.budget_tokens = Some(config::parse_max_tokens(n)?);
        }
        if !self.stop.is_empty() {
            if self.stop.len() > 4 {
                return Err("at most 4 stop sequences are supported".into());
            }
            s.stop = self.stop.iter().map(|s| config::unescape(s)).collect();
        }
        if let Some(t) = self.temperature {
            s.temperature = Some(config::parse_temperature(t)?);
        }
        if let Some(p) = self.top_p {
            s.top_p = Some(config::parse_top_p(p)?);
        }
        if let Some(c) = &self.compress {
            s.context_strategy = config::ContextStrategy::parse(c)?;
        }
        if let Some(n) = self.keep_recent {
            if n == 0 {
                return Err("--keep-recent must be at least 1".into());
            }
            s.keep_recent = n;
        }
        if let Some(n) = self.summarize_every {
            if n == 0 {
                return Err("--summarize-every must be at least 1".into());
            }
            s.summarize_every = n;
        }
        if let Some(k) = self.top_k {
            if k == 0 || k < -1 {
                return Err(format!(
                    "top_k must be -1 (off) or a positive count (got {k})"
                ));
            }
            s.top_k = Some(k);
        }
        Ok(s)
    }
}

pub fn run() -> Res<()> {
    let cli = Cli::parse();

    if cli.keys {
        return print_keys();
    }
    if let Some(id) = cli.logout.as_deref() {
        return logout(id);
    }
    if cli.login.is_some() || cli.key_stdin {
        return login(cli.login.as_deref(), cli.key_stdin);
    }
    if cli.verify_login {
        return verify_login();
    }
    if cli.sessions {
        return print_sessions();
    }
    if cli.models {
        return print_models(cli.live);
    }
    if cli.live {
        return Err("--live is only meaningful together with --models".into());
    }

    // `--memory-dir` доезжает до памяти через ту же переменную, что и
    // `ASK_MEMORY_DIR`: у `memory_root()` остаётся один источник правды.
    if let Some(dir) = cli.memory_dir.as_deref().filter(|d| !d.is_empty()) {
        std::env::set_var("ASK_MEMORY_DIR", dir);
    }
    if let Some(path) = cli.inv_file.as_deref().filter(|p| !p.is_empty()) {
        std::env::set_var("ASK_INVARIANTS_FILE", path);
    }

    if cli.profiles {
        println!("{}", profile::ProfileSet::open(memory::memory_root()).listing());
        return Ok(());
    }

    let settings = cli.to_settings()?;

    if let Some(which) = cli.verify_invariants.as_deref() {
        println!("{}", crate::invariants::verify(which)?);
        return Ok(());
    }

    if cli.verify_billing {
        let report = billing::run()?;
        print!("{}", report.render());
        if !report.confirmed() {
            return Err("billing is not pay-per-token".into());
        }
        return Ok(());
    }

    if cli.verify {
        let report = verify::run()?;
        println!("{}", report.render());
        return Ok(());
    }

    if let Some(which) = cli.verify_context.as_deref() {
        let checks = verify::ContextCheck::parse(which)?;
        println!(
            "проверяю стратегии: {}",
            checks
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let reports = verify::run_context(&checks, cli.verify_model.as_deref())?;
        let mut calls = 0;
        for report in &reports {
            print!("{}", report.render());
            println!();
            calls += report.calls();
        }
        for report in &reports {
            println!("{}", report.status_line());
        }
        println!("живых вызовов всего: {calls}");
        if let Some(bad) = reports.iter().find(|r| !r.confirmed()) {
            return Err(format!(
                "context strategy not confirmed: {}",
                bad.status_line()
            ));
        }
        return Ok(());
    }

    if let Some(which) = cli.verify_memory.as_deref() {
        let checks = verify::MemoryCheck::parse(which)?;
        println!(
            "проверяю модель памяти: {}",
            checks
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let reports = verify::run_memory(&checks, cli.verify_model.as_deref(), cli.offline)?;
        let mut calls = 0;
        for report in &reports {
            print!("{}", report.render());
            println!();
            calls += report.calls();
        }
        for report in &reports {
            println!("{}", report.status_line());
        }
        println!("живых вызовов всего: {calls}");
        if let Some(bad) = reports.iter().find(|r| !r.confirmed()) {
            return Err(format!("memory model not confirmed: {}", bad.status_line()));
        }
        return Ok(());
    }

    if let Some(url) = cli.mcp_tools.as_deref() {
        println!("MCP: подключаюсь к {url}");
        let mut conn = mcp::Connection::connect(url)?;
        println!(
            "соединение установлено: {} {} (протокол {})",
            conn.server_name, conn.server_version, conn.protocol_version
        );
        let tools = conn.list_tools()?;
        println!("инструментов: {}", tools.len());
        for t in &tools {
            println!("\n• {}({})", t.name, t.params.join(", "));
            let first = t.description.lines().next().unwrap_or("");
            if !first.is_empty() {
                println!("  {first}");
            }
        }
        return Ok(());
    }

    if cli.mcp_serve {
        let server = mcp_server::Server::new(std::path::Path::new(&cli.repo), true)?;
        let listener = std::net::TcpListener::bind(("127.0.0.1", cli.mcp_port))
            .map_err(|e| format!("bind 127.0.0.1:{}: {e}", cli.mcp_port))?;
        println!("git MCP-сервер: http://127.0.0.1:{}/mcp", cli.mcp_port);
        println!("репозиторий: {}", server.repo().display());
        println!(
            "инструменты: {}",
            mcp_server::tool_specs()
                .iter()
                .filter_map(|t| t["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("Ctrl+C — остановить");
        server.serve(listener);
        return Ok(());
    }

    if let Some(tool) = cli.mcp_call.as_deref() {
        let url = mcp_url(&cli);
        let args: Value = serde_json::from_str(&cli.mcp_args)
            .map_err(|e| format!("--mcp-args is not JSON: {e}"))?;
        let mut conn = mcp::Connection::connect(&url)?;
        let r = conn.call_tool(tool, args)?;
        println!("{}", r.text);
        if r.is_error {
            return Err(format!("tool {tool} reported an error"));
        }
        return Ok(());
    }

    if cli.verify_mcp {
        let settings = cli.to_settings()?;
        println!("проверяю MCP-инструмент на модели {}", settings.model);
        return if mcp_agent::verify(&settings)? {
            Ok(())
        } else {
            Err("MCP tool use not confirmed".into())
        };
    }

    if cli.verify_scheduler {
        let settings = cli.to_settings()?;
        println!("проверяю планировщик на модели {}", settings.model);
        return if scheduler::verify(&settings)? {
            Ok(())
        } else {
            Err("scheduler not confirmed".into())
        };
    }

    if let Some(dir) = cli.rag_index.as_deref().or(cli.rag_compare.as_deref()) {
        let cfg = rag::Config {
            dir: dir.into(),
            strategies: rag::Strategy::parse_list(&cli.chunk_strategy)?,
            size: cli.chunk_size,
            overlap: cli.chunk_overlap,
            struct_max: cli.struct_max,
            model: cli.embed_model.clone(),
            url: cli.ollama_url.clone(),
            db: cli.rag_db.clone().into(),
        }
        .resolve_paths();
        return if cli.rag_index.is_some() { rag::index(&cfg) } else { rag::compare(&cfg) };
    }

    if let Some(dir) = cli.rag_eval.as_deref() {
        let settings = cli.to_settings()?;
        let opts = ragqa::EvalOpts {
            paths: rag::Config::locate(dir, &cli.rag_db),
            strategy: settings.rag_strategy.clone(),
            modes: ragqa::Mode::parse_list(&cli.rag_eval_modes)?,
            only: cli.rag_eval_only.clone(),
            tuned: crate::rerank::Pipeline::from_settings(&settings),
        };
        return ragqa::eval(&settings, &opts);
    }

    if let Some(dir) = cli.rag_cite_eval.as_deref() {
        let settings = cli.to_settings()?;
        return crate::cite::eval(&settings, &rag::Config::locate(dir, &cli.rag_db), &cli.rag_eval_only);
    }

    if cli.rag_chat {
        let settings = cli.to_settings()?;
        return crate::chatmem::repl(&settings, &rag::Config::locate("docs", &cli.rag_db));
    }

    if let Some(dir) = cli.rag_chat_eval.as_deref() {
        let settings = cli.to_settings()?;
        return crate::chatmem::eval(&settings, &rag::Config::locate(dir, &cli.rag_db), &cli.rag_eval_only);
    }

    if let Some(dir) = cli.rag_tune.as_deref() {
        let settings = cli.to_settings()?;
        return crate::rerank::tune(&settings, &rag::Config::locate(dir, &cli.rag_db), &cli.rag_eval_only);
    }

    if let Some(which) = cli.verify_pipeline.as_deref() {
        let settings = cli.to_settings()?;
        println!("проверяю пайплайн MCP-инструментов ({which}) на модели {}", settings.model);
        return if toolchain::verify(which, &settings)? {
            Ok(())
        } else {
            Err("pipeline not confirmed".into())
        };
    }

    if let Some(which) = cli.verify_orchestra.as_deref() {
        let settings = cli.to_settings()?;
        println!("проверяю оркестрацию MCP-серверов ({which}) на модели {}", settings.model);
        return if orchestra::verify(which, &settings)? {
            Ok(())
        } else {
            Err("orchestration not confirmed".into())
        };
    }

    if let Some(dir) = cli.orchestra_demo.as_deref() {
        let fx = orchestra::demo(std::path::Path::new(dir))?;
        println!("демо-репозиторий: {dir}");
        for (src, who) in &fx.todos {
            println!("  TODO в {src} — последним файл менял {who}");
        }
        println!("  последний коммит (HEAD) — {}: он НЕ исполнитель ни одной метки", fx.head_author);
        println!(
            "  последний коммит в src/ — {} от {}: ревьюеры {}",
            &fx.review_hash[..7],
            fx.review_author,
            fx.review.iter().map(|(f, r)| format!("{f} → {r}")).collect::<Vec<_>>().join(", ")
        );
        println!("\nдальше: cd {dir} && ask   → в чате /triage, /review in src или «разбери TODO в проекте …»");
        return Ok(());
    }

    if let Some(marker) = cli.triage.as_deref() {
        let root = std::path::Path::new(&cli.pipeline_root);
        let mut tb = orchestra::local_toolbox(root)?;
        for s in &tb.servers {
            println!("MCP: {} — {}", s.conn.server_name, s.label);
        }
        let lanes = tb.lanes();
        let mut req = orchestra::TriageRequest::new(Some(marker), Some(&cli.triage_file));
        req.path = cli.scope.clone();
        let rep = orchestra::run_triage(&mut tb, &req, &lanes, &mut |l| println!("{l}"))?;
        println!("\n--- {} ---\n{}", rep.path, rep.table);
        return if rep.ok() { Ok(()) } else { Err("triage flow audit failed".into()) };
    }

    if let Some(rev) = cli.review.as_deref() {
        let mut tb = orchestra::local_toolbox(std::path::Path::new(&cli.pipeline_root))?;
        for s in &tb.servers {
            println!("MCP: {} — {}", s.conn.server_name, s.label);
        }
        let lanes = tb.lanes();
        let req = orchestra::ReviewRequest::new(Some(rev), cli.scope.as_deref(), None);
        let rep = orchestra::run_review(&mut tb, &req, &lanes, &mut |l| println!("{l}"))?;
        println!("\n--- {} ---\n{}", rep.path, rep.table);
        return if rep.ok() { Ok(()) } else { Err("review flow audit failed".into()) };
    }

    if cli.pipeline.is_some() || cli.pipeline_serve {
        let out = cli
            .pipeline_out
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(toolchain::default_out);
        let server = toolchain::Server::new(std::path::Path::new(&cli.pipeline_root), &out, cli.pipeline_serve)?;
        if cli.pipeline_serve {
            let listener = std::net::TcpListener::bind(("127.0.0.1", cli.pipeline_port))
                .map_err(|e| format!("bind 127.0.0.1:{}: {e}", cli.pipeline_port))?;
            println!("pipeline MCP-сервер: http://127.0.0.1:{}/mcp", cli.pipeline_port);
            println!("поиск в: {}\nфайлы в: {}", server.root().display(), server.out().display());
            println!("инструменты: {}", toolchain::TOOLS.join(", "));
            println!("Ctrl+C — остановить");
            server.serve(listener);
            return Ok(());
        }
        let url = server.spawn(0)?;
        let mut conn = mcp::Connection::connect(&url)?;
        println!("MCP: {} {} на {url}", conn.server_name, conn.server_version);
        let query = cli.pipeline.as_deref().unwrap_or_default();
        let req = toolchain::ChainRequest::new(query, &cli.pipeline_source, cli.pipeline_file.as_deref());
        let rep = toolchain::run_chain(&mut conn, &req, &mut |l| println!("{l}"))?;
        println!("\n--- {} ---\n{}", rep.path, rep.summary);
        return if rep.ok() { Ok(()) } else { Err("pipeline hand-off mismatch".into()) };
    }

    if cli.scheduler {
        let db = match cli.sched_db.as_deref() {
            Some(p) => std::path::PathBuf::from(p),
            None => std::path::PathBuf::from(std::env::var("HOME").map_err(|_| "HOME is not set")?)
                .join(".ask6/scheduler.db"),
        };
        return scheduler::run_daemon(scheduler::DaemonOpts {
            db,
            port: cli.sched_port,
            settings: cli.to_settings()?,
            telegram_token: cli.telegram_token.clone().filter(|t| !t.trim().is_empty()),
            owner: cli.telegram_chat,
            send_only: cli.telegram_send_only,
            proxy: cli.sched_proxy.clone().filter(|p| !p.trim().is_empty()),
        });
    }

    if let Some(url) = cli.mcp.as_deref() {
        let settings = cli.to_settings()?;
        let question = read_question(&cli.question)?;
        let ep = Endpoint::for_model(&settings.model)?;
        let mut conn = mcp::Connection::connect(url)?;
        let tools = conn.list_tools()?;
        eprintln!(
            "MCP: {} {} — инструменты: {}",
            conn.server_name,
            conn.server_version,
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")
        );
        let mut on_step = |s: &mcp_agent::ToolStep| {
            eprintln!("→ {} {}", s.name, s.args);
            eprintln!("← {}{}", if s.is_error { "ERROR " } else { "" }, mcp_agent::preview(&s.result));
        };
        let run = mcp_agent::run(&ep, &settings, &mut conn, &tools, &question, &mut on_step)?;
        println!("{}", if run.answer.is_empty() { "(no content)" } else { &run.answer });
        eprintln!("{}", run.footer());
        return Ok(());
    }

    if let Some(which) = cli.verify_lifecycle.as_deref() {
        let checks = verify::LifeCheck::parse(which)?;
        println!(
            "проверяю жизненный цикл задачи: {}",
            checks
                .iter()
                .map(|c| if c.offline() {
                    format!("{} (без сети)", c.label())
                } else {
                    c.label().to_string()
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        let reports = verify::run_lifecycle(&checks, cli.verify_model.as_deref())?;
        let mut calls = 0;
        for report in &reports {
            print!("{}", report.render());
            println!();
            calls += report.calls;
        }
        for report in &reports {
            println!("{}", report.status_line());
        }
        println!("живых вызовов всего: {calls}");
        if let Some(bad) = reports.iter().find(|r| !r.confirmed()) {
            return Err(format!("lifecycle not confirmed: {}", bad.status_line()));
        }
        return Ok(());
    }

    if let Some(which) = cli.verify_todo.as_deref() {
        let checks = verify::TodoCheck::parse(which)?;
        println!(
            "проверяю состояние задачи: {}",
            checks
                .iter()
                .map(|c| if c.offline() {
                    format!("{} (без сети)", c.label())
                } else {
                    c.label().to_string()
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        let reports = verify::run_todo(&checks, cli.verify_model.as_deref())?;
        let mut calls = 0;
        for report in &reports {
            print!("{}", report.render());
            println!();
            calls += report.calls();
        }
        for report in &reports {
            println!("{}", report.status_line());
        }
        println!("живых вызовов всего: {calls}");
        if let Some(bad) = reports.iter().find(|r| !r.confirmed()) {
            return Err(format!("todo not confirmed: {}", bad.status_line()));
        }
        return Ok(());
    }

    if let Some(which) = cli.verify_profile.as_deref() {
        let checks = verify::ProfileCheck::parse(which)?;
        println!(
            "проверяю персонализацию: {}",
            checks
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let reports = verify::run_profile(&checks, cli.verify_model.as_deref())?;
        let mut calls = 0;
        for report in &reports {
            print!("{}", report.render());
            println!();
            calls += report.calls();
        }
        for report in &reports {
            println!("{}", report.status_line());
        }
        println!("живых вызовов всего: {calls}");
        if let Some(bad) = reports.iter().find(|r| !r.confirmed()) {
            return Err(format!("profile not confirmed: {}", bad.status_line()));
        }
        return Ok(());
    }

    if cli.verify_compress {
        let report = verify::run_compression(cli.verify_model.as_deref())?;
        print!("{}", report.render());
        if !report.confirmed() {
            return Err(format!(
                "compression not confirmed: {}",
                report.status_line()
            ));
        }
        return Ok(());
    }

    if cli.verify_isolation {
        let report = isolation::run(cli.offline)?;
        print!("{}", report.render());
        if !report.all_confirmed() {
            return Err("isolation not fully confirmed".into());
        }
        return Ok(());
    }

    let dir = session::sessions_dir();
    let loaded = if let Some(id) = cli.resume.as_deref() {
        Some(session::load_session(&dir, id)?)
    } else if cli.continue_last {
        Some(session::continue_last(&dir)?)
    } else {
        None
    };

    let question = read_question(&cli.question)?;
    if question.is_empty() {
        return crate::tui::run(settings, loaded);
    }

    if settings.todo {
        // Тудушка включена — один вопрос превращается в лестницу этапов,
        // ровно как в TUI: по запросу на этап, переход только на
        // подтверждении этапа.
        return todo_shot(settings, &question);
    }
    one_shot(&cli, settings, &question, loaded)
}

/// Лестница этапов в один заход (`ask --todo "..."`). Печатает каждый
/// переход — то же «явно показывает, когда переходит на следующий шаг», но
/// в терминал, а не в панель.
///
/// Правила переходов сюда не копируются: и TUI, и этот путь двигают один и
/// тот же [`crate::run::TaskRun`] через одну и ту же таблицу. Разница
/// только в дефолте гейта — в неинтерактивном заходе плану некому сказать
/// «ок», поэтому подпись ставит `auto`, и это видно в журнале.
fn todo_shot(settings: Settings, question: &str) -> Res<()> {
    let mut agent = crate::agent::Agent::new(settings.clone())?;
    let mut run = crate::run::TaskRun::new("one-shot", &settings);
    run.apply(crate::run::Event::Start(question.to_string()))
        .map_err(|r| r.why)?;

    let mut history: Vec<api::ChatMessage> = Vec::new();
    loop {
        if run.awaiting_approval() {
            // Гейт проходится, а не пропускается: в одноразовом заходе
            // подпись ставит `auto`, но переход всё равно идёт событием
            // `Approve` и ложится в журнал.
            if settings.approve == crate::run::ApprovePolicy::Auto {
                run.apply(crate::run::Event::Approve("auto".into()))
                    .map_err(|r| r.why)?;
                eprintln!(
                    "\u{2714} план утверждён автоматически (approved-by=auto) \u{b7} {}",
                    run.line()
                );
            } else {
                eprintln!("{}", run.line());
                return Err(format!(
                    "план этапа `plan` закрыт и ждёт утверждения, а спросить некого: \
                     запустите с --approve auto или ведите задачу в TUI ({})",
                    run.log_tail(1)
                ));
            }
        }
        if !run.running() {
            break;
        }
        eprintln!("{}", run.state.enter_line());
        let set = agent.invariants().clone();
        let inv_on = agent.settings().invariants;
        let hist = history.clone();
        let mut sent_prompt = String::new();
        let outcome = crate::run::Engine::step(
            &mut run,
            &set,
            inv_on,
            None,
            |prompt, _, retry_note| {
                sent_prompt = prompt.to_string();
                let mut attempt = hist.clone();
                attempt.push(api::ChatMessage::user(prompt.to_string()));
                if let Some(note) = retry_note {
                    attempt.push(api::ChatMessage::user(note));
                }
                agent.complete(&attempt)
            },
            &mut |r: &crate::run::TaskRun| {
                // Одноразовый заход ничего не хранит на диске: прогон
                // живёт ровно столько, сколько идёт процесс. Фазу всё
                // равно показываем — по ней видно, где нас оборвёт.
                let _ = r;
            },
        )?;
        agent.set_todo(run.state.clone());
        match outcome {
            crate::run::StepOutcome::Advanced { closed, summary, next, text } => {
                eprintln!("\u{2192} следующий этап: `{}`", next.id());
                println!("{}", text.trim());
                println!();
                history.push(api::ChatMessage::user(sent_prompt));
                history.push(api::ChatMessage::assistant(text));
                eprintln!("{}", crate::todo::leave_line(closed, &summary));
            }
            crate::run::StepOutcome::AwaitingApproval { stage, summary, text } => {
                println!("{}", text.trim());
                println!();
                history.push(api::ChatMessage::user(sent_prompt));
                history.push(api::ChatMessage::assistant(text));
                eprintln!("{}", crate::todo::leave_line(stage, &summary));
                eprintln!(
                    "\u{270b} этап `{}` закрыт и ждёт утверждения \u{2014} до подписи реализация не начнётся",
                    stage.id()
                );
            }
            crate::run::StepOutcome::Done { pass, why, text } => {
                if let Some(t) = text {
                    println!("{}", t.trim());
                }
                eprintln!("{}", run.line());
                return if pass {
                    Ok(())
                } else {
                    Err(format!("прогон закрыт как done(fail): {why}"))
                };
            }
            crate::run::StepOutcome::Paused { why, text } => {
                if let Some(t) = text {
                    println!("{}", t.trim());
                }
                eprintln!("\u{23f8} {why}");
                return Err(format!(
                    "лестница не дошла до done: остановились на этапе `{}` ({why})",
                    run.state.stage.id()
                ));
            }
            crate::run::StepOutcome::Refused(r) => {
                eprintln!("{}", r.line());
                return Err(format!("переход отклонён: {}", r.why));
            }
        }
    }
    eprintln!("{}", run.line());
    if run.status != crate::run::RunStatus::DonePass {
        return Err(format!(
            "лестница не дошла до done: остановились на `{}` ({})",
            run.status.as_str(),
            run.state.stage.id()
        ));
    }
    Ok(())
}

/// The one-shot path runs through the multi-agent runtime rather than around
/// it: one `Runtime`, one `AgentBox`, one turn. The box is what enforces the
/// input/output policies and owns the session, so this path exercises the
/// same code a hundred concurrent boxes would.
fn one_shot(
    cli: &Cli,
    settings: Settings,
    question: &str,
    loaded: Option<session::Session>,
) -> Res<()> {
    // Задача 24: с RAG ответ — с обязательными источниками и цитатами, а
    // при слабом контексте — «не знаю» без вызова модели (`cite.rs`). Это
    // отдельный путь без сессии: проверка ответа важнее истории.
    if settings.rag {
        return crate::cite::ask_once(question, &settings);
    }
    let mut rt = Runtime::for_model(&settings.model, session::sessions_dir())?;
    let spec = BoxSpec::new("", settings.clone());
    let id = match loaded {
        Some(s) => rt.resume(&s.id, spec)?,
        None => rt.spawn(spec),
    };
    let turn = rt.get_mut(&id).ok_or("box vanished")?.ask_with(question, None)?;

    if let Some(why) = turn.refusal() {
        return Err(why);
    }
    let reply = turn.reply.as_ref().ok_or("no reply on an accepted turn")?;

    if cli.raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&reply.raw).unwrap_or_default()
        );
    } else {
        let (display, parse_note) = if settings.json_mode.enabled {
            render::render_json_reply(&turn.text)
        } else {
            (turn.text.clone(), None)
        };
        if display.is_empty() {
            println!("(no content)");
        } else {
            println!("{display}");
        }
        if reply.truncated_by_max_chars {
            // Потолок мог приехать из профиля, а не из `--max-chars`:
            // печатаем тот, по которому реально резали.
            let cap = rt
                .get(&id)
                .and_then(|b| b.effective_settings().max_chars)
                .or(settings.max_chars)
                .unwrap_or(0);
            eprintln!("! truncated to max_chars={cap}");
        }
        if let Some(e) = parse_note {
            eprintln!("! {e}");
        }
    }

    if !cli.quiet {
        let u = &reply.usage;
        let b = rt.get(&id).ok_or("box vanished")?;
        eprintln!(
            "« model={}  finish={}  tokens: prompt={} completion={} (reasoning={}) total={}  ~${:.6}  {}ms  |  {}",
            if reply.model.is_empty() { "?" } else { &reply.model },
            reply.finish_reason.as_deref().unwrap_or("?"),
            u.prompt_tokens,
            u.completion_tokens,
            u.reasoning_tokens,
            u.total_tokens,
            billing::cost_usd(u),
            reply.latency_ms,
            b.settings().summary(),
        );
        for f in b.context_files() {
            eprintln!(
                "« context {}: {} ({} chars{})",
                f.scope.as_str(),
                f.path.display(),
                f.chars,
                if f.truncated { ", truncated" } else { "" }
            );
        }
    }
    if reply.truncated() {
        eprintln!("! cut by max_tokens — the answer may be incomplete by construction");
    }
    if reply.stopped_by_sequence() && !settings.stop.is_empty() {
        eprintln!("! stopped on a stop sequence");
    }
    if let Some(r) = reply.reasoning.as_deref().filter(|s| !s.trim().is_empty()) {
        eprintln!("« reasoning: {} chars", r.trim().chars().count());
    }
    if let Err(e) = rt.close(&id) {
        eprintln!("warning: could not save session: {e}");
    }
    Ok(())
}

fn print_sessions() -> Res<()> {
    let dir = session::sessions_dir();
    let list = session::list_sessions(&dir);
    if list.is_empty() {
        println!("no saved sessions in {}", dir.display());
        return Ok(());
    }
    for s in list {
        println!("{}", s.line());
    }
    Ok(())
}

// --- login flows ------------------------------------------------------------

/// `--keys`: one row per provider — source (env/store), masked key, last live check.
fn print_keys() -> Res<()> {
    let rows = auth::status_all();
    println!(
        "{:<12} {:<34} {:<24} last live check",
        "provider", "source (env/store)", "key"
    );
    for row in &rows {
        let (source, key) = match (&row.source, &row.masked) {
            (Some(s), Some(k)) => (s.describe().to_string(), k.clone()),
            _ => ("—".to_string(), "—".to_string()),
        };
        let check = match &row.last_check {
            Some(c) => format!(
                "{} {} ({})",
                c.verdict,
                session::format_updated(c.at),
                c.evidence
            ),
            None => "—".to_string(),
        };
        println!(
            "{:<12} {:<34} {:<24} {}",
            row.provider.id(),
            source,
            key,
            check
        );
    }
    if rows.iter().all(|r| !r.connected()) {
        println!(
            "\nno keys configured — run `ask --login <provider>` (stored in {}, never in the app)",
            auth::auth_path().display()
        );
    }
    Ok(())
}

/// `--models`: catalog grouped by provider. Offline unless `--live`.
fn print_models(live: bool) -> Res<()> {
    let connected = auth::connected_providers();
    for &p in &Provider::ALL {
        let has = connected.contains(&p);
        println!(
            "{} ({})  {}",
            p.label(),
            p.id(),
            if has { "connected" } else { "no key" }
        );
        for m in config::MODEL_CATALOG.iter().filter(|m| m.provider == p) {
            let tag = if !has {
                "no key"
            } else if api::is_live_model(m.provider, m.id) {
                "live"
            } else {
                "refused (paid)"
            };
            println!("  {:<52} {tag}", m.id);
        }
        println!();
    }
    let missing: Vec<&str> = Provider::ALL
        .iter()
        .filter(|p| !connected.contains(p))
        .map(|p| p.id())
        .collect();
    if !missing.is_empty() {
        println!(
            "unlock a provider: {}",
            missing
                .iter()
                .map(|id| format!("ask --login {id}"))
                .collect::<Vec<_>>()
                .join(" · ")
        );
    }
    if !live {
        return Ok(());
    }
    println!("--live: GET /models per connected provider vs static catalog");
    for &p in &connected {
        let ep = match Endpoint::for_provider(p) {
            Ok(ep) => ep,
            Err(e) => {
                println!("  {}: cannot resolve key ({e})", p.id());
                continue;
            }
        };
        match api::list_model_ids(&ep) {
            Ok(up) => {
                let catalog: Vec<&str> = config::MODEL_CATALOG
                    .iter()
                    .filter(|m| m.provider == p)
                    .map(|m| m.id)
                    .collect();
                let gone: Vec<&str> = catalog
                    .iter()
                    .copied()
                    .filter(|id| !up.iter().any(|u| u == id))
                    .collect();
                let new: Vec<&str> = up
                    .iter()
                    .map(|s| s.as_str())
                    .filter(|id| !catalog.contains(id))
                    .collect();
                println!(
                    "  {}: upstream {} id(s), catalog {}",
                    p.id(),
                    up.len(),
                    catalog.len()
                );
                for id in &gone {
                    println!("    in catalog but gone upstream: {id}");
                }
                for id in &new {
                    println!("    new upstream, not in catalog: {id}");
                }
                if gone.is_empty() && new.is_empty() {
                    println!("    no drift");
                }
            }
            Err(e) => println!("  {}: live list failed: {e}", p.id()),
        }
    }
    Ok(())
}

/// `--logout`: drop the key from the local store. An env var is out of reach.
fn logout(id: &str) -> Res<()> {
    let provider = Provider::parse(id)?;
    match auth::disconnect(provider) {
        Ok(true) => println!(
            "{}: key removed from {}",
            provider.label(),
            auth::auth_path().display()
        ),
        Ok(false) => println!(
            "{}: nothing stored in {} (an env var, if any, stays — unset ${} yourself)",
            provider.label(),
            auth::auth_path().display(),
            provider.env_var()
        ),
        Err(e) => return Err(e),
    }
    Ok(())
}

/// `--verify-login`: live-recheck everything configured.
fn verify_login() -> Res<()> {
    let mut configured = Vec::new();
    for &p in &Provider::ALL {
        match auth::resolve(p) {
            Ok(r) => configured.push((p, r.source.describe())),
            Err(_) => println!("{:<12} — not configured (ask --login {})", p.id(), p.id()),
        }
    }
    if configured.is_empty() {
        return Err(format!(
            "no keys configured — run `ask --login <provider>`; keys live in {}",
            auth::auth_path().display()
        ));
    }
    let mut rejected = 0;
    for (p, source) in configured {
        match auth::recheck(p) {
            Ok(CheckResult::Confirmed { evidence }) => {
                println!("{:<12} CONFIRMED    [{source}] {evidence}", p.id())
            }
            Ok(CheckResult::Rejected { http, msg }) => {
                rejected += 1;
                println!("{:<12} REJECTED     [{source}] HTTP {http}: {msg}", p.id())
            }
            Ok(CheckResult::Unreachable { msg }) => {
                println!("{:<12} UNREACHABLE  [{source}] {msg}", p.id())
            }
            Err(e) => println!("{:<12} ERROR        {e}", p.id()),
        }
    }
    if rejected > 0 {
        return Err(format!(
            "{rejected} configured key(s) were rejected by their provider"
        ));
    }
    Ok(())
}

/// `--login`: hidden prompt (or stdin), live check, 0600 store.
fn login(provider_arg: Option<&str>, key_stdin: bool) -> Res<()> {
    let provider = match provider_arg.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Provider::parse(s)?,
        None if key_stdin => {
            return Err("--key-stdin needs a provider: ask --login glm --key-stdin".into())
        }
        None => pick_provider()?,
    };
    let key = if key_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("cannot read stdin: {e}"))?;
        buf.trim().to_string()
    } else {
        prompt_key(provider)?
    };
    if key.is_empty() {
        return Err("empty key — nothing connected".into());
    }
    println!("checking {} key live…", provider.label());
    match auth::connect(provider, &key)? {
        (CheckResult::Confirmed { evidence }, true) => {
            println!("CONFIRMED — {} key works: {evidence}", provider.label());
            println!(
                "saved to {} (mode 0600, never committed)",
                auth::auth_path().display()
            );
        }
        (CheckResult::Unreachable { msg }, true) => {
            println!(
                "{}: provider unreachable ({msg}) — key saved UNVERIFIED; run `ask --verify-login` later",
                provider.label()
            );
        }
        (CheckResult::Rejected { http, msg }, false) => {
            return Err(format!("REJECTED (HTTP {http}): {msg} — key NOT saved"));
        }
        _ => unreachable!("connect returns only the verdicts above"),
    }
    Ok(())
}

/// Bare `--login` with a terminal: numbered provider picker.
fn pick_provider() -> Res<Provider> {
    if !std::io::stdin().is_terminal() {
        return Err("specify a provider: ask --login glm|deepseek|openrouter".into());
    }
    println!("Which provider do you want to connect?");
    for (i, p) in Provider::ALL.iter().enumerate() {
        println!("  {}. {} ({})", i + 1, p.label(), p.id());
    }
    print!("1-3: ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read choice: {e}"))?;
    let idx: usize = line
        .trim()
        .parse()
        .map_err(|_| format!("not a number: {}", line.trim()))?;
    Provider::ALL
        .get(idx.wrapping_sub(1))
        .copied()
        .ok_or_else(|| format!("no provider #{idx} (1-3)"))
}

/// Reads an API key without echo: raw mode, one char at a time, `*` shown
/// per char. Esc cancels; Ctrl-C restores the terminal and cancels. No new
/// dependencies — plain crossterm, which the TUI already pulls in.
fn prompt_key(provider: Provider) -> Res<String> {
    use crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    if !std::io::stdin().is_terminal() {
        return Err("no terminal for a hidden prompt — use --key-stdin".into());
    }
    println!(
        "{} API key (input hidden; Enter submits, Esc cancels):",
        provider.label()
    );
    enable_raw_mode().map_err(|e| format!("cannot enable raw mode: {e}"))?;
    let mut key = String::new();
    let outcome = loop {
        match read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Enter => break Ok(key),
                KeyCode::Esc => break Err("login cancelled".into()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err("login cancelled (Ctrl-C)".into());
                }
                KeyCode::Backspace => {
                    key.pop();
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    key.push(c);
                    print!("*");
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
                _ => {}
            },
            Ok(_) => {}
            Err(e) => break Err(format!("cannot read key input: {e}")),
        }
    };
    disable_raw_mode().map_err(|e| format!("cannot restore terminal: {e}"))?;
    println!();
    outcome.map(|k| k.trim().to_string())
}

/// Positional args, or stdin when it is piped in. Empty (and a TTY) means
/// "open the chat TUI" — same convention as before.
/// `--mcp` if given, else the default address of `--mcp-serve`.
fn mcp_url(cli: &Cli) -> String {
    cli.mcp
        .clone()
        .unwrap_or_else(|| format!("http://127.0.0.1:{}/mcp", cli.mcp_port))
}

fn read_question(args: &[String]) -> Res<String> {
    if !args.is_empty() {
        return Ok(args.join(" "));
    }
    if std::io::stdin().is_terminal() {
        return Ok(String::new());
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("cannot read stdin: {e}"))?;
    Ok(buf.trim().to_string())
}
