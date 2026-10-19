use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Duration, Utc};
use color_eyre::{Result, eyre::bail};
use serde::Deserialize;

pub const ACTOR: &str = "sulla";
const STATE_DIM: &str = "sulla";
pub const STATE_EVENT_PREFIX: &str = "State change: sulla → ";

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

fn newest_comment(json: &str) -> Result<Option<String>> {
    Ok(parse_comments(json)?
        .into_iter()
        .enumerate()
        .max_by_key(|(i, c)| (c.created_at, *i))
        .map(|(_, c)| c.text))
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

#[derive(Debug, Clone)]
pub struct Beads {
    repo: PathBuf,
}

impl Beads {
    pub fn new(repo: &Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
        }
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("bd")
            .args(args)
            .current_dir(&self.repo)
            .output()?;
        if !out.status.success() {
            bail!(
                "bd {}: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn json(&self, args: &[&str]) -> Result<Vec<Bead>> {
        let out = self.run(args)?;
        if out.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_str(&out)?)
    }

    pub fn list_all(&self) -> Result<Vec<Bead>> {
        let mut all = self.json(&["list", "--all", "--json", "-n", "0"])?;
        stamp_state_changes(&mut all);
        Ok(all.into_iter().filter(Bead::is_work).collect())
    }

    pub fn ready(&self) -> Result<Vec<Bead>> {
        Ok(self
            .json(&[
                "ready",
                "--json",
                "-n",
                "0",
                "--exclude-label",
                "sulla:held",
                "--exclude-label",
                "sulla:backlog",
                "--exclude-label",
                "sulla:pr",
            ])?
            .into_iter()
            .filter(|b| b.is_work() && b.issue_type != "epic")
            .collect())
    }

    pub fn blocked(&self) -> Result<HashMap<String, Vec<String>>> {
        let out = self.run(&["blocked", "--json"])?;
        if out.trim().is_empty() {
            return Ok(HashMap::new());
        }
        let list: Vec<Blocked> = serde_json::from_str(&out)?;
        Ok(list.into_iter().map(|b| (b.id, b.blocked_by)).collect())
    }

    pub fn show(&self, id: &str) -> Result<Bead> {
        match self.json(&["show", id, "--json"])?.into_iter().next() {
            Some(b) => Ok(b),
            None => bail!("bead {id} not found"),
        }
    }

    pub fn show_text(&self, id: &str) -> Result<String> {
        self.run(&["show", id])
    }

    pub fn comments(&self, id: &str) -> Result<Vec<Comment>> {
        parse_comments(&self.run(&["comments", id, "--json"])?)
    }

    pub fn events(&self, id: &str) -> Result<Vec<Bead>> {
        Ok(self
            .json(&["list", "--parent", id, "--all", "--json", "-n", "0"])?
            .into_iter()
            .filter(|b| b.issue_type == "event")
            .collect())
    }

    pub fn latest_comment(&self, id: &str) -> Result<Option<String>> {
        newest_comment(&self.run(&["comments", id, "--json"])?)
    }

    pub fn claim(&self, id: &str) -> Result<()> {
        self.run(&["update", id, "--claim", "--actor", ACTOR])
            .map(drop)
    }

    pub fn unclaim(&self, id: &str) -> Result<()> {
        self.run(&["unclaim", id, "--force", "--actor", ACTOR])
            .map(drop)
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
        let mut args = vec!["update", bead.id.as_str()];
        for l in labels {
            args.extend(["--remove-label", l]);
        }
        self.run(&args).map(drop)
    }

    pub fn add_label(&self, id: &str, label: &str) -> Result<()> {
        self.run(&["update", id, "--add-label", label]).map(drop)
    }

    pub fn remove_label(&self, id: &str, label: &str) -> Result<()> {
        self.run(&["update", id, "--remove-label", label]).map(drop)
    }

    pub fn append_notes(&self, id: &str, text: &str) -> Result<()> {
        self.run(&["update", id, "--append-notes", text]).map(drop)
    }

    pub fn set_priority(&self, id: &str, priority: u8) -> Result<()> {
        self.run(&["update", id, "-p", &priority.to_string()])
            .map(drop)
    }

    pub fn close(&self, id: &str, reason: &str) -> Result<()> {
        self.run(&["close", id, "-r", reason, "--actor", ACTOR])
            .map(drop)
    }

    pub fn force_close(&self, id: &str, reason: &str) -> Result<()> {
        self.run(&["close", id, "-r", reason, "--force", "--actor", ACTOR])
            .map(drop)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.run(&["delete", id, "--force", "--actor", ACTOR])
            .map(drop)
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
        let p = priority.to_string();
        let mut args = vec!["create", title, "-p", &p, "--silent"];
        if !description.is_empty() {
            args.extend(["-d", description]);
        }
        if backlog {
            args.extend(["-l", "sulla:backlog"]);
        }
        for l in labels {
            args.extend(["-l", l.as_str()]);
        }
        Ok(self.run(&args)?.trim().to_string())
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
    fn held_bead_stays_pending() {
        let now = Utc::now();
        let held = bead(r#"{"id":"a","title":"t","status":"open","labels":["sulla:held"]}"#);
        assert_eq!(held.state(), Some(State::Held));
        assert_eq!(held.column(now, 3), Some(Column::Pending));
    }
}
