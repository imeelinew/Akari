mod app;
mod artwork;
mod config;
mod core;
mod library;
mod player;
mod state;
mod transfer;
mod ui;
mod visual;

use anyhow::{Result, bail};
use std::{
    io::{self, IsTerminal},
    sync::{Arc, atomic::AtomicBool},
};

fn run() -> Result<()> {
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "--version" | "-v" => {
                println!("Akari {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "Akari — Rust + Ratatui music player\n\nUsage: akari\nConfig: ~/.config/akari/config.toml"
                );
                return Ok(());
            }
            _ => bail!("unknown option: {arg}"),
        }
    }
    if !cfg!(target_os = "macos") {
        bail!("Akari requires macOS");
    }
    let cfg = config::load()?;
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("interactive terminal required");
    }
    let quit = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGINT,
    ] {
        signal_hook::flag::register(signal, quit.clone())?;
    }
    let mut app = app::App::new(cfg)?;
    let result = ratatui::run(|terminal| app.run(terminal, quit));
    let saved = app.save();
    app.shutdown();
    result.and(saved)
}
fn main() {
    // Audio needs the process main thread; the UI owns its own thread.
    if let Err(error) = player::run(run) {
        eprintln!("akari: {error:#}");
        std::process::exit(1);
    }
}
