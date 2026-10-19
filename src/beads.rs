mod serve;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use color_eyre::{Result, eyre::bail};
use serde::Deserialize;
use serde_json::json;

use crate::health::{Status as Health, Tool};
use serve::{Fail, Http, Readiness, Server, Status as ServeStatus};
pub use serve::{NO_TELEMETRY, beads_dir, dolt_port, metadata, server_mode, start_dolt};

pub const ACTOR: &str = "sulla";
const STATE_DIM: &str = "sulla";
pub const STATE_EVENT_PREFIX: &str = "State change: sulla → ";
const DOLT_JOURNAL: &str = "vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv";

#[derive(Debug, Clone, Deserialize)]
pub struct Bead {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub priority: u8,
    #[serde(default)]
    pub issue_type: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub notes: String,
    pub created_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(skip)]
    pub state_changed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Dependency {
    #[serde(alias = "depends_on_id")]
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, alias = "type")]
    pub dependency_type: String,
}

#[derive(Debug, Deserialize)]
struct Blocked {
    id: String,
    #[serde(default)]
    blocked_by: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Comment {
    #[serde(default)]
    pub text: String,
    pub created_at: Option<DateTime<Utc>>,
}

fn parse_comments(json: &str) -> Result<Vec<Comment>> {
    if json.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(json)?)
}

fn newest(comments: Vec<Comment>) -> Option<String> {
    comments
        .into_iter()
        .enumerate()
        .max_by_key(|(i, c)| (c.created_at, *i))
        .map(|(_, c)| c.text)
}

#[cfg(test)]
fn newest_comment(json: &str) -> Result<Option<String>> {
    Ok(newest(parse_comments(json)?))
}

fn stamp_state_changes(all: &mut [Bead]) {
    let mut latest: HashMap<(String, String), DateTime<Utc>> = HashMap::new();
    for e in all.iter().filter(|b| b.issue_type == "event") {
        let (Some(parent), Some(state), Some(at)) = (
            &e.parent,
            e.title.strip_prefix(STATE_EVENT_PREFIX),
            e.created_at,
        ) else {
            continue;
        };
        let t = latest
            .entry((parent.clone(), state.trim().to_string()))
            .or_insert(at);
        *t = (*t).max(at);
    }
    for b in all.iter_mut() {
        if let Some(s) = b.state() {
            b.state_changed_at = latest.get(&(b.id.clone(), s.as_str().to_string())).copied();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Backlog,
    Pending,
    InProgress,
    Review,
    AwaitingPr,
    Done,
}

impl Column {
    pub const ALL: [Column; 6] = [
        Column::Backlog,
        Column::Pending,
        Column::InProgress,
        Column::Review,
        Column::AwaitingPr,
        Column::Done,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Column::Backlog => "Backlog",
            Column::Pending => "Pending",
            Column::InProgress => "In Progress",
            Column::Review => "Ready for Review",
            Column::AwaitingPr => "Awaiting PR",
            Column::Done => "Done",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Backlog,
    Running,
    Reviewing,
    Review,
    Held,
    Pr,
    Revise,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Backlog => "backlog",
            State::Running => "running",
            State::Reviewing => "reviewing",
            State::Review => "review",
            State::Held => "held",
            State::Pr => "pr",
            State::Revise => "revise",
        }
    }
}

impl Bead {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.iter().find_map(|l| {
            let v = l.strip_prefix(key)?.strip_prefix(':')?.trim();
            (!v.is_empty()).then_some(v)
        })
    }

    pub fn state(&self) -> Option<State> {
        self.labels
            .iter()
            .find_map(|l| match l.strip_prefix("sulla:")? {
                "backlog" => Some(State::Backlog),
                "running" => Some(State::Running),
                "reviewing" => Some(State::Reviewing),
                "review" => Some(State::Review),
                "held" => Some(State::Held),
                "pr" => Some(State::Pr),
                "revise" => Some(State::Revise),
                _ => None,
            })
    }

    pub fn set_state_label(&mut self, state: Option<State>) {
        self.labels.retain(|l| !l.starts_with("sulla:"));
        if let Some(s) = state {
            self.labels.push(format!("{STATE_DIM}:{}", s.as_str()));
        }
    }

    pub fn state_since(&self) -> Option<DateTime<Utc>> {
        if self.status == "closed" {
            return self.closed_at.or(self.updated_at);
        }
        self.state()
            .and(self.state_changed_at)
            .or(self.updated_at)
            .or(self.created_at)
    }

    pub fn open_blockers(&self) -> Vec<&str> {
        self.dependencies
            .iter()
            .filter(|d| d.dependency_type == "blocks" && d.status != "closed")
            .map(|d| d.id.as_str())
            .collect()
    }

    pub fn is_work(&self) -> bool {
        !matches!(self.issue_type.as_str(), "event" | "gate" | "molecule")
    }

    pub fn column(&self, now: DateTime<Utc>, retention_days: i64) -> Option<Column> {
        if !self.is_work() {
            return None;
        }
        if self.status == "closed" {
            let recent = self
                .closed_at
                .is_none_or(|t| Duration::try_days(retention_days).is_none_or(|r| now - t <= r));
            return recent.then_some(Column::Done);
        }
        match self.state() {
            Some(State::Backlog) => Some(Column::Backlog),
            Some(State::Review) => Some(Column::Review),
            Some(State::Pr) => Some(Column::AwaitingPr),
            Some(State::Revise) => Some(Column::Pending),
            Some(State::Running | State::Reviewing) => Some(Column::InProgress),
            _ if self.status == "in_progress" => Some(Column::InProgress),
            _ => Some(Column::Pending),
        }
    }
}

