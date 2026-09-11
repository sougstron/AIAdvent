mod agent;
mod api;
mod auth;
mod billing;
mod cli;
mod config;
mod context;
mod isolation;
mod render;
mod runtime;
mod session;
mod tui;
mod verify;

fn main() {
    if let Err(e) = cli::run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
