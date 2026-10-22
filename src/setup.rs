use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use color_eyre::{Result, eyre::bail};

use crate::agent;
use crate::config::{Config, SandboxKind};
use crate::git::Git;
use crate::sandbox;

const TAIL_LINES: usize = 20;
const TAIL_BYTES: usize = 64 * 1024;
const POLL: Duration = Duration::from_millis(100);
const DRAIN: Duration = Duration::from_secs(1);

pub fn argv(script: &str) -> Vec<OsString> {
    ["sh", "-c", script].map(Into::into).into()
}

pub fn env(id: &str, slot: Option<usize>, kache: bool) -> Vec<(String, String)> {
    let mut env = vec![("SULLA_BEAD".to_string(), id.to_string())];
    if let Some(slot) = slot {
        env.push(("SULLA_SLOT".into(), slot.to_string()));
    }
    if kache {
        env.push(("RUSTC_WRAPPER".into(), "kache".into()));
    }
    env
}

pub struct Setup {
    pub script: String,
    pub cmd: Command,
    pub log: PathBuf,
}

pub struct Ran {
    pub script: String,
    pub ok: bool,
    pub status: String,
    pub tail: String,
    pub took: Duration,
}

impl Setup {
    pub fn run(self, id: &str, echo: bool) -> Ran {
        let started = Instant::now();
        let mut sink = Sink {
            log: open_log(&self.log)
                .inspect_err(|e| tracing::warn!("opening session log of {id}: {e:#}"))
                .ok(),
            echo,
            tail: Tail::default(),
        };
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        sink.note(&format!(
            "\x1b[0m\n--- sulla: {id} setup started {now}: {} ---\n",
            self.script
        ));
        let r = spawn(self.cmd, &mut |bytes| sink.output(bytes));
        let took = started.elapsed();
        let (ok, status) = match r {
            Ok(s) if s.success() => (true, "finished".to_string()),
            Ok(s) => (false, format!("failed ({s})")),
            Err(e) => (false, format!("did not start: {e:#}")),
        };
        sink.note(&format!(
            "--- sulla: {id} setup {status} after {}s ---\n",
            took.as_secs()
        ));
        Ran {
            script: self.script,
            ok,
            status,
            tail: sink.tail.lines(),
            took,
        }
    }
}

struct Sink {
    log: Option<File>,
    echo: bool,
    tail: Tail,
}

impl Sink {
    fn write(&mut self, bytes: &[u8]) {
        if self.echo {
            let mut o = std::io::stdout();
            let _ = o.write_all(bytes).and_then(|()| o.flush());
        }
        if let Some(f) = &mut self.log
            && f.write_all(&crlf(bytes)).is_err()
        {
            self.log = None;
        }
    }

    fn note(&mut self, text: &str) {
        self.write(text.as_bytes());
    }

    fn output(&mut self, bytes: &[u8]) {
        self.write(bytes);
        self.tail.push(bytes);
    }
}

fn spawn(mut cmd: Command, out: &mut impl FnMut(&[u8])) -> Result<ExitStatus> {
    let (mut reader, writer) = std::io::pipe()?;
    cmd.stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    let mut child = cmd.spawn()?;
    drop(cmd);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf)
            && n > 0
            && tx.send(buf[..n].to_vec()).is_ok()
        {}
    });
    let mut exited: Option<(ExitStatus, Instant)> = None;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(bytes) => out(&bytes),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if exited.is_none() {
            exited = child.try_wait()?.map(|s| (s, Instant::now()));
        }
        if exited.is_some_and(|(_, at)| at.elapsed() >= DRAIN) {
            break;
        }
    }
    match exited {
        Some((s, _)) => Ok(s),
        None => Ok(child.wait()?),
    }
}