const MUTATION_HOOKS: [&str; 3] = ["on_create", "on_update", "on_close"];
const READY_EXCLUDES: [&str; 3] = ["sulla:held", "sulla:backlog", "sulla:pr"];

#[derive(Debug, Clone)]
pub struct Beads {
    repo: PathBuf,
    server: Arc<Server>,
}

impl Beads {
    pub fn new(repo: &Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
            server: Arc::new(Server::new(repo)),
        }
    }

    pub fn server_mode(&self) -> bool {
        self.server.mode
    }

    pub fn connect(&self) {
        if !self.server.mode {
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || me.server.start());
    }

    pub fn shutdown(&self) {
        self.server.stop();
    }

    pub fn health(&self) -> Vec<Tool> {
        if !self.server.mode {
            return Vec::new();
        }
        let tool = |name: &str, bin: &str, status| Tool {
            name: name.into(),
            bin: bin.into(),
            status,
        };
        let sql = match dolt_port(&self.repo) {
            Some(p) if serve::listening(p) => Health::Ok(format!("running on 127.0.0.1:{p}")),
            Some(p) => Health::Failed(format!(
                "not answering on 127.0.0.1:{p}, start it with `bd dolt start`"
            )),
            None => Health::Failed("not running, start it with `bd dolt start`".into()),
        };
        let serve = match (self.server.status(), self.server.readiness()) {
            (ServeStatus::Ready { url, ours }, Some(Readiness::NotReady(e))) => {
                Health::Failed(self.not_ready(&url, ours, &e))
            }
            (ServeStatus::Ready { url, .. }, Some(Readiness::Down(_))) => {
                Health::Failed(format!("{url} stopped answering, using the bd CLI"))
            }
            (ServeStatus::Ready { url, ours }, _) => Health::Ok(format!(
                "{url} ({})",
                if ours {
                    "started by sulla"
                } else {
                    "already running"
                }
            )),
            (ServeStatus::Starting, _) => Health::Off("starting".into()),
            (ServeStatus::Failed(e), _) => Health::Failed(format!(
                "{e}, using the bd CLI (log: {})",
                serve::log_path(&self.repo).display()
            )),
            (ServeStatus::Embedded, _) => Health::Off("not used (embedded database)".into()),
        };
        vec![tool("dolt sql", "dolt", sql), tool("bd serve", "bd", serve)]
    }

    fn not_ready(&self, url: &str, ours: bool, e: &str) -> String {
        let mut msg = format!("{url} is up but not ready ({e}), using the bd CLI");
        if ours {
            msg.push_str(", sulla restarts it");
        }
        if let Some(pid) = serve::proxy_pid(&self.repo) {
            msg.push_str(&format!(
                ". If the Dolt server moved to a new port, `bd db-proxy-child` {pid} (.beads/dolt/proxy.pid) still points at the old one"
            ));
            if !ours {
                msg.push_str(": stop that bd serve and the proxy");
            }
        }
        msg
    }

    fn http(&self, cap: &str) -> Option<&Http> {
        self.server.http().filter(|h| h.can(cap))
    }

    fn http_write(&self, cap: &str) -> Option<&Http> {
        self.http(cap).filter(|_| !self.has_mutation_hooks())
    }

    fn has_mutation_hooks(&self) -> bool {
        use std::os::unix::fs::PermissionsExt;
        let dir = beads_dir(&self.repo).join("hooks");
        MUTATION_HOOKS.iter().any(|h| {
            std::fs::metadata(dir.join(h))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    }

    fn via<T>(
        &self,
        http: Option<&Http>,
        call: impl FnOnce(&Http) -> Result<T, Fail>,
        cli: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if let Some(h) = http {
            match call(h) {
                Ok(v) => return Ok(v),
                Err(Fail::Err(e)) => return Err(e),
                Err(Fail::Down(e) | Fail::Unready(e)) => self.server.down(&e),
            }
        }
        cli()
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("bd");
        cmd.args(args).current_dir(&self.repo);
        if self.server.mode {
            cmd.env(serve::NO_TELEMETRY.0, serve::NO_TELEMETRY.1);
        }
        let out = cmd.output()?;
        if !out.status.success() {
            bail!(
                "bd {}: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn stamp(&self) -> Option<Vec<u8>> {
        let beads = self.repo.join(".beads");
        let mut dbs: Vec<(PathBuf, bool)> = [("embeddeddolt", false), ("dolt", true)]
            .into_iter()
            .filter_map(|(dir, server)| Some((std::fs::read_dir(beads.join(dir)).ok()?, server)))
            .flat_map(|(entries, server)| {
                entries
                    .filter_map(move |e| Some((e.ok()?.path().join(".dolt").join("noms"), server)))
            })
            .filter(|(noms, _)| noms.join("manifest").is_file())
            .collect();
        if dbs.is_empty() {
            return None;
        }
        dbs.sort();
        let mut stamp = Vec::new();
        for (noms, server) in dbs {
            stamp.extend(std::fs::read(noms.join("manifest")).ok()?);
            if server && let Ok(journal) = std::fs::metadata(noms.join(DOLT_JOURNAL)) {
                let modified = journal
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?;
                stamp.extend(journal.len().to_le_bytes());
                stamp.extend(modified.as_nanos().to_le_bytes());
            }
        }
        Some(stamp)
    }

    fn json(&self, args: &[&str]) -> Result<Vec<Bead>> {
        let out = self.run(args)?;
        if out.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_str(&out)?)
    }

    pub fn list_all(&self) -> Result<Vec<Bead>> {
        let mut all = self.via(
            self.http("issues.list"),
            |h| h.list(None),
            || self.json(&["list", "--all", "--json", "-n", "0"]),
        )?;
        stamp_state_changes(&mut all);
        Ok(all.into_iter().filter(Bead::is_work).collect())
    }

    pub fn ready(&self) -> Result<Vec<Bead>> {
        let ready = self.via(
            self.http("ready.list"),
            |h| h.ready(&READY_EXCLUDES),
            || {
                let mut args = vec!["ready", "--json", "-n", "0"];
                for l in READY_EXCLUDES {
                    args.extend(["--exclude-label", l]);
                }
                self.json(&args)
            },
        )?;
        Ok(ready
            .into_iter()
            .filter(|b| b.is_work() && b.issue_type != "epic")
            .collect())
    }

    pub fn blocked(&self, all: &[Bead]) -> Result<HashMap<String, Vec<String>>> {
        let open: Vec<&str> = all
            .iter()
            .filter(|b| !matches!(b.status.as_str(), "closed" | "pinned"))
            .map(|b| b.id.as_str())
            .collect();
        let http = self
            .http("blocked.list")
            .or_else(|| self.http("dependencies.blocking"));
        let list = self.via(
            http,
            |h| {
                if h.can("blocked.list") {
                    h.blocked()
                } else if open.is_empty() {
                    Ok(Vec::new())
                } else {
                    h.blocking(&open)
                }
            },
            || {
                let out = self.run(&["blocked", "--json"])?;
                if out.trim().is_empty() {
                    return Ok(Vec::new());
                }
                Ok(serde_json::from_str::<Vec<Blocked>>(&out)?)
            },
        )?;
        Ok(list.into_iter().map(|b| (b.id, b.blocked_by)).collect())
    }

    pub fn show(&self, id: &str) -> Result<Bead> {
        self.via(
            self.http("issues.get"),
            |h| h.show(id),
            || match self.json(&["show", id, "--json"])?.into_iter().next() {
                Some(b) => Ok(b),
                None => bail!("bead {id} not found"),
            },
        )
    }

    pub fn show_text(&self, id: &str) -> Result<String> {
        self.via(
            self.http("issues.get"),
            |h| h.detail(id).map(|d| serve::render(&d)),
            || self.run(&["show", id]),
        )
    }

    pub fn comments(&self, id: &str) -> Result<Vec<Comment>> {
        self.via(
            self.http("issues.get"),
            |h| h.comments(id),
            || parse_comments(&self.run(&["comments", id, "--json"])?),
        )
    }

    pub fn events(&self, id: &str) -> Result<Vec<Bead>> {
        Ok(self
            .via(
                self.http("issues.list"),
                |h| h.list(Some(id)),
                || self.json(&["list", "--parent", id, "--all", "--json", "-n", "0"]),
            )?
            .into_iter()
            .filter(|b| b.issue_type == "event")
            .collect())
    }

    pub fn latest_comment(&self, id: &str) -> Result<Option<String>> {
        Ok(newest(self.comments(id)?))
    }

    pub fn claim(&self, id: &str) -> Result<()> {
        self.via(
            self.http_write("issues.claim"),
            |h| h.claim(id),
            || {
                self.run(&["update", id, "--claim", "--actor", ACTOR])
                    .map(drop)
            },
        )
    }

    pub fn unclaim(&self, id: &str) -> Result<()> {
        self.via(
            self.http_write("issues.release"),
            |h| h.release(id),
            || {
                self.run(&["unclaim", id, "--force", "--actor", ACTOR])
                    .map(drop)
            },
        )
    }

    fn update(&self, id: &str, patch: serde_json::Value, args: &[&str]) -> Result<()> {
        self.via(
            self.http_write("issues.update"),
            |h| h.update(id, patch),
            || {
                let mut all = vec!["update", id];
                all.extend_from_slice(args);
                self.run(&all).map(drop)
            },
        )
    }

    pub fn set_state(&self, id: &str, state: State) -> Result<()> {
        self.set_state_because(id, state, "sulla")
    }

    pub fn set_state_because(&self, id: &str, state: State, reason: &str) -> Result<()> {
        let kv = format!("{STATE_DIM}={}", state.as_str());
        self.run(&["set-state", id, &kv, "--reason", reason, "--actor", ACTOR])
            .map(drop)
    }

    pub fn clear_state(&self, bead: &Bead) -> Result<()> {
        let labels: Vec<&str> = bead
            .labels
            .iter()
            .filter(|l| l.starts_with("sulla:"))
            .map(String::as_str)
            .collect();
        if labels.is_empty() {
            return Ok(());
        }
        let mut args = Vec::new();
        for l in &labels {
            args.extend(["--remove-label", l]);
        }
        self.update(&bead.id, json!({"remove_labels": labels}), &args)
    }

    pub fn add_label(&self, id: &str, label: &str) -> Result<()> {
        self.update(id, json!({"add_labels": [label]}), &["--add-label", label])
    }

    pub fn remove_label(&self, id: &str, label: &str) -> Result<()> {
        self.update(
            id,
            json!({"remove_labels": [label]}),
            &["--remove-label", label],
        )
    }

    pub fn append_notes(&self, id: &str, text: &str) -> Result<()> {
        self.update(id, json!({"append_notes": text}), &["--append-notes", text])
    }

    pub fn set_priority(&self, id: &str, priority: u8) -> Result<()> {
        self.update(
            id,
            json!({"priority": priority}),
            &["-p", &priority.to_string()],
        )
    }

    pub fn close(&self, id: &str, reason: &str) -> Result<()> {
        self.via(
            self.http_write("issues.close"),
            |h| h.close(id, reason, false),
            || {
                self.run(&["close", id, "-r", reason, "--actor", ACTOR])
                    .map(drop)
            },
        )
    }

    pub fn force_close(&self, id: &str, reason: &str) -> Result<()> {
        self.via(
            self.http_write("issues.close"),
            |h| h.close(id, reason, true),
            || {
                self.run(&["close", id, "-r", reason, "--force", "--actor", ACTOR])
                    .map(drop)
            },
        )
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.via(
            self.http_write("issues.delete"),
            |h| h.delete(id),
            || {
                self.run(&["delete", id, "--force", "--actor", ACTOR])
                    .map(drop)
            },
        )
    }

    pub fn open_blockers(&self, id: &str) -> Result<Vec<String>> {
        Ok(self
            .show(id)?
            .open_blockers()
            .into_iter()
            .map(String::from)
            .collect())
    }

    pub fn create(
        &self,
        title: &str,
        description: &str,
        priority: u8,
        backlog: bool,
        labels: &[String],
    ) -> Result<String> {
        let mut all: Vec<&str> = Vec::new();
        if backlog {
            all.push("sulla:backlog");
        }
        all.extend(labels.iter().map(String::as_str));
        self.via(
            self.http_write("issues.create"),
            |h| h.create(title, description, priority, &all),
            || {
                let p = priority.to_string();
                let mut args = vec!["create", title, "-p", &p, "--silent"];
                if !description.is_empty() {
                    args.extend(["-d", description]);
                }
                for l in &all {
                    args.extend(["-l", l]);
                }
                Ok(self.run(&args)?.trim().to_string())
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bead(json: &str) -> Bead {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn parses_bd_json_and_maps_columns() {
        let now: DateTime<Utc> = "2026-10-18T00:00:00Z".parse().unwrap();
        let open =
            bead(r#"{"id":"a","title":"t","status":"open","priority":1,"issue_type":"task"}"#);
        let running = bead(
            r#"{"id":"b","title":"t","status":"in_progress","issue_type":"task","labels":["sulla:running"]}"#,
        );
        let review = bead(
            r#"{"id":"c","title":"t","status":"in_progress","issue_type":"task","labels":["x","sulla:review"]}"#,
        );
        let recent = bead(
            r#"{"id":"d","title":"t","status":"closed","issue_type":"task","closed_at":"2026-10-16T00:00:00Z"}"#,
        );
        let old = bead(
            r#"{"id":"e","title":"t","status":"closed","issue_type":"task","closed_at":"2026-10-10T00:00:00Z"}"#,
        );
        let event = bead(r#"{"id":"f","title":"t","status":"closed","issue_type":"event"}"#);

        assert_eq!(open.column(now, 3), Some(Column::Pending));
        assert_eq!(running.column(now, 3), Some(Column::InProgress));
        assert_eq!(review.column(now, 3), Some(Column::Review));
        assert_eq!(recent.column(now, 3), Some(Column::Done));
        assert_eq!(old.column(now, 3), None);
        assert_eq!(event.column(now, 3), None);
    }

    #[test]
    fn huge_retention_does_not_panic() {
        let now: DateTime<Utc> = "2026-10-18T00:00:00Z".parse().unwrap();
        let old = bead(
            r#"{"id":"e","title":"t","status":"closed","issue_type":"task","closed_at":"2020-01-01T00:00:00Z"}"#,
        );
        assert_eq!(old.column(now, i64::MAX), Some(Column::Done));
    }

    #[test]
    fn backlog_bead_has_own_column() {
        let b = bead(r#"{"id":"a","title":"t","status":"open","labels":["sulla:backlog"]}"#);
        assert_eq!(b.column(Utc::now(), 3), Some(Column::Backlog));
    }

    #[test]
    fn open_blockers_ignores_closed_and_non_blocking_deps() {
        let b = bead(
            r#"{"id":"a","title":"t","status":"in_progress","dependencies":[
                {"id":"x","status":"open","dependency_type":"blocks"},
                {"id":"y","status":"closed","dependency_type":"blocks"},
                {"id":"z","status":"in_progress","dependency_type":"blocks"},
                {"id":"p","status":"open","dependency_type":"parent-child"}
            ]}"#,
        );
        assert_eq!(b.open_blockers(), vec!["x", "z"]);
        let free = bead(r#"{"id":"a","title":"t","status":"open"}"#);
        assert!(free.open_blockers().is_empty());
    }

    #[test]
    fn parses_list_style_dependency_edges() {
        let b = bead(
            r#"{"id":"a","title":"t","status":"open","dependencies":[
                {"issue_id":"a","depends_on_id":"x","type":"blocks","metadata":"{}"}
            ]}"#,
        );
        assert_eq!(b.dependencies[0].id, "x");
        assert_eq!(b.dependencies[0].dependency_type, "blocks");
    }

    #[test]
    fn reviewing_bead_is_in_progress() {
        let b =
            bead(r#"{"id":"a","title":"t","status":"in_progress","labels":["sulla:reviewing"]}"#);
        assert_eq!(b.state(), Some(State::Reviewing));
        assert_eq!(State::Reviewing.as_str(), "reviewing");
        assert_eq!(b.column(Utc::now(), 3), Some(Column::InProgress));
    }

    #[test]
    fn newest_comment_picks_latest() {
        let json = r#"[
            {"id":"1","text":"old","created_at":"2026-10-19T00:00:00Z"},
            {"id":"2","text":"new","created_at":"2026-10-19T00:00:05Z"},
            {"id":"3","text":"tie","created_at":"2026-10-19T00:00:05Z"},
            {"id":"4","text":"early","created_at":"2026-10-18T00:00:00Z"}
        ]"#;
        assert_eq!(newest_comment(json).unwrap().as_deref(), Some("tie"));
        assert_eq!(newest_comment("[]").unwrap(), None);
        assert_eq!(newest_comment("").unwrap(), None);
    }

    #[test]
    fn state_since_uses_latest_matching_state_change() {
        let mut all = vec![
            bead(
                r#"{"id":"a","title":"t","status":"in_progress","labels":["sulla:running"],"updated_at":"2026-10-19T05:00:00Z"}"#,
            ),
            bead(
                r#"{"id":"a.1","title":"State change: sulla → running","status":"closed","issue_type":"event","parent":"a","created_at":"2026-10-19T01:00:00Z"}"#,
            ),
            bead(
                r#"{"id":"a.2","title":"State change: sulla → review","status":"closed","issue_type":"event","parent":"a","created_at":"2026-10-19T02:00:00Z"}"#,
            ),
            bead(
                r#"{"id":"a.3","title":"State change: sulla → running","status":"closed","issue_type":"event","parent":"a","created_at":"2026-10-19T03:00:00Z"}"#,
            ),
            bead(r#"{"id":"b","title":"t","status":"open","updated_at":"2026-10-19T04:00:00Z"}"#),
        ];
        stamp_state_changes(&mut all);
        let at = |s: &str| s.parse::<DateTime<Utc>>().ok();
        assert_eq!(all[0].state_since(), at("2026-10-19T03:00:00Z"));
        assert_eq!(all[4].state_since(), at("2026-10-19T04:00:00Z"));
    }

    #[test]
    fn pr_bead_awaits_pr() {
        let b =
            bead(r#"{"id":"a","title":"t","status":"in_progress","labels":["sulla:pr","pr:42"]}"#);
        assert_eq!(b.state(), Some(State::Pr));
        assert_eq!(b.label("pr"), Some("42"));
        assert_eq!(b.column(Utc::now(), 3), Some(Column::AwaitingPr));
        let r = bead(
            r#"{"id":"a","title":"t","status":"in_progress","labels":["sulla:revise","pr:42"]}"#,
        );
        assert_eq!(r.state(), Some(State::Revise));
        assert_eq!(r.column(Utc::now(), 3), Some(Column::Pending));
    }

    #[test]
    fn set_state_label_replaces_sulla_labels() {
        let mut b =
            bead(r#"{"id":"a","title":"t","status":"open","labels":["x","sulla:held","pr:4"]}"#);
        b.set_state_label(Some(State::Backlog));
        assert_eq!(b.state(), Some(State::Backlog));
        assert_eq!(b.labels, ["x", "pr:4", "sulla:backlog"]);
        b.set_state_label(None);
        assert_eq!(b.state(), None);
        assert_eq!(b.labels, ["x", "pr:4"]);
    }

    #[test]
    fn held_bead_stays_pending() {
        let now = Utc::now();
        let held = bead(r#"{"id":"a","title":"t","status":"open","labels":["sulla:held"]}"#);
        assert_eq!(held.state(), Some(State::Held));
        assert_eq!(held.column(now, 3), Some(Column::Pending));
    }

    #[test]
    fn stamp_tracks_embedded_manifests() {
        let dir = std::env::temp_dir().join(format!("sulla-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let beads = Beads::new(&dir);
        assert_eq!(beads.stamp(), None);
        let noms = dir.join(".beads/embeddeddolt/db/.dolt/noms");
        std::fs::create_dir_all(&noms).unwrap();
        assert_eq!(beads.stamp(), None);
        std::fs::write(noms.join("manifest"), "root-a").unwrap();
        let a = beads.stamp();
        assert_eq!(a.as_deref(), Some(b"root-a".as_slice()));
        assert_eq!(beads.stamp(), a);
        std::fs::write(noms.join("manifest"), "root-b").unwrap();
        let b = beads.stamp();
        assert_ne!(b, a);
        touch(&noms.join(DOLT_JOURNAL), 1);
        assert_eq!(beads.stamp(), b);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stamp_tracks_server_journal() {
        let dir = std::env::temp_dir().join(format!("sulla-stamp-server-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let beads = Beads::new(&dir);
        let noms = dir.join(".beads/dolt/db/.dolt/noms");
        std::fs::create_dir_all(dir.join(".beads/dolt/.dolt")).unwrap();
        std::fs::create_dir_all(&noms).unwrap();
        std::fs::write(noms.join("manifest"), "root-a").unwrap();
        assert_eq!(beads.stamp().as_deref(), Some(b"root-a".as_slice()));
        touch(&noms.join(DOLT_JOURNAL), 1);
        let a = beads.stamp();
        assert_eq!(beads.stamp(), a);
        touch(&noms.join(DOLT_JOURNAL), 2);
        assert_ne!(beads.stamp(), a);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn touch(path: &Path, secs: u64) {
        let f = std::fs::File::create(path).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }
}
