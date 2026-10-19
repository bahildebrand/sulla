mod claude;
mod copilot;

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use color_eyre::{Result, eyre::eyre};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub use claude::Claude;
pub use copilot::Copilot;

use crate::app::Event;
use crate::beads::Bead;
use crate::config::StallConfig;

pub const CHAT_ID: &str = "@chat";
const REVIEW_SUFFIX: &str = "~review";

const MAX_WHEEL_EVENTS: usize = 500;
const LINES_PER_WHEEL: usize = 3;
const PROMPT_QUIET: Duration = Duration::from_secs(3);
const BELL_GRACE: Duration = Duration::from_secs(2);
const PROMPT_ROWS: usize = 15;

pub struct Launch {
    pub id: String,
    pub session: String,
    pub bead: bool,
    pub worktree: PathBuf,
    pub resume: bool,
    pub prompt: Option<String>,
}

pub trait Backend: Send + Sync {
    fn name(&self) -> &str;
    fn command(&self, launch: &Launch) -> CommandBuilder;
}

pub fn task_prompt(bead: &Bead, parent: Option<&str>) -> String {
    let id = &bead.id;
    let mut desc = if bead.description.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n{}", bead.description.trim())
    };
    if let Some(p) = parent {
        desc.push_str(&format!(
            "\n\nThis bead depends on {p}, which is not merged yet. Your branch is stacked on \
             sulla/{p}, so its changes are already in your worktree. Only commit work for {id}."
        ));
    }
    format!(
        "You are working on bead {id}: {title}{desc}\n\n\
         You are in a dedicated git worktree on branch sulla/{id}. Commit your work to this \
         branch. Do not switch branches, rebase, or touch other worktrees. Rust builds are cached \
         with kache through RUSTC_WRAPPER, leave that alone.\n\n\
         When the task is complete and committed, run \
         `bd set-state {id} sulla=review --reason \"ready for review\"` then stop and wait. \
         A human will review the diff and may send feedback.",
        title = bead.title,
    )
}

pub fn review_id(bead_id: &str) -> String {
    format!("{bead_id}{REVIEW_SUFFIX}")
}

pub fn reviewed_bead(agent_id: &str) -> Option<&str> {
    agent_id.strip_suffix(REVIEW_SUFFIX)
}

pub fn review_prompt(bead: &Bead, base: &str) -> String {
    let id = &bead.id;
    let desc = if bead.description.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n{}", bead.description.trim())
    };
    format!(
        "You are reviewing work on bead {id}: {title}{desc}\n\n\
         Another agent implemented this on branch sulla/{id}, which is checked out in this \
         worktree. Inspect `git diff {base}...HEAD` and `git log {base}..HEAD`. Check \
         correctness, bugs, edge cases, and whether the acceptance criteria above are met. Run \
         the build and tests. Do NOT edit tracked files, commit, rebase, or change branches. \
         Rust builds are cached with kache through RUSTC_WRAPPER, leave that alone.\n\n\
         Leave line-anchored review comments the way a human reviewer does in sulla. Write a \
         JSON file outside the worktree at /tmp/sulla-review-{id}.json with exactly this \
         shape:\n\
         {{\"verdict\":\"approve\"|\"changes\",\"comments\":[{{\"file\":\"path/relative/to/repo\",\
         \"side\":\"new\"|\"old\"|\"file\",\"line\":<number or null>,\"comment\":\"actionable text\"}}]}}\n\
         Use side \"new\" for added or context lines with the line number in the new file, \
         \"old\" for removed lines with the line number in the old file, and \"file\" with \
         line null for file-level remarks. Use file \"COMMIT_MSG\" for remarks about commit \
         messages. Line numbers must match the diff. Use verdict \"approve\" only if nothing \
         needs to change.\n\n\
         Then post it with `bd comments add {id} -f /tmp/sulla-review-{id}.json`, run \
         `bd set-state {id} sulla=review --reason \"ai review done\"`, and stop.",
        title = bead.title,
    )
}

