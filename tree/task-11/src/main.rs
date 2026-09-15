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
mod memory;
mod render;
mod runtime;
mod session;
mod strategy;
mod tokens;
mod tui;
mod verify;

fn main() {
    if let Err(e) = cli::run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