fn open_log(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

fn crlf(bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    for &b in bytes {
        if b == b'\n' {
            v.push(b'\r');
        }
        v.push(b);
    }
    v
}

#[derive(Default)]
struct Tail(VecDeque<u8>);

impl Tail {
    fn push(&mut self, bytes: &[u8]) {
        self.0.extend(bytes);
        let over = self.0.len().saturating_sub(TAIL_BYTES);
        self.0.drain(..over);
    }

    fn lines(&self) -> String {
        let (a, b) = self.0.as_slices();
        let text = String::from_utf8_lossy(&[a, b].concat()).into_owned();
        let lines: Vec<&str> = text
            .lines()
            .map(|l| l.rsplit('\r').find(|s| !s.is_empty()).unwrap_or(""))
            .collect();
        lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
    }
}

impl Ran {
    pub fn summary(&self, id: &str) -> String {
        if self.ok {
            format!("{id}: setup finished in {}s", self.took.as_secs())
        } else {
            format!("{id}: setup {}, output is in its session log", self.status)
        }
    }

    pub fn agent_note(&self) -> String {
        let mut s = format!(
            "Note: the worktree setup hook `{}` {}, so dependencies may be missing.",
            self.script, self.status
        );
        if !self.tail.trim().is_empty() {
            s.push_str(&format!(
                " The end of its output:\n```\n{}\n```",
                self.tail.trim_end()
            ));
        }
        s.push_str(&format!(
            "\nFix what you can and re-run `{}` in the worktree when needed.",
            self.script
        ));
        s
    }
}

pub fn with_note(prompt: Option<String>, ran: Option<&Ran>) -> Option<String> {
    let Some(note) = ran.filter(|r| !r.ok).map(Ran::agent_note) else {
        return prompt;
    };
    Some(match prompt {
        Some(p) => format!("{p}\n\n{note}"),
        None => note,
    })
}

pub fn cli(repo: &Path, cfg: &Config, id: &str) -> Result<()> {
    let Some(script) = cfg.hooks.setup_script() else {
        bail!("no setup hook, set `[hooks] setup` in .sulla/config.toml");
    };
    if cfg.sandbox == SandboxKind::Incus {
        bail!(
            "sandbox = \"incus\" runs setup in the bead's slot container, use `:setup {id}` in sulla"
        );
    }
    let git = Git::new(repo.to_path_buf()).with_prefix(&cfg.branch_prefix);
    let dir = git.worktree_path(id);
    if !dir.join(".git").exists() {
        bail!("{id} has no worktree at {}", dir.display());
    }
    let setup = Setup {
        script: script.to_string(),
        cmd: sandbox::host_command(&dir, &argv(script), &env(id, None, cfg.kache)),
        log: agent::log_path(&repo.join(".sulla").join("logs"), id),
    };
    let ran = setup.run(id, true);
    git.mark_setup(id)?;
    if !ran.ok {
        bail!("{}", ran.summary(id));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sulla-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn setup(dir: &Path, script: &str) -> Setup {
        Setup {
            script: script.into(),
            cmd: sandbox::host_command(dir, &argv(script), &env("s-1", Some(2), false)),
            log: dir.join("logs").join("s-1.log"),
        }
    }

    #[test]
    fn output_goes_to_the_session_log_with_crlf() {
        let d = scratch("ok");
        let ran = setup(
            &d,
            "echo out; echo err >&2; echo \"$SULLA_BEAD $SULLA_SLOT\"",
        )
        .run("s-1", false);
        assert!(ran.ok, "{}", ran.status);
        assert_eq!(ran.tail, "out\nerr\ns-1 2");
        let log = std::fs::read_to_string(d.join("logs").join("s-1.log")).unwrap();
        assert!(log.contains("--- sulla: s-1 setup started"), "{log}");
        assert!(log.contains("out\r\nerr\r\ns-1 2\r\n"), "{log}");
        assert!(log.contains("setup finished after"), "{log}");
        assert_eq!(
            with_note(Some("task".into()), Some(&ran)).as_deref(),
            Some("task")
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn failure_keeps_the_tail_and_tells_the_agent() {
        let d = scratch("fail");
        let ran = setup(&d, "seq 1 30; printf 'a\\rb\\n'; exit 3").run("s-1", false);
        assert!(!ran.ok);
        assert!(ran.status.contains('3'), "{}", ran.status);
        let tail: Vec<&str> = ran.tail.lines().collect();
        assert_eq!(tail.len(), TAIL_LINES);
        assert_eq!(tail.first(), Some(&"12"));
        assert_eq!(tail.last(), Some(&"b"));
        assert!(ran.summary("s-1").contains("session log"));
        let p = with_note(Some("task".into()), Some(&ran)).unwrap();
        assert!(
            p.starts_with("task\n\nNote: the worktree setup hook"),
            "{p}"
        );
        assert!(p.contains("```\n12\n"), "{p}");
        let p = with_note(None, Some(&ran)).unwrap();
        assert!(p.starts_with("Note:"), "{p}");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_background_child_holding_the_pipe_does_not_block() {
        let d = scratch("bg");
        let started = Instant::now();
        let ran = setup(&d, "sleep 30 & echo done").run("s-1", false);
        assert!(ran.ok, "{}", ran.status);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(ran.tail, "done");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn missing_program_is_a_failure_not_a_panic() {
        let d = scratch("missing");
        let mut s = setup(&d, "x");
        s.cmd = Command::new(d.join("does-not-exist"));
        let ran = s.run("s-1", false);
        assert!(!ran.ok);
        assert!(ran.status.starts_with("did not start"), "{}", ran.status);
        std::fs::remove_dir_all(d).unwrap();
    }
}
