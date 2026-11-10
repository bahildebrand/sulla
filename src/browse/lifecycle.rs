use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use color_eyre::{Result, eyre::eyre};

use super::Note;
use crate::config::BrowseConfig;
use crate::git;
use crate::sandbox::Sandbox;

const TAIL: usize = 5;
const HEADER: &str = "--- ";
const ATTEMPT: Duration = Duration::from_secs(10);
const CONNECT: Duration = Duration::from_secs(1);
const LINGER: Duration = Duration::from_millis(500);
const WAIT_STEP: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env: Vec<(OsString, Option<OsString>)>,
}

impl Spec {
    pub fn of(cmd: &Command) -> Self {
        Self {
            program: cmd.get_program().to_os_string(),
            args: cmd.get_args().map(|a| a.to_os_string()).collect(),
            cwd: cmd.get_current_dir().map(Path::to_path_buf),
            env: cmd
                .get_envs()
                .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
                .collect(),
        }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.args);
        if let Some(d) = &self.cwd {
            c.current_dir(d);
        }
        for (k, v) in &self.env {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
        c
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    script: String,
    spec: Spec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Tcp(SocketAddr),
    Shell(Step),
}

#[derive(Debug, Clone)]
pub struct Plan {
    up: Option<Step>,
    down: Option<Step>,
    ready: Probe,
    timeout: Duration,
    interval: Duration,
    log: PathBuf,
}

pub fn log_path(repo: &Path, bead: &str) -> PathBuf {
    repo.join(".sulla")
        .join("logs")
        .join(format!("{bead}.app.log"))
}

fn env(bead: &str, slot: usize) -> Vec<(String, String)> {
    vec![
        ("SULLA_BEAD".into(), bead.into()),
        ("SULLA_SLOT".into(), slot.to_string()),
    ]
}

fn up_argv(script: &str) -> Vec<OsString> {
    ["setsid", "nohup", "bash", "-c", script]
        .map(OsString::from)
        .into()
}

fn shell_argv(script: &str) -> Vec<OsString> {
    ["bash", "-c", script].map(OsString::from).into()
}

pub fn plan(
    cfg: &BrowseConfig,
    repo: &Path,
    bead: &str,
    slot: usize,
    sandbox: &dyn Sandbox,
) -> Result<Plan> {
    let cwd = git::worktree_dir(repo, bead);
    let env = env(bead, slot);
    let step = |script: &str, argv: Vec<OsString>| Step {
        script: script.to_string(),
        spec: Spec::of(&sandbox.command(slot, &cwd, &argv, &env)),
    };
    let ready = match cfg.ready_script() {
        Some(s) => Probe::Shell(step(s, shell_argv(s))),
        None => {
            let ip = super::loopback(slot)
                .ok_or_else(|| eyre!("slot {slot} has no loopback address"))?;
            let port = cfg
                .ports
                .first()
                .ok_or_else(|| eyre!("browse needs [incus.browse] ports"))?;
            Probe::Tcp(format!("{ip}:{port}").parse()?)
        }
    };
    Ok(Plan {
        up: cfg.up_script().map(|s| step(s, up_argv(s))),
        down: cfg.down_script().map(|s| step(s, shell_argv(s))),
        ready,
        timeout: Duration::from_secs(cfg.ready_timeout_secs),
        interval: Duration::from_secs(1),
        log: log_path(repo, bead),
    })
}

fn tcp_ready(addr: SocketAddr) -> bool {
    answers(addr, CONNECT, LINGER)
}

pub fn answers(addr: SocketAddr, connect: Duration, linger: Duration) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&addr, connect) else {
        return false;
    };
    if s.set_read_timeout(Some(linger)).is_err() {
        return false;
    }
    match s.read(&mut [0u8; 1]) {
        Ok(n) => n > 0,
        Err(e) => matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
    }
}

fn open_log(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| eyre!("opening {}: {e}", path.display()))
}

fn header(path: &Path, what: &str, script: &str) -> Result<()> {
    let mut f = open_log(path)?;
    writeln!(
        f,
        "{HEADER}{} {what}: {script}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    )?;
    Ok(())
}

fn spawn_logged(spec: &Spec, log: &Path) -> Result<Child> {
    let out = open_log(log)?;
    let err = out.try_clone()?;
    spec.command()
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
        .map_err(|e| eyre!("starting {}: {e}", spec.program.to_string_lossy()))
}

fn wait_until(child: &mut Child, deadline: Instant) -> Result<Option<ExitStatus>> {
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(Some(st));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(WAIT_STEP);
    }
}

