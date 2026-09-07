mod agent;
mod api;
mod cli;
mod config;
mod render;
mod session;
mod tui;
mod verify;

fn main() {
    if let Err(e) = cli::run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
