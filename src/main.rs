mod agent;
mod app;
mod beads;
mod config;
mod git;
mod scheduler;
mod ui;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use clap::Parser;
use color_eyre::{Result, eyre::WrapErr};
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste};
use ratatui::DefaultTerminal;

use app::{App, Event};
use config::Config;

#[derive(Parser)]
#[command(
    version,
    about = "Keyboard-first TUI for scheduling, steering and merging coding agents"
)]
struct Args {
    #[arg(short = 'C', long, default_value = ".")]
    repo: PathBuf,
    #[arg(long)]
    max_agents: Option<usize>,
    #[arg(long)]
    base: Option<String>,
}

fn init_logging() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let dir = directories::ProjectDirs::from("", "", "sulla")
        .map(|d| d.data_local_dir().to_path_buf())
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&dir)?;
    let (writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(dir, "sulla.log"));
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SULLA_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    Ok(guard)
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let args = Args::parse();
    let mut cfg = Config::load().wrap_err("loading config")?;
    if let Some(n) = args.max_agents {
        cfg.max_agents = n;
    }
    if args.base.is_some() {
        cfg.base_branch = args.base;
    }
    let repo = git::toplevel(&args.repo).wrap_err("not a git repository")?;
    beads::Beads::new(&repo)
        .list_all()
        .wrap_err("bd is not usable here, run `bd init` in the repo")?;
    let _guard = init_logging()?;

    let (tx, rx) = channel();
    let mut app = App::new(cfg, repo, tx.clone())?;

    let input_tx = tx.clone();
    std::thread::spawn(move || {
        loop {
            let ev = match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(e) => Event::Input(e),
                    Err(_) => continue,
                },
                _ => Event::Tick,
            };
            if input_tx.send(ev).is_err() {
                return;
            }
        }
    });

    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(stdout(), EnableBracketedPaste);
    let res = run(&mut terminal, &mut app, rx);
    app.shutdown();
    let _ = crossterm::execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    res
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, rx: Receiver<Event>) -> Result<()> {
    let size = terminal.size()?;
    app.on_resize(size.width, size.height);
    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;
        app.handle(rx.recv()?);
        while !app.quit
            && let Ok(ev) = rx.try_recv()
        {
            app.handle(ev);
        }
    }
    Ok(())
}
