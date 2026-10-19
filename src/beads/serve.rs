use std::collections::HashSet;
use std::fs::File;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use color_eyre::{Report, Result, eyre::eyre};
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::{Value, json};

use super::{ACTOR, Bead, Blocked, Comment};

pub const NO_TELEMETRY: (&str, &str) = ("DOLT_DISABLE_EVENT_FLUSH", "1");
const BODY_LIMIT: u64 = 512 << 20;
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_POLL: Duration = Duration::from_millis(100);
const PORT_BASE: u16 = 20000;
const PORT_SPAN: u64 = 10000;

pub fn server_mode(repo: &Path) -> bool {
    std::fs::read_to_string(repo.join(".beads").join("metadata.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .is_some_and(|v| v["dolt_mode"] == "server")
}

pub fn dolt_port(repo: &Path) -> Option<u16> {
    std::fs::read_to_string(repo.join(".beads").join("dolt-server.port"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), PROBE_TIMEOUT).is_ok()
}

pub fn serve_port(repo: &Path) -> u16 {
    let hash = repo
        .to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x100000001b3)
        });
    PORT_BASE + (hash % PORT_SPAN) as u16
}

#[derive(Debug)]
pub enum Fail {
    Down(String),
    Err(Report),
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    items: Vec<T>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Debug, Deserialize)]
struct Problem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    detail: String,
}

