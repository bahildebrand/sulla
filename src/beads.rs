use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Duration, Utc};
use color_eyre::{Result, eyre::bail};
use serde::Deserialize;

pub const ACTOR: &str = "sulla";
const STATE_DIM: &str = "sulla";

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
    pub created_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Pending,
    InProgress,
    Review,
    Done,
}

impl Column {
    pub const ALL: [Column; 4] = [
        Column::Pending,
        Column::InProgress,
        Column::Review,
        Column::Done,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Column::Pending => "Pending",
            Column::InProgress => "In Progress",
            Column::Review => "Ready for Review",
            Column::Done => "Done",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Review,
    Held,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Review => "review",
            State::Held => "held",
        }
    }
}

impl Bead {
    pub fn state(&self) -> Option<State> {
        self.labels
            .iter()
            .find_map(|l| match l.strip_prefix("sulla:")? {
                "running" => Some(State::Running),
                "review" => Some(State::Review),
                "held" => Some(State::Held),
                _ => None,
            })
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
                .is_none_or(|t| now - t <= Duration::days(retention_days));
            return recent.then_some(Column::Done);
        }
        match self.state() {
            Some(State::Review) => Some(Column::Review),
            Some(State::Running) => Some(Column::InProgress),
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
        Ok(self
            .json(&["list", "--all", "--json", "-n", "0"])?
            .into_iter()
            .filter(Bead::is_work)
            .collect())
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
            ])?
            .into_iter()
            .filter(|b| b.is_work() && b.issue_type != "epic")
            .collect())
    }

    pub fn show(&self, id: &str) -> Result<Bead> {
        match self.json(&["show", id, "--json"])?.into_iter().next() {
            Some(b) => Ok(b),
            None => bail!("bead {id} not found"),
        }
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
        let kv = format!("{STATE_DIM}={}", state.as_str());
        self.run(&["set-state", id, &kv, "--reason", "sulla", "--actor", ACTOR])
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

    pub fn set_priority(&self, id: &str, priority: u8) -> Result<()> {
        self.run(&["update", id, "-p", &priority.to_string()])
            .map(drop)
    }

    pub fn close(&self, id: &str, reason: &str) -> Result<()> {
        self.run(&["close", id, "-r", reason, "--actor", ACTOR])
            .map(drop)
    }

    pub fn create(&self, title: &str, priority: u8) -> Result<String> {
        Ok(self
            .run(&["q", title, "-p", &priority.to_string()])?
            .trim()
            .to_string())
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
    fn held_bead_stays_pending() {
        let now = Utc::now();
        let held = bead(r#"{"id":"a","title":"t","status":"open","labels":["sulla:held"]}"#);
        assert_eq!(held.state(), Some(State::Held));
        assert_eq!(held.column(now, 3), Some(Column::Pending));
    }
}
