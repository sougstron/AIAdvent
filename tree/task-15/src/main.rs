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
mod facts;
mod isolation;
mod invariants;
mod memory;
mod profile;
mod render;
mod runtime;
mod pipeline;
mod session;
mod strategy;
mod todo;
mod tokens;
mod tui;
mod verify;

fn main() {
    if let Err(e) = cli::run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