fn reap(mut child: Child) {
    std::thread::spawn(move || child.wait());
}

pub fn tail(path: &Path) -> Vec<String> {
    let body = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = body
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with(HEADER))
        .collect();
    lines[lines.len().saturating_sub(TAIL)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

fn failure(msg: String, log: &Path) -> color_eyre::Report {
    let lines = tail(log);
    if lines.is_empty() {
        eyre!("{msg}")
    } else {
        eyre!("{msg}\n{}", lines.join("\n"))
    }
}

impl Plan {
    fn ready(&self, deadline: Instant) -> Result<bool> {
        match &self.ready {
            Probe::Tcp(addr) => Ok(tcp_ready(*addr)),
            Probe::Shell(step) => {
                let mut child = spawn_logged(&step.spec, &self.log)?;
                let deadline = deadline.min(Instant::now() + ATTEMPT);
                Ok(wait_until(&mut child, deadline)?.is_some_and(|s| s.success()))
            }
        }
    }

    pub fn has_down(&self) -> bool {
        self.down.is_some()
    }

    pub fn ensure_up(&self, force: bool, note: &mut dyn FnMut(Note)) -> Result<()> {
        let Some(up) = &self.up else {
            return Ok(());
        };
        if let Probe::Shell(step) = &self.ready {
            header(&self.log, "ready", &step.script)?;
        }
        let start = Instant::now();
        let deadline = start + self.timeout;
        if !force && self.ready(deadline)? {
            return Ok(());
        }
        note(Note::Status("starting app…".into()));
        header(&self.log, "up", &up.script)?;
        let mut child = Some(spawn_logged(&up.spec, &self.log)?);
        note(Note::Up);
        loop {
            if let Some(c) = child.as_mut()
                && let Some(st) = c.try_wait()?
            {
                child = None;
                if !st.success() {
                    return Err(failure(format!("`up` failed ({st})"), &self.log));
                }
            }
            if self.ready(deadline)? {
                if let Some(c) = child {
                    reap(c);
                }
                return Ok(());
            }
            let waited = start.elapsed();
            if waited >= self.timeout {
                if let Some(c) = child {
                    reap(c);
                }
                return Err(failure(
                    format!(
                        "app was not ready within {}s, see {}",
                        self.timeout.as_secs(),
                        self.log.display()
                    ),
                    &self.log,
                ));
            }
            note(Note::Status(format!(
                "waiting for app ({}s)",
                waited.as_secs()
            )));
            std::thread::sleep(
                self.interval
                    .min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    pub fn stop(&self, note: &mut dyn FnMut(Note)) -> Result<()> {
        let down = self
            .down
            .as_ref()
            .ok_or_else(|| eyre!("browse stop needs [incus.browse] down"))?;
        note(Note::Status("stopping app…".into()));
        header(&self.log, "down", &down.script)?;
        let mut child = spawn_logged(&down.spec, &self.log)?;
        match wait_until(&mut child, Instant::now() + self.timeout)? {
            Some(st) if st.success() => {
                note(Note::Down);
                Ok(())
            }
            Some(st) => Err(failure(format!("`down` failed ({st})"), &self.log)),
            None => Err(failure(
                format!("`down` did not finish within {}s", self.timeout.as_secs()),
                &self.log,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::sandbox::{self, Worktree};

    static N: AtomicU32 = AtomicU32::new(0);

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sulla-life-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn host_step(script: &str, dir: &Path) -> Step {
        Step {
            script: script.into(),
            spec: Spec::of(&sandbox::host_command(dir, &shell_argv(script), &[])),
        }
    }

    fn quick(dir: &Path, up: Option<&str>, ready: Probe, timeout_ms: u64) -> Plan {
        Plan {
            up: up.map(|s| host_step(s, dir)),
            down: None,
            ready,
            timeout: Duration::from_millis(timeout_ms),
            interval: Duration::from_millis(10),
            log: log_path(dir, "a-1"),
        }
    }

    fn text(n: Note) -> String {
        match n {
            Note::Status(s) => s,
            Note::Up => "<up>".into(),
            Note::Down => "<down>".into(),
        }
    }

    fn args(s: &Spec) -> Vec<String> {
        s.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn envs(s: &Spec) -> Vec<(String, String)> {
        s.env
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.as_ref().unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn plan_runs_scripts_in_the_bead_worktree_with_slot_env() {
        let cfg = BrowseConfig {
            ports: vec![8000],
            up: Some("docker compose up -d".into()),
            down: Some("docker compose down".into()),
            ready: Some("curl -fsS http://127.0.0.1:8000/health".into()),
            ..BrowseConfig::default()
        };
        let repo = Path::new("/r");
        let p = plan(&cfg, repo, "a-1", 2, &Worktree).unwrap();
        let wt = Path::new("/r/.sulla/worktrees/a-1");
        let want_env = [
            ("SULLA_BEAD".to_string(), "a-1".to_string()),
            ("SULLA_SLOT".to_string(), "2".to_string()),
        ];
        let up = p.up.as_ref().unwrap();
        assert_eq!(up.spec.program, "setsid");
        assert_eq!(
            args(&up.spec),
            ["nohup", "bash", "-c", "docker compose up -d"]
        );
        assert_eq!(up.spec.cwd.as_deref(), Some(wt));
        assert_eq!(envs(&up.spec), want_env);
        let down = p.down.as_ref().unwrap();
        assert_eq!(down.spec.program, "bash");
        assert_eq!(args(&down.spec), ["-c", "docker compose down"]);
        assert_eq!(envs(&down.spec), want_env);
        let Probe::Shell(ready) = &p.ready else {
            panic!("{:?}", p.ready)
        };
        assert_eq!(ready.spec.program, "bash");
        assert_eq!(
            args(&ready.spec),
            ["-c", "curl -fsS http://127.0.0.1:8000/health"]
        );
        assert_eq!(ready.spec.cwd.as_deref(), Some(wt));
        assert_eq!(envs(&ready.spec), want_env);
        assert_eq!(p.timeout, Duration::from_secs(300));
        assert_eq!(p.interval, Duration::from_secs(1));
        assert_eq!(p.log, Path::new("/r/.sulla/logs/a-1.app.log"));
    }

    #[test]
    fn plan_defaults_ready_to_first_port_on_slot_loopback() {
        let cfg = BrowseConfig {
            ports: vec![8000, 8001],
            up: Some("make up".into()),
            ..BrowseConfig::default()
        };
        let p = plan(&cfg, Path::new("/r"), "a-1", 3, &Worktree).unwrap();
        assert_eq!(p.ready, Probe::Tcp("127.0.100.4:8000".parse().unwrap()));
        assert!(p.up.is_some());
        assert!(p.down.is_none());
        let cfg = BrowseConfig {
            up: Some("make up".into()),
            ..BrowseConfig::default()
        };
        assert!(plan(&cfg, Path::new("/r"), "a-1", 3, &Worktree).is_err());
    }

    #[test]
    fn tcp_ready_needs_a_listener_that_keeps_the_connection() {
        let held = TcpListener::bind("127.0.100.250:0").unwrap();
        let dropped = TcpListener::bind("127.0.100.250:0").unwrap();
        let (held_addr, dropped_addr) = (held.local_addr().unwrap(), dropped.local_addr().unwrap());
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let (s, _) = held.accept().unwrap();
            let _ = wait.recv();
            drop(s);
        });
        let dropper = std::thread::spawn(move || drop(dropped.accept().unwrap()));
        assert!(tcp_ready(held_addr));
        release.send(()).unwrap();
        holder.join().unwrap();
        assert!(!tcp_ready(dropped_addr));
        dropper.join().unwrap();
        assert!(!tcp_ready(held_addr));
    }

    #[test]
    fn no_up_skips_probe_and_log() {
        let d = scratch("noup");
        let p = quick(&d, None, Probe::Shell(host_step("exit 1", &d)), 10);
        let mut seen = Vec::new();
        p.ensure_up(false, &mut |n| seen.push(text(n))).unwrap();
        assert!(seen.is_empty());
        assert!(!p.log.exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn ready_app_is_not_started_again() {
        let d = scratch("ready");
        let p = quick(
            &d,
            Some("touch started"),
            Probe::Shell(host_step("echo up", &d)),
            1000,
        );
        let mut seen = Vec::new();
        p.ensure_up(false, &mut |n| seen.push(text(n))).unwrap();
        assert!(seen.is_empty());
        assert!(!d.join("started").exists());
        assert_eq!(tail(&p.log).last().map(String::as_str), Some("up"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn up_runs_then_ready_is_polled_until_it_passes() {
        let d = scratch("poll");
        let p = quick(
            &d,
            Some("echo starting; touch started"),
            Probe::Shell(host_step("test -e started", &d)),
            5000,
        );
        let mut seen = Vec::new();
        p.ensure_up(false, &mut |n| seen.push(text(n))).unwrap();
        assert_eq!(seen.first().map(String::as_str), Some("starting app…"));
        assert_eq!(seen.get(1).map(String::as_str), Some("<up>"));
        assert!(d.join("started").exists());
        let log = std::fs::read_to_string(&p.log).unwrap();
        assert!(log.contains("ready: test -e started"), "{log}");
        assert!(log.contains("up: echo starting; touch started"), "{log}");
        assert!(log.contains("starting\n"), "{log}");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn foreground_up_does_not_block() {
        let d = scratch("fg");
        let p = quick(
            &d,
            Some("touch started; sleep 3"),
            Probe::Shell(host_step("test -e started", &d)),
            5000,
        );
        let t = Instant::now();
        p.ensure_up(false, &mut |_| {}).unwrap();
        assert!(t.elapsed() < Duration::from_secs(5));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn timeout_reports_log_tail_and_fails() {
        let d = scratch("timeout");
        let p = quick(
            &d,
            Some("for i in 1 2 3 4 5 6 7; do echo line$i; done"),
            Probe::Shell(host_step("false", &d)),
            200,
        );
        let mut seen = Vec::new();
        let e = p
            .ensure_up(false, &mut |n| seen.push(text(n)))
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("app was not ready within 0s"), "{e}");
        let lines: Vec<&str> = e.lines().skip(1).collect();
        assert_eq!(lines, ["line3", "line4", "line5", "line6", "line7"]);
        assert!(
            seen.iter().any(|m| m.starts_with("waiting for app (")),
            "{seen:?}"
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn failing_up_stops_early_with_its_output() {
        let d = scratch("fail");
        let p = quick(
            &d,
            Some("echo no compose file >&2; exit 3"),
            Probe::Shell(host_step("false", &d)),
            10_000,
        );
        let t = Instant::now();
        let e = p.ensure_up(false, &mut |_| {}).unwrap_err().to_string();
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(e.starts_with("`up` failed"), "{e}");
        assert!(e.ends_with("no compose file"), "{e}");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn stop_runs_down_or_needs_it() {
        let d = scratch("stop");
        let mut p = quick(&d, None, Probe::Shell(host_step("true", &d)), 5000);
        let e = p.stop(&mut |_| {}).unwrap_err().to_string();
        assert!(e.contains("needs [incus.browse] down"), "{e}");
        p.down = Some(host_step("echo bye; touch stopped", &d));
        let mut seen = Vec::new();
        p.stop(&mut |n| seen.push(text(n))).unwrap();
        assert_eq!(seen, ["stopping app…", "<down>"]);
        assert!(d.join("stopped").exists());
        assert_eq!(tail(&p.log).last().map(String::as_str), Some("bye"));
        p.down = Some(host_step("echo oops; exit 1", &d));
        let mut seen = Vec::new();
        let e = p.stop(&mut |n| seen.push(text(n))).unwrap_err().to_string();
        assert!(e.ends_with("oops"), "{e}");
        assert_eq!(seen, ["stopping app…"]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn forced_up_runs_even_when_the_app_is_ready() {
        let d = scratch("force");
        let p = quick(
            &d,
            Some("touch started"),
            Probe::Shell(host_step("echo up", &d)),
            5000,
        );
        let mut seen = Vec::new();
        p.ensure_up(true, &mut |n| seen.push(text(n))).unwrap();
        assert_eq!(seen, ["starting app…", "<up>"]);
        std::fs::remove_dir_all(d).unwrap();
    }
}