pub fn chat_prompt(base: &str, max_agents: usize) -> String {
    format!(
        "You are a planning assistant inside sulla, a TUI that runs coding agents off a beads \
         (`bd`) queue. You are in the main checkout of the repo on branch {base}. The user wants \
         to talk through work and usually have you turn it into beads that sulla will run. \
         Do not implement code changes yourself unless the user explicitly asks.\n\n\
         How sulla works:\n\
         - Board columns come from `sulla:<state>` labels: Backlog (`sulla:backlog`, never auto \
         scheduled), Pending (no sulla label, or `sulla:held` which waits for the user to \
         release it), In Progress (`sulla:running`, or `sulla:reviewing` while an AI reviewer \
         checks it), Ready for Review (`sulla:review`), Done \
         (closed).\n\
         - Sulla claims ready Pending beads (open, no unmet blockers, not epics) in priority \
         order (P0 first, then oldest) and runs up to {max_agents} agents at once.\n\
         - Each bead gets a fresh agent in its own git worktree on branch `sulla/<id>` off \
         {base}. That agent sees only the bead title and description, so every description \
         must be self-contained: goal, relevant files, constraints, and acceptance criteria.\n\
         - The human reviews each diff in sulla and merges it into {base}, which closes the \
         bead.\n\n\
         Creating beads:\n\
         - `bd create \"Title\" -d \"description\" -p <0-4> -t <task|bug|feature> -l \
         sulla:backlog --silent` prints the new id. Always stage new beads in Backlog first so \
         sulla does not start them before the plan is wired up.\n\
         - Order work with `bd dep add <blocked-id> <blocker-id>`; sulla only starts a bead once \
         its blockers are closed. Prefer small, independently mergeable beads, and avoid \
         parallel beads that edit the same files.\n\
         - Inspect existing work with `bd list`, `bd ready`, and `bd show <id>`.\n\
         - When the user approves, release beads to Pending with \
         `bd update <id> --remove-label sulla:backlog`.\n\
         - Never add or remove `sulla:running`, `sulla:reviewing`, or `sulla:review` labels, \
         never claim beads, and never close beads. Sulla manages those.\n\n\
         Reply with one short line saying you are ready, then wait for the user."
    )
}

pub fn chat_session(repo: &std::path::Path) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    session_id(repo, &format!("{CHAT_ID}{nonce}"))
}

pub fn session_id(repo: &std::path::Path, bead_id: &str) -> String {
    fn fnv(seed: u64, bytes: &[u8]) -> u64 {
        bytes
            .iter()
            .fold(seed, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
    }
    let key = format!("{}\0{bead_id}", repo.display());
    let a = fnv(0xcbf29ce484222325, key.as_bytes());
    let b = fnv(a ^ 0x9e3779b97f4a7c15, key.as_bytes());
    let a = (a & 0xffff_ffff_ffff_0fff) | 0x4000;
    let b = (b & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        a >> 32,
        (a >> 16) & 0xffff,
        a & 0xffff,
        b >> 48,
        b & 0xffff_ffff_ffff
    )
}

#[derive(Default)]
pub struct Bell(bool);

impl vt100::Callbacks for Bell {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.0 = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stall {
    Waiting,
    Stalled,
}

struct Activity {
    last_output: Instant,
    bell: Option<Instant>,
}

impl Activity {
    fn output(&mut self, now: Instant, rang: bool) {
        if rang {
            self.bell = Some(now);
        } else if self
            .bell
            .is_some_and(|b| now.saturating_duration_since(b) > BELL_GRACE)
        {
            self.bell = None;
        }
        self.last_output = now;
    }
}

pub fn classify(idle: Duration, bell: bool, prompt: bool, cfg: &StallConfig) -> Option<Stall> {
    if idle >= PROMPT_QUIET && (bell || prompt) {
        Some(Stall::Waiting)
    } else if cfg.minutes > 0 && idle >= Duration::from_secs(cfg.minutes * 60) {
        Some(Stall::Stalled)
    } else {
        None
    }
}

fn shows_prompt(contents: &str, patterns: &[String]) -> bool {
    let lines: Vec<&str> = contents.trim_end().lines().collect();
    let tail = &lines[lines.len().saturating_sub(PROMPT_ROWS)..];
    tail.iter().any(|l| {
        patterns
            .iter()
            .any(|p| !p.is_empty() && l.contains(p.as_str()))
    })
}

fn is_box_char(c: char) -> bool {
    ('\u{2500}'..='\u{257f}').contains(&c)
}

fn is_chrome(l: &str) -> bool {
    let t = l.trim();
    t.starts_with(['╭', '╰', '┌', '└'])
        || (!t.is_empty() && t.chars().all(|c| is_box_char(c) || c == '-'))
}

fn output_end(lines: &[&str]) -> usize {
    let mut end = lines.len();
    if let Some(bottom) = lines.iter().rposition(|l| is_chrome(l))
        && lines.len() - bottom <= 12
    {
        end = bottom;
        let mut gap = 0;
        for j in (0..bottom).rev() {
            if is_chrome(lines[j]) {
                end = j;
                gap = 0;
            } else {
                gap += 1;
                if gap > 3 {
                    break;
                }
            }
        }
    }
    end
}

fn output_lines(text: &str) -> impl Iterator<Item = &str> {
    let lines: Vec<&str> = text.lines().collect();
    let end = output_end(&lines);
    lines
        .into_iter()
        .take(end)
        .rev()
        .map(|l| l.trim_matches(|c: char| c.is_whitespace() || is_box_char(c)))
        .filter(|l| !l.is_empty())
}

pub fn last_line(text: &str) -> Option<String> {
    output_lines(text).next().map(String::from)
}

const RATE_LIMIT_TAIL: usize = 8;
const RATE_LIMIT_PHRASES: &[&str] = &[
    "too many requests",
    "rate limit exceeded",
    "rate limit reached",
    "rate limited",
    "rate-limited",
    "rate_limit_error",
    "rate_limit_exceeded",
    "hit a rate limit",
    "hit your rate limit",
    "hit the rate limit",
    "usage limit reached",
    "usage limit exceeded",
    "limit reached|",
    "quota exceeded",
    "exceeded your quota",
    "exceeded your current quota",
    "insufficient_quota",
    "resource_exhausted",
    "error: 429",
    "error 429",
    "status 429",
    "status: 429",
    "(429)",
    "429 too many",
];

pub fn rate_limit(text: &str) -> Option<(String, usize)> {
    let hits: Vec<&str> = output_lines(text)
        .take(RATE_LIMIT_TAIL)
        .filter(|l| {
            let l = l.to_lowercase();
            RATE_LIMIT_PHRASES.iter().any(|p| l.contains(p))
        })
        .collect();
    hits.first().map(|l| (l.to_string(), hits.len()))
}

const LOG_TAIL: usize = 5000;

pub fn log_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.log"))
}

pub fn open_log(path: &Path, id: &str) -> Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    write!(
        f,
        "\x1b[0m\r\n--- sulla: {id} session started {now} ---\r\n"
    )?;
    Ok(f)
}