#[derive(Debug, Deserialize)]
struct Context {
    #[serde(default)]
    beads_dir: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct WithComments {
    #[serde(default)]
    comments: Vec<Comment>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Link {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub priority: u8,
    #[serde(default, alias = "type")]
    pub dependency_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Note {
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub text: String,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Detail {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub priority: u8,
    #[serde(default)]
    pub issue_type: String,
    #[serde(default)]
    pub assignee: String,
    #[serde(default)]
    pub created_by: String,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub close_reason: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub design: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub acceptance_criteria: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<Link>,
    #[serde(default)]
    pub dependents: Vec<Link>,
    #[serde(default)]
    pub comments: Vec<Note>,
}

#[derive(Debug)]
pub struct Http {
    agent: ureq::Agent,
    base: String,
    caps: HashSet<String>,
    actor: String,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(None)
        .timeout_global(Some(CALL_TIMEOUT))
        .build()
        .into()
}

fn segment(id: &str) -> String {
    id.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn transport(what: &str, e: ureq::Error) -> Fail {
    let refused = match &e {
        ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => true,
        ureq::Error::Io(io) => io.kind() == std::io::ErrorKind::ConnectionRefused,
        _ => false,
    };
    if refused {
        Fail::Down(format!("{what}: {e}"))
    } else {
        Fail::Err(eyre!("bd serve {what}: {e}"))
    }
}

fn read<T: DeserializeOwned>(
    what: &str,
    r: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<T, Fail> {
    let mut resp = r.map_err(|e| transport(what, e))?;
    let status = resp.status();
    let text = resp
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .map_err(|e| Fail::Err(eyre!("bd serve {what}: {e}")))?;
    if !status.is_success() {
        let msg = match serde_json::from_str::<Problem>(&text) {
            Ok(p) if !p.detail.is_empty() => p.detail,
            Ok(p) if !p.title.is_empty() => p.title,
            _ => text.trim().to_string(),
        };
        return Err(Fail::Err(eyre!(
            "bd serve {what}: {} {msg}",
            status.as_u16()
        )));
    }
    serde_json::from_str(&text).map_err(|e| Fail::Err(eyre!("bd serve {what}: {e}")))
}

fn whole<T>(what: &str, p: Page<T>) -> Result<Vec<T>, Fail> {
    if p.has_more {
        return Err(Fail::Err(eyre!("bd serve {what}: answer was truncated")));
    }
    Ok(p.items)
}

fn context(agent: &ureq::Agent, base: &str) -> Option<Context> {
    let resp = agent
        .get(format!("{base}/v0/beads/context"))
        .config()
        .timeout_global(Some(PROBE_TIMEOUT * 4))
        .build()
        .call();
    read("context", resp).ok()
}

impl Http {
    pub fn can(&self, cap: &str) -> bool {
        self.caps.contains(cap)
    }

    fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T, Fail> {
        let mut req = self.agent.get(format!("{}{path}", self.base));
        for (k, v) in query {
            req = req.query(*k, *v);
        }
        read(path, req.call())
    }

    fn post(&self, path: &str, body: Value) -> Result<(), Fail> {
        let req = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json");
        read::<IgnoredAny>(path, req.send(body.to_string())).map(drop)
    }

    fn patch(&self, id: &str, patch: Value) -> Result<(), Fail> {
        let path = format!("/v0/beads/issues/{}", segment(id));
        let body = json!({"actor": self.actor, "patch": patch});
        let req = self
            .agent
            .patch(format!("{}{path}", self.base))
            .header("content-type", "application/json");
        read::<IgnoredAny>(&path, req.send(body.to_string())).map(drop)
    }

    pub fn list(&self, parent: Option<&str>) -> Result<Vec<Bead>, Fail> {
        let mut q = vec![("all", "true"), ("limit", "0"), ("sort", "priority")];
        q.extend(parent.map(|p| ("parent", p)));
        whole("list", self.get("/v0/beads/issues", &q)?)
    }

    pub fn ready(&self, exclude: &[&str]) -> Result<Vec<Bead>, Fail> {
        let mut q = vec![("limit", "0")];
        q.extend(exclude.iter().map(|l| ("exclude_label", *l)));
        whole("ready", self.get("/v0/beads/ready", &q)?)
    }

    pub fn blocked(&self) -> Result<Vec<Blocked>, Fail> {
        whole("blocked", self.get("/v0/beads/blocked", &[])?)
    }

    pub fn show(&self, id: &str) -> Result<Bead, Fail> {
        self.get(&format!("/v0/beads/issues/{}", segment(id)), &[])
    }

    pub fn comments(&self, id: &str) -> Result<Vec<Comment>, Fail> {
        let path = format!("/v0/beads/issues/{}", segment(id));
        let c: WithComments = self.get(&path, &[("include_comments", "true")])?;
        Ok(c.comments)
    }

    pub fn detail(&self, id: &str) -> Result<Detail, Fail> {
        let path = format!("/v0/beads/issues/{}", segment(id));
        self.get(
            &path,
            &[("include_comments", "true"), ("include_dependents", "true")],
        )
    }

    pub fn claim(&self, id: &str) -> Result<(), Fail> {
        let path = format!("/v0/beads/issues/{}:claim", segment(id));
        self.post(&path, json!({"actor": ACTOR}))
    }

    pub fn release(&self, id: &str) -> Result<(), Fail> {
        let path = format!("/v0/beads/issues/{}:release", segment(id));
        self.post(&path, json!({"actor": ACTOR, "force": true}))
    }

    pub fn close(&self, id: &str, reason: &str, force: bool) -> Result<(), Fail> {
        let path = format!("/v0/beads/issues/{}:close", segment(id));
        self.post(
            &path,
            json!({"actor": ACTOR, "reason": reason, "force": force}),
        )
    }

    pub fn delete(&self, id: &str) -> Result<(), Fail> {
        self.post(
            "/v0/beads/issues:delete",
            json!({"ids": [id], "actor": ACTOR, "force": true}),
        )
    }

    pub fn update(&self, id: &str, patch: Value) -> Result<(), Fail> {
        self.patch(id, patch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Embedded,
    Starting,
    Ready { url: String, ours: bool },
    Failed(String),
}

#[derive(Debug)]
pub struct Server {
    pub mode: bool,
    http: OnceLock<Http>,
    status: Mutex<Status>,
    child: Mutex<Option<Child>>,
    warned: AtomicBool,
}

impl Server {
    pub fn new(mode: bool) -> Self {
        Self {
            mode,
            http: OnceLock::new(),
            status: Mutex::new(if mode {
                Status::Starting
            } else {
                Status::Embedded
            }),
            child: Mutex::new(None),
            warned: AtomicBool::new(false),
        }
    }

    pub fn http(&self) -> Option<&Http> {
        self.http.get()
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn down(&self, e: &str) {
        if !self.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!("bd serve unreachable, using the bd CLI: {e}");
        }
    }

    pub fn start(&self, repo: &Path) {
        let status = match self.launch(repo) {
            Ok(http) => {
                let url = http.base.clone();
                let ours = self.child.lock().unwrap().is_some();
                tracing::info!(
                    "bd serve at {url} ({})",
                    if ours { "started" } else { "reused" }
                );
                let _ = self.http.set(http);
                Status::Ready { url, ours }
            }
            Err(e) => {
                tracing::warn!("bd serve unavailable, using the bd CLI: {e:#}");
                Status::Failed(format!("{e:#}"))
            }
        };
        *self.status.lock().unwrap() = status;
    }

    fn launch(&self, repo: &Path) -> Result<Http> {
        start_dolt(repo)?;
        let port = serve_port(repo);
        let base = format!("http://127.0.0.1:{port}");
        let agent = agent();
        let beads_dir = std::fs::canonicalize(repo.join(".beads"))?;
        let ctx = match context(&agent, &base) {
            Some(ctx) => ctx,
            None if listening(port) => {
                return Err(eyre!("port {port} is in use by another program"));
            }
            None => self.spawn(repo, port, &agent, &base)?,
        };
        if std::fs::canonicalize(&ctx.beads_dir).ok().as_deref() != Some(beads_dir.as_path()) {
            return Err(eyre!(
                "bd serve on port {port} belongs to {}",
                ctx.beads_dir
            ));
        }
        Ok(Http {
            agent,
            base,
            caps: ctx.capabilities.into_iter().collect(),
            actor: default_actor(repo),
        })
    }

    fn spawn(&self, repo: &Path, port: u16, agent: &ureq::Agent, base: &str) -> Result<Context> {
        let log_path = log_path(repo);
        if let Some(dir) = log_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let log = File::create(&log_path)?;
        let mut child = Command::new("bd")
            .args(["serve", "--addr", &format!("127.0.0.1:{port}")])
            .current_dir(repo)
            .env(NO_TELEMETRY.0, NO_TELEMETRY.1)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        let start = Instant::now();
        loop {
            if let Some(ctx) = context(agent, base) {
                *self.child.lock().unwrap() = Some(child);
                return Ok(ctx);
            }
            if let Some(code) = child.try_wait()? {
                return Err(eyre!(
                    "`bd serve` exited with {code}: {}",
                    log_tail(&log_path)
                ));
            }
            if start.elapsed() >= READY_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                return Err(eyre!(
                    "`bd serve` did not answer within {}s: {}",
                    READY_TIMEOUT.as_secs(),
                    log_tail(&log_path)
                ));
            }
            std::thread::sleep(READY_POLL);
        }
    }

    pub fn stop(&self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    pub fn live(&self) -> bool {
        self.http().is_some_and(|h| {
            h.agent
                .get(format!("{}/healthz", h.base))
                .config()
                .timeout_global(Some(PROBE_TIMEOUT * 4))
                .build()
                .call()
                .is_ok_and(|r| r.status().is_success())
        })
    }
}

pub fn log_path(repo: &Path) -> PathBuf {
    repo.join(".sulla").join("bd-serve.log")
}

fn log_tail(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output")
        .to_string()
}

fn start_dolt(repo: &Path) -> Result<()> {
    if dolt_port(repo).is_some_and(listening) {
        return Ok(());
    }
    let out = Command::new("bd")
        .args(["dolt", "start"])
        .current_dir(repo)
        .env(NO_TELEMETRY.0, NO_TELEMETRY.1)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err.lines().map(str::trim).find(|l| !l.is_empty());
        return Err(eyre!(
            "`bd dolt start` failed: {}",
            msg.unwrap_or("no output")
        ));
    }
    Ok(())
}

fn default_actor(repo: &Path) -> String {
    if let Ok(a) = std::env::var("BEADS_ACTOR")
        && !a.trim().is_empty()
    {
        return a;
    }
    let git = Command::new("git")
        .args(["config", "user.name"])
        .current_dir(repo)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    git.or_else(|| std::env::var("USER").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| ACTOR.to_string())
}

fn icon(status: &str) -> &'static str {
    match status {
        "open" => "○",
        "in_progress" => "◐",
        "blocked" => "●",
        "closed" => "✓",
        "deferred" => "❄",
        _ => "·",
    }
}

fn day(t: Option<DateTime<Utc>>) -> Option<String> {
    t.map(|t| t.format("%Y-%m-%d").to_string())
}

fn indent(out: &mut String, pad: &str, text: &str) {
    for l in text.trim_end().lines() {
        if !l.trim().is_empty() {
            out.push_str(pad);
            out.push_str(l.trim_end());
        }
        out.push('\n');
    }
}

fn block(out: &mut String, heading: &str, body: &str) {
    if body.trim().is_empty() {
        return;
    }
    out.push_str(&format!("\n{heading}\n"));
    indent(out, "  ", body);
}

fn links<'a>(out: &mut String, heading: &str, arrow: &str, items: impl Iterator<Item = &'a Link>) {
    let items: Vec<&Link> = items.collect();
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("\n{heading}\n"));
    for l in items {
        let kind = match l.dependency_type.as_str() {
            "blocks" | "parent-child" => String::new(),
            t => format!(" ({t})"),
        };
        out.push_str(&format!(
            "  {arrow} {} {}: {} P{}{kind}\n",
            icon(&l.status),
            l.id,
            l.title,
            l.priority
        ));
    }
}

pub fn render(d: &Detail) -> String {
    let mut out = format!(
        "{} {} · {}   [P{} · {}]\n",
        icon(&d.status),
        d.id,
        d.title,
        d.priority,
        d.status.to_uppercase()
    );
    let who: Vec<String> = [
        (!d.created_by.is_empty()).then(|| format!("Created by: {}", d.created_by)),
        (!d.assignee.is_empty()).then(|| format!("Assignee: {}", d.assignee)),
        (!d.issue_type.is_empty()).then(|| format!("Type: {}", d.issue_type)),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !who.is_empty() {
        out.push_str(&format!("{}\n", who.join(" · ")));
    }
    let when: Vec<String> = [
        day(d.created_at).map(|t| format!("Created: {t}")),
        day(d.started_at).map(|t| format!("Started: {t}")),
        day(d.updated_at).map(|t| format!("Updated: {t}")),
        day(d.closed_at).map(|t| format!("Closed: {t}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !when.is_empty() {
        out.push_str(&format!("{}\n", when.join(" · ")));
    }
    if !d.close_reason.is_empty() {
        out.push_str(&format!("Close reason: {}\n", d.close_reason));
    }
    if d.description.trim().is_empty() {
        out.push_str("\nDESCRIPTION\n  (none)\n");
    } else {
        block(&mut out, "DESCRIPTION", &d.description);
    }
    block(&mut out, "DESIGN", &d.design);
    block(&mut out, "NOTES", &d.notes);
    block(&mut out, "ACCEPTANCE CRITERIA", &d.acceptance_criteria);
    if !d.labels.is_empty() {
        out.push_str(&format!("\nLABELS: {}\n", d.labels.join(", ")));
    }
    let is = |t: &'static str| move |l: &&Link| l.dependency_type == t;
    let other = |l: &&Link| !matches!(l.dependency_type.as_str(), "blocks" | "parent-child");
    links(
        &mut out,
        "PARENT",
        "↑",
        d.dependencies.iter().filter(is("parent-child")),
    );
    links(
        &mut out,
        "DEPENDS ON",
        "→",
        d.dependencies.iter().filter(is("blocks")),
    );
    links(
        &mut out,
        "CHILDREN",
        "↳",
        d.dependents.iter().filter(is("parent-child")),
    );
    links(
        &mut out,
        "BLOCKS",
        "←",
        d.dependents.iter().filter(is("blocks")),
    );
    links(
        &mut out,
        "RELATED",
        "↔",
        d.dependencies.iter().chain(&d.dependents).filter(other),
    );
    if !d.comments.is_empty() {
        out.push_str("\nCOMMENTS\n");
        for c in &d.comments {
            let at = c.created_at.map(|t| t.format("%Y-%m-%d %H:%M").to_string());
            let head: Vec<&str> = at
                .as_deref()
                .into_iter()
                .chain([c.author.as_str()])
                .collect();
            out.push_str(&format!("  {}\n", head.join(" ")));
            indent(&mut out, "    ", &c.text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_server_mode_and_port_from_beads_dir() {
        let dir = std::env::temp_dir().join(format!("sulla-serve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".beads")).unwrap();
        assert!(!server_mode(&dir));
        assert_eq!(dolt_port(&dir), None);
        std::fs::write(
            dir.join(".beads/metadata.json"),
            r#"{"backend":"dolt","dolt_mode":"embedded"}"#,
        )
        .unwrap();
        assert!(!server_mode(&dir));
        std::fs::write(
            dir.join(".beads/metadata.json"),
            r#"{"backend":"dolt","dolt_mode":"server"}"#,
        )
        .unwrap();
        assert!(server_mode(&dir));
        std::fs::write(dir.join(".beads/dolt-server.port"), "46509\n").unwrap();
        assert_eq!(dolt_port(&dir), Some(46509));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn serve_port_is_stable_per_repo_and_in_range() {
        let a = serve_port(Path::new("/home/x/git/a"));
        assert_eq!(a, serve_port(Path::new("/home/x/git/a")));
        assert_ne!(a, serve_port(Path::new("/home/x/git/b")));
        for p in ["/", "/r", "/home/x/git/a"] {
            let port = serve_port(Path::new(p));
            assert!((PORT_BASE..PORT_BASE + PORT_SPAN as u16).contains(&port));
        }
    }

    #[test]
    fn segment_escapes_path_characters() {
        assert_eq!(segment("sulla-glp.1"), "sulla-glp.1");
        assert_eq!(segment("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn problem_detail_becomes_the_error() {
        let p: Problem =
            serde_json::from_str(r#"{"title":"Conflict","detail":"already claimed","status":409}"#)
                .unwrap();
        assert_eq!(p.detail, "already claimed");
        let page: Page<Bead> = serde_json::from_str(r#"{"items":[],"has_more":true}"#).unwrap();
        assert!(matches!(whole("list", page), Err(Fail::Err(_))));
    }

    #[test]
    fn parses_blocked_page_and_comments_from_issue() {
        let page: Page<Blocked> = serde_json::from_str(
            r#"{"items":[{"id":"b","title":"t","status":"open","blocked_by_count":1,"blocked_by":["a"]}],"has_more":false}"#,
        )
        .unwrap();
        let items = whole("blocked", page).unwrap();
        assert_eq!(items[0].id, "b");
        assert_eq!(items[0].blocked_by, vec!["a"]);
        let c: WithComments = serde_json::from_str(
            r#"{"id":"a","title":"t","status":"open","comments":[{"id":"1","author":"B","text":"hi","created_at":"2026-10-19T15:43:44Z"}]}"#,
        )
        .unwrap();
        assert_eq!(c.comments[0].text, "hi");
        let none: WithComments = serde_json::from_str(r#"{"id":"a"}"#).unwrap();
        assert!(none.comments.is_empty());
    }

    #[test]
    fn renders_show_text_from_detail() {
        let d: Detail = serde_json::from_str(
            r#"{"id":"a","title":"First","status":"open","priority":1,"issue_type":"task",
            "created_by":"B","created_at":"2026-10-19T15:43:44Z","description":"line one\n\nline two",
            "notes":"n","labels":["sulla:review","x"],
            "dependencies":[{"id":"p","title":"Epic","status":"open","priority":0,"dependency_type":"parent-child"},
                            {"id":"z","title":"Base","status":"closed","priority":2,"dependency_type":"blocks"}],
            "dependents":[{"id":"a.1","title":"State change: sulla → review","status":"closed","priority":4,"dependency_type":"parent-child"},
                          {"id":"b","title":"Second","status":"in_progress","priority":2,"dependency_type":"blocks"},
                          {"id":"c","title":"Kin","status":"open","priority":3,"dependency_type":"related"}],
            "comments":[{"author":"B","text":"hello","created_at":"2026-10-19T15:43:44Z"}]}"#,
        )
        .unwrap();
        let text = render(&d);
        let want = "○ a · First   [P1 · OPEN]
Created by: B · Type: task
Created: 2026-10-19

DESCRIPTION
  line one

  line two

NOTES
  n

LABELS: sulla:review, x

PARENT
  ↑ ○ p: Epic P0

DEPENDS ON
  → ✓ z: Base P2

CHILDREN
  ↳ ✓ a.1: State change: sulla → review P4

BLOCKS
  ← ◐ b: Second P2

RELATED
  ↔ ○ c: Kin P3 (related)

COMMENTS
  2026-10-19 15:43 B
    hello
";
        assert_eq!(text, want);
    }
}
