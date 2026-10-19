use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use color_eyre::Result;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use serde::{Deserialize, Serialize};

use crate::app::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Queued,
    Running,
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub head: String,
    pub dirty: String,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    key: Key,
    ok: bool,
    secs: u64,
}

pub struct Check {
    pub key: Key,
    pub status: Status,
    pub log: PathBuf,
    pub host: bool,
    pub run: u64,
    pub queued: Instant,
    started: Option<Instant>,
    took: Option<Duration>,
    child: Option<Arc<Mutex<Option<Child>>>>,
}

pub struct Done {
    pub id: String,
    pub run: u64,
    pub ok: bool,
}

pub fn log_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.log"))
}

fn saved_path(log: &Path) -> PathBuf {
    log.with_extension("json")
}

impl Check {
    pub fn queued(key: Key, log: PathBuf, run: u64, host: bool) -> Self {
        Self {
            key,
            status: Status::Queued,
            log,
            host,
            run,
            queued: Instant::now(),
            started: None,
            took: None,
            child: None,
        }
    }

    pub fn restore(log: PathBuf, key: &Key) -> Option<Self> {
        let saved: Saved =
            serde_json::from_str(&std::fs::read_to_string(saved_path(&log)).ok()?).ok()?;
        if saved.key != *key {
            return None;
        }
        let mut c = Self::queued(saved.key, log, 0, false);
        c.status = if saved.ok { Status::Pass } else { Status::Fail };
        c.took = Some(Duration::from_secs(saved.secs));
        Some(c)
    }

    pub fn launch(&mut self, id: &str, mut cmd: Command, tx: Sender<Event>) -> Result<()> {
        self.started = Some(Instant::now());
        self.status = Status::Running;
        let _ = std::fs::remove_file(saved_path(&self.log));
        if let Some(parent) = self.log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let out = File::create(&self.log)?;
        let err = out.try_clone()?;
        cmd.stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .process_group(0);
        let child = Arc::new(Mutex::new(Some(cmd.spawn()?)));
        self.child = Some(child.clone());
        let (id, run) = (id.to_string(), self.run);
        std::thread::spawn(move || {
            loop {
                let done = {
                    let mut g = child.lock().unwrap();
                    let Some(c) = g.as_mut() else { return };
                    match c.try_wait() {
                        Ok(None) => None,
                        Ok(Some(s)) => Some(s.success()),
                        Err(_) => Some(false),
                    }
                    .inspect(|_| {
                        g.take();
                    })
                };
                if let Some(ok) = done {
                    let _ = tx.send(Event::CheckDone(Done { id, run, ok }));
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Ok(())
    }

    pub fn fail(&mut self, err: &str) {
        let _ = self.log.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(&self.log, format!("check did not run: {err}\n"));
        self.started.get_or_insert_with(Instant::now);
        self.finish(false, None);
    }

    pub fn finish(&mut self, ok: bool, key: Option<Key>) {
        self.child = None;
        self.took = Some(self.started.map_or(Duration::ZERO, |s| s.elapsed()));
        self.status = if ok { Status::Pass } else { Status::Fail };
        if let Some(key) = key {
            self.key = key;
        }
        let saved = Saved {
            key: self.key.clone(),
            ok,
            secs: self.took.unwrap_or_default().as_secs(),
        };
        if let Ok(json) = serde_json::to_string(&saved) {
            let _ = std::fs::write(saved_path(&self.log), json);
        }
    }

    pub fn stop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        let mut g = child.lock().unwrap();
        if let Some(c) = g.as_mut()
            && matches!(c.try_wait(), Ok(None))
        {
            let _ = Command::new("kill")
                .args(["-TERM", "--", &format!("-{}", c.id())])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    pub fn badge(&self) -> Span<'static> {
        match self.status {
            Status::Queued => Span::styled("…chk ", Style::new().fg(Color::DarkGray)),
            Status::Running => Span::styled("⟳chk ", Style::new().fg(Color::Yellow)),
            Status::Pass => Span::styled("✓chk ", Style::new().fg(Color::Green)),
            Status::Fail => Span::styled("✗chk ", Style::new().fg(Color::Red)),
        }
    }

    pub fn summary(&self) -> String {
        let secs = self.took.unwrap_or_default().as_secs();
        let s = match self.status {
            Status::Queued => "checks queued".to_string(),
            Status::Running => format!(
                "checks running {}s",
                self.started.map_or(0, |s| s.elapsed().as_secs())
            ),
            Status::Pass => format!("checks passed in {secs}s"),
            Status::Fail => format!("checks failed in {secs}s"),
        };
        if self.host { format!("{s} (host)") } else { s }
    }

    pub fn output(&self) -> String {
        std::fs::read_to_string(&self.log)
            .map(|s| s.replace("\r\n", "\n"))
            .unwrap_or_else(|e| format!("cannot read log: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{Receiver, channel};

    fn key(head: &str) -> Key {
        Key {
            head: head.into(),
            dirty: String::new(),
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sulla-check-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn start(dir: &Path, cmd: &str) -> (Check, Receiver<Event>) {
        let (tx, rx) = channel();
        let mut c = Check::queued(key("abc"), log_path(dir, "b-1"), 7, false);
        let mut sh = Command::new("sh");
        sh.args(["-c", cmd]).current_dir(dir);
        c.launch("b-1", sh, tx).unwrap();
        (c, rx)
    }

    fn done(rx: &Receiver<Event>) -> Done {
        let Ok(Event::CheckDone(d)) = rx.recv_timeout(Duration::from_secs(10)) else {
            panic!("no check result");
        };
        d
    }

    #[test]
    fn result_is_saved_and_restored_only_for_same_key() {
        let dir = tempdir("pass");
        let (mut c, rx) = start(&dir, "echo hi && pwd >&2");
        let d = done(&rx);
        assert_eq!((d.id.as_str(), d.run, d.ok), ("b-1", 7, true));
        c.finish(d.ok, None);
        assert!(c.output().starts_with("hi\n"));
        let r = Check::restore(log_path(&dir, "b-1"), &key("abc")).unwrap();
        assert_eq!(r.status, Status::Pass);
        assert!(Check::restore(log_path(&dir, "b-1"), &key("def")).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failing_command_in_chain_fails_check() {
        let dir = tempdir("fail");
        let (mut c, rx) = start(&dir, "true && echo boom && false");
        let d = done(&rx);
        assert!(!d.ok);
        c.finish(d.ok, None);
        assert_eq!(c.status, Status::Fail);
        assert_eq!(c.output(), "boom\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stop_kills_whole_command_group() {
        let dir = tempdir("stop");
        let (mut c, rx) = start(&dir, "sleep 30; sleep 30");
        c.stop();
        assert!(!done(&rx).ok);
        c.stop();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
