use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use color_eyre::Result;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

use crate::app::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Pass,
    Fail,
}

pub struct Check {
    pub head: String,
    pub status: Status,
    pub log: PathBuf,
    pub started: Instant,
    pub took: Option<Duration>,
    pid: Option<u32>,
}

pub struct Done {
    pub id: String,
    pub head: String,
    pub ok: bool,
}

impl Check {
    pub fn start(
        id: &str,
        head: &str,
        cmd: &str,
        dir: &Path,
        log: PathBuf,
        kache: bool,
        tx: Sender<Event>,
    ) -> Result<Self> {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let out = File::create(&log)?;
        let err = out.try_clone()?;
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(cmd)
            .current_dir(dir)
            .env("SULLA_BEAD", id)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .process_group(0);
        if kache {
            c.env("RUSTC_WRAPPER", "kache");
        }
        let mut child = c.spawn()?;
        let pid = child.id();
        let (id, head_s) = (id.to_string(), head.to_string());
        std::thread::spawn(move || {
            let ok = child.wait().is_ok_and(|s| s.success());
            let _ = tx.send(Event::CheckDone(Done {
                id,
                head: head_s,
                ok,
            }));
        });
        Ok(Self {
            head: head.to_string(),
            status: Status::Running,
            log,
            started: Instant::now(),
            took: None,
            pid: Some(pid),
        })
    }

    pub fn failed(head: &str, log: PathBuf, err: &str) -> Self {
        let _ = log.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(&log, format!("failed to start check: {err}\n"));
        Self {
            head: head.to_string(),
            status: Status::Fail,
            log,
            started: Instant::now(),
            took: Some(Duration::ZERO),
            pid: None,
        }
    }

    pub fn finish(&mut self, ok: bool) {
        self.pid = None;
        self.took = Some(self.started.elapsed());
        self.status = if ok { Status::Pass } else { Status::Fail };
    }

    pub fn stop(&mut self) {
        if let Some(pid) = self.pid.take() {
            let _ = Command::new("kill")
                .args(["-TERM", "--", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    pub fn badge(&self) -> Span<'static> {
        match self.status {
            Status::Running => Span::styled("⟳chk ", Style::new().fg(Color::Yellow)),
            Status::Pass => Span::styled("✓chk ", Style::new().fg(Color::Green)),
            Status::Fail => Span::styled("✗chk ", Style::new().fg(Color::Red)),
        }
    }

    pub fn summary(&self) -> String {
        match (self.status, self.took) {
            (Status::Running, _) => format!("checks running {}s", self.started.elapsed().as_secs()),
            (Status::Pass, Some(t)) => format!("checks passed in {}s", t.as_secs()),
            (Status::Fail, Some(t)) => format!("checks failed in {}s", t.as_secs()),
            (Status::Pass, None) => "checks passed".into(),
            (Status::Fail, None) => "checks failed".into(),
        }
    }

    pub fn output(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_else(|e| format!("cannot read log: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn run(cmd: &str, tag: &str) -> (Check, bool, String) {
        let dir = std::env::temp_dir().join(format!("sulla-check-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = channel();
        let mut c = Check::start("b-1", "abc", cmd, &dir, dir.join("log"), false, tx).unwrap();
        let Ok(Event::CheckDone(d)) = rx.recv_timeout(Duration::from_secs(10)) else {
            panic!("no check result");
        };
        assert_eq!((d.id.as_str(), d.head.as_str()), ("b-1", "abc"));
        c.finish(d.ok);
        let out = c.output();
        std::fs::remove_dir_all(dir).unwrap();
        (c, d.ok, out)
    }

    #[test]
    fn passing_check_captures_output_in_worktree() {
        let (c, ok, out) = run("echo hi $SULLA_BEAD && pwd >&2", "pass");
        assert!(ok);
        assert_eq!(c.status, Status::Pass);
        assert!(out.starts_with("hi b-1\n"));
        assert!(out.contains("sulla-check-pass"));
    }

    #[test]
    fn failing_command_in_chain_fails_check() {
        let (c, ok, out) = run("true && echo boom && false", "fail");
        assert!(!ok);
        assert_eq!(c.status, Status::Fail);
        assert_eq!(out, "boom\n");
    }

    #[test]
    fn stop_kills_whole_command_group() {
        let dir = std::env::temp_dir().join(format!("sulla-check-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = channel();
        let mut c = Check::start(
            "b",
            "h",
            "sleep 30; sleep 30",
            &dir,
            dir.join("log"),
            false,
            tx,
        )
        .unwrap();
        c.stop();
        let Ok(Event::CheckDone(d)) = rx.recv_timeout(Duration::from_secs(5)) else {
            panic!("check was not killed");
        };
        assert!(!d.ok);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
