mod agent;
mod api;
mod auth;
mod billing;
mod branch;
mod cli;
mod complete;
mod compress;
mod config;
mod context;
mod docs_mcp;
mod facts;
mod isolation;
mod mcp;
mod mcp_agent;
mod mcp_server;
mod orchestra;
mod invariants;
mod memory;
mod profile;
mod render;
mod run;
mod runtime;
mod scheduler;
mod pipeline;
mod rag;
mod ragqa;
mod session;
mod strategy;
mod todo;
mod tokens;
mod toolchain;
mod tui;
mod verify;

fn main() {
    if let Err(e) = cli::run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
