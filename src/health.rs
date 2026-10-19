use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::app::Event;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Ok(String),
    Missing,
    Failed(String),
    Off(String),
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    pub bin: String,
    pub status: Status,
}

#[derive(Debug, Clone)]
pub struct Probe {
    pub name: String,
    pub bin: String,
    pub off: Option<String>,
    pub extra: Option<Vec<String>>,
}

impl Probe {
    pub fn new(name: &str, bin: &str) -> Self {
        Self {
            name: name.into(),
            bin: bin.into(),
            off: None,
            extra: None,
        }
    }

    pub fn off(mut self, why: Option<&str>) -> Self {
        self.off = why.map(String::from);
        self
    }

    pub fn also(mut self, args: &[&str]) -> Self {
        self.extra = Some(args.iter().map(|s| s.to_string()).collect());
        self
    }

    fn run(&self) -> Tool {
        let status = match &self.off {
            Some(why) => Status::Off(why.clone()),
            None => match run(&self.bin, &["--version".to_string()]) {
                Err(s) => s,
                Ok(v) => match &self.extra {
                    Some(args) => match run(&self.bin, args) {
                        Ok(_) => Status::Ok(v),
                        Err(s) => s,
                    },
                    None => Status::Ok(v),
                },
            },
        };
        Tool {
            name: self.name.clone(),
            bin: self.bin.clone(),
            status,
        }
    }
}

fn run(bin: &str, args: &[String]) -> Result<String, Status> {
    let mut child = match Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Status::Missing),
        Err(e) => return Err(Status::Failed(e.to_string())),
    };
    let start = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() >= TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Status::Failed(format!(
                    "`{bin} {}` timed out",
                    args.join(" ")
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(Status::Failed(e.to_string())),
        }
    };
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    if !code.success() {
        let msg = first_line(&err).or_else(|| first_line(&out));
        return Err(Status::Failed(format!(
            "`{bin} {}` failed: {}",
            args.join(" "),
            msg.unwrap_or_else(|| code.to_string())
        )));
    }
    Ok(first_line(&out)
        .or_else(|| first_line(&err))
        .unwrap_or_else(|| "ok".into()))
}

fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
}

pub fn spawn(probes: Vec<Probe>, tx: Sender<Event>) {
    std::thread::spawn(move || {
        let tools = probes.iter().map(Probe::run).collect();
        let _ = tx.send(Event::Health(tools));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_missing_failed_off_and_version() {
        assert_eq!(
            Probe::new("x", "sulla-no-such-bin").run().status,
            Status::Missing
        );
        assert_eq!(
            Probe::new("x", "sulla-no-such-bin")
                .off(Some("disabled"))
                .run()
                .status,
            Status::Off("disabled".into())
        );
        assert!(matches!(
            Probe::new("x", "false").run().status,
            Status::Failed(_)
        ));
        assert!(matches!(
            Probe::new("x", "true").run().status,
            Status::Ok(_)
        ));
        assert!(matches!(
            Probe::new("x", "true").also(&["x"]).run().status,
            Status::Ok(_)
        ));
        assert!(matches!(
            Probe::new("x", "bash").also(&["-c", "echo nope >&2; exit 3"]).run().status,
            Status::Failed(m) if m.contains("nope")
        ));
    }
}
