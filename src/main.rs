mod api;
mod cli;
mod compare;
mod config;
mod engine;
mod news;
mod topics;
mod tui;

fn main() {
    if let Err(e) = cli::run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
