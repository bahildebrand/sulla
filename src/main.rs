mod agent;
mod app;
mod beads;
mod config;
mod git;
mod scheduler;
mod ui;

use std::io::stdout;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use clap::Parser;
use color_eyre::{Result, eyre::WrapErr};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    MouseEventKind,
};
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
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

    let gate = Arc::new(Gate::default());
    let input_tx = tx.clone();
    let input_gate = gate.clone();
    std::thread::spawn(move || {
        loop {
            if input_gate.park() {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let ev = match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(event::Event::Mouse(m))
                        if !matches!(
                            m.kind,
                            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        ) =>
                    {
                        continue;
                    }
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
    let _ = crossterm::execute!(stdout(), EnableBracketedPaste, EnableMouseCapture);
    let res = run(&mut terminal, &mut app, rx, &gate);
    app.shutdown();
    let _ = crossterm::execute!(stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
    res
}

const RUNNING: u8 = 0;
const PAUSING: u8 = 1;
const PARKED: u8 = 2;

#[derive(Default)]
struct Gate(AtomicU8);

impl Gate {
    fn park(&self) -> bool {
        let _ = self
            .0
            .compare_exchange(PAUSING, PARKED, Ordering::SeqCst, Ordering::SeqCst);
        self.0.load(Ordering::SeqCst) != RUNNING
    }

    fn pause(&self) {
        self.0.store(PAUSING, Ordering::SeqCst);
        while self.0.load(Ordering::SeqCst) != PARKED {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn resume(&self) {
        self.0.store(RUNNING, Ordering::SeqCst);
    }
}

fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: Receiver<Event>,
    gate: &Gate,
) -> Result<()> {
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
        if let Some(dir) = app.pending_edit.take() {
            let r = suspend_for_editor(terminal, gate, &dir);
            let size = terminal.size()?;
            app.on_resize(size.width, size.height);
            for ev in rx.try_iter().collect::<Vec<_>>() {
                if !matches!(ev, Event::Input(_)) {
                    app.handle(ev);
                }
            }
            app.edit_finished(r);
        }
    }
    Ok(())
}

fn suspend_for_editor(
    terminal: &mut DefaultTerminal,
    gate: &Gate,
    dir: &Path,
) -> std::io::Result<ExitStatus> {
    gate.pause();
    let _ = crossterm::execute!(stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
    let r = editor_command(&editor_spec(), dir).and_then(|mut c| c.status());
    let _ = enable_raw_mode();
    let _ = crossterm::execute!(
        stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    );
    let _ = terminal.clear();
    gate.resume();
    r
}

fn editor_spec() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "nvim".into())
}

fn editor_command(spec: &str, dir: &Path) -> std::io::Result<Command> {
    let mut parts = spec.split_whitespace();
    let bin = parts
        .next()
        .ok_or_else(|| std::io::Error::other("empty editor command"))?;
    let mut cmd = Command::new(bin);
    cmd.args(parts).current_dir(dir);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_command_splits_args() {
        let c = editor_command("nvim  -u NONE", Path::new("/tmp")).unwrap();
        assert_eq!(c.get_program(), "nvim");
        assert_eq!(c.get_args().collect::<Vec<_>>(), ["-u", "NONE"]);
        assert_eq!(c.get_current_dir(), Some(Path::new("/tmp")));
        assert!(editor_command("  ", Path::new("/tmp")).is_err());
    }
}