pub fn render_log(bytes: &[u8], rows: u16, cols: u16) -> String {
    let mut p = vt100::Parser::new(rows.max(1), cols.max(1), LOG_TAIL);
    p.process(bytes);
    let screen = p.screen_mut();
    screen.set_scrollback(usize::MAX);
    let back = screen.scrollback();
    let total = back + screen.size().0 as usize;
    let mut lines: Vec<String> = Vec::new();
    let mut joined = false;
    let mut next = 0;
    while next < total {
        let off = back.saturating_sub(next);
        screen.set_scrollback(off);
        let top = back - off;
        let skip = next - top;
        let rows: Vec<String> = screen.rows(0, cols).collect();
        for (i, row) in rows.into_iter().enumerate().skip(skip) {
            let row = row.trim_end().to_string();
            match lines.last_mut() {
                Some(last) if joined => last.push_str(&row),
                _ => lines.push(row),
            }
            joined = screen.row_wrapped(i as u16);
            next += 1;
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let start = lines.len().saturating_sub(LOG_TAIL);
    lines[start..].join("\n")
}

static NEXT_GEN: AtomicU64 = AtomicU64::new(0);

pub struct Agent {
    pub parser: Arc<Mutex<vt100::Parser<Bell>>>,
    pub alive: bool,
    activity: Arc<Mutex<Activity>>,
    pub generation: u64,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

pub fn command(
    backend: &dyn Backend,
    launch: &Launch,
    kache: bool,
    slot: Option<usize>,
) -> CommandBuilder {
    let mut cmd = backend.command(launch);
    cmd.cwd(&launch.worktree);
    cmd.env("TERM", "xterm-256color");
    if launch.bead {
        cmd.env(
            "SULLA_BEAD",
            reviewed_bead(&launch.id).unwrap_or(&launch.id),
        );
    }
    if let Some(slot) = slot {
        cmd.env("SULLA_SLOT", slot.to_string());
    }
    if kache {
        cmd.env("RUSTC_WRAPPER", "kache");
    }
    cmd
}

impl Agent {
    pub fn spawn(
        cmd: CommandBuilder,
        id: &str,
        rows: u16,
        cols: u16,
        mut log: Option<File>,
        tx: Sender<Event>,
    ) -> Result<Self> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system().openpty(size).map_err(|e| eyre!(e))?;
        let child = pair.slave.spawn_command(cmd).map_err(|e| eyre!(e))?;
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            5000,
            Bell::default(),
        )));
        let mut reader = pair.master.try_clone_reader().map_err(|e| eyre!(e))?;
        let writer = pair.master.take_writer().map_err(|e| eyre!(e))?;
        let id = id.to_string();
        let generation = NEXT_GEN.fetch_add(1, Ordering::Relaxed);
        let p = parser.clone();
        let activity = Arc::new(Mutex::new(Activity {
            last_output: Instant::now(),
            bell: None,
        }));
        let act = activity.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Some(f) = &mut log
                            && f.write_all(&buf[..n]).is_err()
                        {
                            log = None;
                        }
                        let rang = {
                            let mut p = p.lock().unwrap();
                            p.process(&buf[..n]);
                            std::mem::take(&mut p.callbacks_mut().0)
                        };
                        act.lock().unwrap().output(Instant::now(), rang);
                        if tx.send(Event::AgentOutput).is_err() {
                            return;
                        }
                        if rang && tx.send(Event::AgentBell(id.clone())).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Event::AgentExited(id, generation));
        });

        Ok(Self {
            parser,
            alive: true,
            activity,
            generation,
            writer,
            master: pair.master,
            child,
        })
    }

    pub fn idle(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.activity.lock().unwrap().last_output)
    }

    pub fn stall(&self, now: Instant, cfg: &StallConfig) -> Option<Stall> {
        if !self.alive {
            return None;
        }
        let (idle, bell) = {
            let a = self.activity.lock().unwrap();
            (
                now.saturating_duration_since(a.last_output),
                a.bell.is_some(),
            )
        };
        let prompt = idle >= PROMPT_QUIET
            && !bell
            && shows_prompt(
                &self.parser.lock().unwrap().screen().contents(),
                &cfg.prompts,
            );
        classify(idle, bell, prompt, cfg)
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if self.alive {
            let _ = self.writer.write_all(bytes);
            let _ = self.writer.flush();
        }
    }

    pub fn submit(&mut self, text: &str) {
        let bracketed = self.parser.lock().unwrap().screen().bracketed_paste();
        let mut bytes = crate::ui::keys::paste(text, bracketed);
        bytes.push(b'\r');
        self.write(&bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
    }

    pub fn scroll(&mut self, delta: isize) {
        let (rows, cols) = self.parser.lock().unwrap().screen().size();
        self.scroll_at(delta, cols / 2, rows / 2);
    }

    pub fn scroll_at(&mut self, delta: isize, col: u16, row: u16) {
        let mut p = self.parser.lock().unwrap();
        let screen = p.screen();
        let (mode, enc, cur) = (
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
            screen.scrollback() as isize,
        );
        if self.alive && mode != vt100::MouseProtocolMode::None {
            drop(p);
            let event = crate::ui::keys::wheel(delta > 0, col, row, enc);
            let n = delta.unsigned_abs().div_ceil(LINES_PER_WHEEL);
            self.write(&event.repeat(n.min(MAX_WHEEL_EVENTS)));
            return;
        }
        p.screen_mut()
            .set_scrollback(cur.saturating_add(delta).max(0) as usize);
    }

    pub fn scroll_reset(&mut self) {
        self.parser.lock().unwrap().screen_mut().set_scrollback(0);
    }

    pub fn last_line(&self) -> Option<String> {
        last_line(&self.parser.lock().unwrap().screen().contents())
    }

    pub fn rate_limit(&self) -> Option<(String, usize)> {
        rate_limit(&self.parser.lock().unwrap().screen().contents())
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.alive = false;
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if self.alive {
            self.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exit_generation(rx: &std::sync::mpsc::Receiver<Event>) -> u64 {
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(Event::AgentExited(_, g)) => return g,
                Ok(_) => {}
                Err(e) => panic!("no exit event: {e}"),
            }
        }
    }

    #[test]
    fn exit_events_carry_the_spawned_generation() {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawn = || {
            let mut cmd = CommandBuilder::new("sh");
            cmd.args(["-c", "exit 0"]);
            Agent::spawn(cmd, "x-1", 10, 40, None, tx.clone()).unwrap()
        };
        let old = spawn();
        let old_gen = exit_generation(&rx);
        let new = spawn();
        assert_eq!(old_gen, old.generation);
        assert_ne!(old.generation, new.generation);
        assert_eq!(exit_generation(&rx), new.generation);
    }

    #[test]
    fn session_id_is_stable_uuid() {
        let repo = std::path::Path::new("/r");
        let a = session_id(repo, "x-1");
        assert_eq!(a, session_id(repo, "x-1"));
        assert_ne!(a, session_id(repo, "x-2"));
        let parts: Vec<_> = a.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12]);
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn bell_ignores_osc_terminator() {
        let mut p = vt100::Parser::new_with_callbacks(24, 80, 0, Bell::default());
        p.process(b"\x1b]0;title\x07hello");
        assert!(!p.callbacks().0);
        p.process(b"\x07");
        assert!(p.callbacks().0);
    }

    #[test]
    fn classify_prefers_waiting_and_respects_threshold() {
        let cfg = StallConfig::default();
        let s = Duration::from_secs;
        assert_eq!(classify(s(1), true, true, &cfg), None);
        assert_eq!(classify(s(4), true, false, &cfg), Some(Stall::Waiting));
        assert_eq!(classify(s(4), false, true, &cfg), Some(Stall::Waiting));
        assert_eq!(classify(s(299), false, false, &cfg), None);
        assert_eq!(classify(s(300), false, false, &cfg), Some(Stall::Stalled));
        assert_eq!(classify(s(900), false, true, &cfg), Some(Stall::Waiting));
        let off = StallConfig { minutes: 0, ..cfg };
        assert_eq!(classify(s(9000), false, false, &off), None);
    }

    #[test]
    fn bell_clears_once_output_resumes() {
        let t0 = Instant::now();
        let mut a = Activity {
            last_output: t0,
            bell: None,
        };
        a.output(t0, true);
        a.output(t0 + Duration::from_secs(1), false);
        assert!(a.bell.is_some());
        a.output(t0 + Duration::from_secs(3), false);
        assert!(a.bell.is_none());
    }

    #[test]
    fn prompt_only_matches_bottom_rows() {
        let pats = StallConfig::default().prompts;
        let prompt = "Do you want to run this command?\n 1. Yes\n 3. No, and tell Copilot\n\n\n";
        assert!(shows_prompt(prompt, &pats));
        let old = format!("Do you want to\n{}", "line\n".repeat(PROMPT_ROWS));
        assert!(!shows_prompt(&old, &pats));
        assert!(!shows_prompt("Do you want to", &[String::new()]));
    }

    #[test]
    fn last_line_skips_input_box_and_footer() {
        let boxed = "● Reading files\n  ⎿ src/app.rs\n\n● Running tests\n\n╭──────╮\n│ >    │\n╰──────╯\n  ? for shortcuts\n\n";
        assert_eq!(last_line(boxed).as_deref(), Some("● Running tests"));
        let ruled = "● Done editing\n─────────\n> \n─────────\n  model · 12k tokens\n";
        assert_eq!(last_line(ruled).as_deref(), Some("● Done editing"));
        assert_eq!(last_line("plain\noutput  \n\n").as_deref(), Some("output"));
        assert_eq!(last_line("\n \n"), None);
    }

    #[test]
    fn rate_limit_only_matches_recent_output() {
        let hit = "● Working\n\n  API Error: 429 {\"type\":\"rate_limit_error\"}\n\n╭──────╮\n│ >    │\n╰──────╯\n";
        assert_eq!(
            rate_limit(hit),
            Some(("API Error: 429 {\"type\":\"rate_limit_error\"}".into(), 1))
        );
        let twice = "✗ Too Many Requests\nretrying\n✗ Too Many Requests\n";
        assert_eq!(rate_limit(twice), Some(("✗ Too Many Requests".into(), 2)));
        let old = format!("rate limit exceeded\n{}", "line\n".repeat(RATE_LIMIT_TAIL));
        assert_eq!(rate_limit(&old), None);
        assert_eq!(
            rate_limit("fixed line 429 in parser\nquota handling\n"),
            None
        );
    }

    #[test]
    fn render_log_keeps_scrollback_and_joins_wraps() {
        let mut bytes = Vec::new();
        for i in 0..10 {
            bytes.extend(format!("line {i}\r\n").as_bytes());
        }
        bytes.extend(b"abcdefghij\x1b[1;31mred\x1b[0m\r\n\r\n");
        let text = render_log(&bytes, 4, 8);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 11);
        assert_eq!(lines[0], "line 0");
        assert_eq!(lines[9], "line 9");
        assert_eq!(lines[10], "abcdefghijred");
    }

    #[test]
    fn open_log_appends_sessions() {
        let dir = std::env::temp_dir().join(format!("sulla-log-{}", std::process::id()));
        let path = log_path(&dir.join("logs"), "x-1");
        open_log(&path, "x-1").unwrap().write_all(b"first").unwrap();
        open_log(&path, "x-1")
            .unwrap()
            .write_all(b"second")
            .unwrap();
        let text = render_log(&std::fs::read(&path).unwrap(), 24, 80);
        assert_eq!(text.matches("session started").count(), 2);
        assert!(text.contains("first") && text.contains("second"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn review_id_round_trips() {
        let r = review_id("x-1");
        assert_eq!(r, "x-1~review");
        assert_eq!(reviewed_bead(&r), Some("x-1"));
        assert_eq!(reviewed_bead("x-1"), None);
    }
}
