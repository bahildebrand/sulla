use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use color_eyre::{Result, eyre::bail};
use serde::{Deserialize, Serialize};

use crate::health::Probe;

pub const GH: &str = "gh";

pub fn probe() -> Probe {
    Probe::new("gh", GH).also(&["auth", "status"])
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Pr {
    pub number: u64,
    pub url: String,
}

fn gh(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(GH).args(args).current_dir(repo).output()?;
    if !out.status.success() {
        bail!(
            "gh {}: {}",
            args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_list(json: &str) -> Result<Option<Pr>> {
    if json.trim().is_empty() {
        return Ok(None);
    }
    Ok(serde_json::from_str::<Vec<Pr>>(json)?.into_iter().next())
}

fn parse_created(out: &str) -> Option<Pr> {
    let url = out
        .lines()
        .map(str::trim)
        .rfind(|l| l.starts_with("http"))?;
    let number = url.trim_end_matches('/').rsplit('/').next()?.parse().ok()?;
    Some(Pr {
        number,
        url: url.to_string(),
    })
}

pub fn find(repo: &Path, branch: &str) -> Result<Option<Pr>> {
    parse_list(&gh(
        repo,
        &[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "open",
            "--json",
            "number,url",
        ],
    )?)
}

pub fn create(
    repo: &Path,
    branch: &str,
    base: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<Pr> {
    if let Some(pr) = find(repo, branch)? {
        return Ok(pr);
    }
    let mut args = vec![
        "pr", "create", "--head", branch, "--base", base, "--title", title, "--body", body,
    ];
    if draft {
        args.push("--draft");
    }
    let out = gh(repo, &args)?;
    match parse_created(&out) {
        Some(pr) => Ok(pr),
        None => match find(repo, branch)? {
            Some(pr) => Ok(pr),
            None => bail!("gh pr create returned no PR url: {}", out.trim()),
        },
    }
}

pub fn body(id: &str, description: &str) -> String {
    let description = description.trim();
    if description.is_empty() {
        format!("Bead: {id}")
    } else {
        format!("{description}\n\nBead: {id}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrComment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub created_at: Option<DateTime<Utc>>,
}

impl PrComment {
    pub fn describe(&self) -> String {
        let at = match (&self.path, self.line) {
            (Some(p), Some(l)) => format!(" on {p}:{l}"),
            (Some(p), None) => format!(" on {p}"),
            _ => String::new(),
        };
        format!("{}{at}: {}", self.author, self.body.trim())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "result", content = "failing")]
pub enum Checks {
    #[default]
    None,
    Pending,
    Passing,
    Failing(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStatus {
    pub state: PrState,
    pub comments: Vec<PrComment>,
    pub checks: Checks,
}

#[derive(Deserialize)]
struct Author {
    #[serde(default)]
    login: String,
}

#[derive(Deserialize)]
struct RawItem {
    id: serde_json::Value,
    #[serde(alias = "user")]
    author: Option<Author>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    state: String,
    path: Option<String>,
    line: Option<u64>,
    original_line: Option<u64>,
    #[serde(alias = "createdAt", alias = "submittedAt")]
    created_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct RawCheck {
    name: Option<String>,
    context: Option<String>,
    conclusion: Option<String>,
    state: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawView {
    state: String,
    #[serde(default)]
    comments: Vec<RawItem>,
    #[serde(default)]
    reviews: Vec<RawItem>,
    #[serde(default)]
    status_check_rollup: Option<Vec<RawCheck>>,
}

fn comment(r: RawItem) -> Option<PrComment> {
    let body = match (r.body.trim(), r.state.as_str()) {
        ("", "APPROVED") => "approved".to_string(),
        ("", "CHANGES_REQUESTED") => "changes requested".to_string(),
        ("", _) => return None,
        (b, _) => b.to_string(),
    };
    Some(PrComment {
        id: match r.id {
            serde_json::Value::String(s) => s,
            v => v.to_string(),
        },
        author: r
            .author
            .map(|a| a.login)
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "ghost".into()),
        body,
        path: r.path,
        line: r.line.or(r.original_line),
        created_at: r.created_at,
    })
}

fn checks(rollup: &[RawCheck]) -> Checks {
    if rollup.is_empty() {
        return Checks::None;
    }
    let mut failing = Vec::new();
    let mut pending = false;
    for c in rollup {
        let result = c
            .conclusion
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(c.state.as_deref())
            .unwrap_or("");
        match result {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            "" | "PENDING" | "EXPECTED" => pending = true,
            _ => failing.push(
                c.name
                    .clone()
                    .or_else(|| c.context.clone())
                    .unwrap_or_else(|| "check".into()),
            ),
        }
    }
    if !failing.is_empty() {
        failing.sort();
        failing.dedup();
        Checks::Failing(failing)
    } else if pending {
        Checks::Pending
    } else {
        Checks::Passing
    }
}

fn parse_inline(json: &str) -> Result<Vec<PrComment>> {
    let mut out = Vec::new();
    for page in serde_json::Deserializer::from_str(json).into_iter::<Vec<RawItem>>() {
        out.extend(page?.into_iter().filter_map(comment));
    }
    Ok(out)
}

fn parse_view(json: &str, inline: Vec<PrComment>) -> Result<PrStatus> {
    let v: RawView = serde_json::from_str(json)?;
    let state = match v.state.as_str() {
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        s => bail!("unknown PR state {s}"),
    };
    let mut comments: Vec<PrComment> = v
        .comments
        .into_iter()
        .chain(v.reviews)
        .filter_map(comment)
        .chain(inline)
        .collect();
    comments.sort_by_key(|c| c.created_at);
    Ok(PrStatus {
        state,
        comments,
        checks: checks(v.status_check_rollup.as_deref().unwrap_or(&[])),
    })
}

pub fn status(repo: &Path, number: u64) -> Result<PrStatus> {
    let n = number.to_string();
    let view = gh(
        repo,
        &[
            "pr",
            "view",
            &n,
            "--json",
            "state,comments,reviews,statusCheckRollup",
        ],
    )?;
    let inline = gh(
        repo,
        &[
            "api",
            "--paginate",
            &format!("repos/{{owner}}/{{repo}}/pulls/{n}/comments"),
        ],
    )?;
    parse_view(&view, parse_inline(&inline)?)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    #[serde(default)]
    pub comments: Vec<String>,
    #[serde(default)]
    pub checks: Checks,
    #[serde(default)]
    pub closed: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Activity {
    pub comments: Vec<PrComment>,
    pub failing: Option<Vec<String>>,
    pub closed: bool,
}

impl Activity {
    pub fn is_empty(&self) -> bool {
        self.comments.is_empty() && self.failing.is_none() && !self.closed
    }
}

impl Seen {
    pub fn update(&mut self, s: &PrStatus) -> Activity {
        let known: HashSet<&str> = self.comments.iter().map(String::as_str).collect();
        let fresh: Vec<PrComment> = s
            .comments
            .iter()
            .filter(|c| !known.contains(c.id.as_str()))
            .cloned()
            .collect();
        self.comments.extend(fresh.iter().map(|c| c.id.clone()));
        let failing = match &s.checks {
            Checks::Failing(names) if self.checks != s.checks => Some(names.clone()),
            _ => None,
        };
        self.checks = s.checks.clone();
        let closed = s.state == PrState::Closed && !self.closed;
        self.closed = s.state == PrState::Closed;
        Activity {
            comments: fresh,
            failing,
            closed,
        }
    }
}

pub struct Watch {
    path: PathBuf,
    pub beads: BTreeMap<String, Seen>,
}

impl Watch {
    pub fn load(path: PathBuf) -> Self {
        let beads = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, beads }
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.beads)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

pub fn notes(number: u64, a: &Activity) -> String {
    let mut lines = Vec::new();
    if !a.comments.is_empty() {
        lines.push(format!(
            "PR #{number} review activity (address in code, never reply on GitHub):"
        ));
        lines.extend(a.comments.iter().map(|c| format!("- {}", c.describe())));
    }
    if let Some(names) = &a.failing {
        lines.push(format!("PR #{number} checks failing: {}", names.join(", ")));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_created_url_and_list() {
        assert_eq!(
            parse_created("Creating pull request\nhttps://github.com/o/r/pull/12\n"),
            Some(Pr {
                number: 12,
                url: "https://github.com/o/r/pull/12".into()
            })
        );
        assert_eq!(parse_created("nothing"), None);
        let pr = parse_list(r#"[{"number":3,"url":"https://github.com/o/r/pull/3"}]"#).unwrap();
        assert_eq!(pr.map(|p| p.number), Some(3));
        assert_eq!(parse_list("[]").unwrap(), None);
        assert_eq!(parse_list("").unwrap(), None);
    }
    const VIEW: &str = r#"{
        "state": "OPEN",
        "mergedAt": null,
        "headRefOid": "abc123",
        "comments": [
            {"id": "IC_1", "author": {"login": "alice"}, "body": "looks off", "createdAt": "2026-10-18T10:00:00Z"}
        ],
        "reviews": [
            {"id": "PRR_1", "author": {"login": "bob"}, "body": "", "state": "COMMENTED", "submittedAt": "2026-10-18T09:00:00Z"},
            {"id": "PRR_2", "author": {"login": "bob"}, "body": "", "state": "CHANGES_REQUESTED", "submittedAt": "2026-10-18T11:00:00Z"}
        ],
        "statusCheckRollup": [
            {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "FAILURE"},
            {"__typename": "CheckRun", "name": "lint", "status": "IN_PROGRESS", "conclusion": ""},
            {"__typename": "StatusContext", "context": "ci/legacy", "state": "ERROR"}
        ]
    }"#;

    const INLINE: &str = r#"[{"id": 7, "user": {"login": "carol"}, "body": "rename", "path": "src/a.rs", "line": null, "original_line": 12, "created_at": "2026-10-18T10:30:00Z"}][{"id": 8, "user": null, "body": "nit", "path": "src/b.rs", "line": 3, "created_at": "2026-10-18T12:00:00Z"}]"#;

    fn status_of(view: &str) -> PrStatus {
        parse_view(view, parse_inline(INLINE).unwrap()).unwrap()
    }

    #[test]
    fn parses_view_into_status() {
        let s = status_of(VIEW);
        assert_eq!(s.state, PrState::Open);
        let ids: Vec<_> = s.comments.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["IC_1", "7", "PRR_2", "8"]);
        assert_eq!(s.comments[1].describe(), "carol on src/a.rs:12: rename");
        assert_eq!(s.comments[2].body, "changes requested");
        assert_eq!(s.comments[3].author, "ghost");
        assert_eq!(
            s.checks,
            Checks::Failing(vec!["ci/legacy".into(), "test".into()])
        );
    }

    #[test]
    fn classifies_checks_and_states() {
        let merged = r#"{"state":"MERGED","statusCheckRollup":[{"__typename":"CheckRun","name":"b","status":"QUEUED","conclusion":""},{"__typename":"StatusContext","context":"c","state":"SUCCESS"}]}"#;
        let s = parse_view(merged, Vec::new()).unwrap();
        assert_eq!(s.state, PrState::Merged);
        assert_eq!(s.checks, Checks::Pending);
        let s = parse_view(r#"{"state":"CLOSED","statusCheckRollup":[{"__typename":"CheckRun","name":"b","status":"COMPLETED","conclusion":"SKIPPED"}]}"#, Vec::new()).unwrap();
        assert_eq!(s.state, PrState::Closed);
        assert_eq!(s.checks, Checks::Passing);
        let s = parse_view(r#"{"state":"OPEN","statusCheckRollup":null}"#, Vec::new()).unwrap();
        assert_eq!(s.checks, Checks::None);
        assert!(parse_view(r#"{"state":"WEIRD"}"#, Vec::new()).is_err());
        assert!(parse_inline("").unwrap().is_empty());
    }

    #[test]
    fn reports_only_new_comments_and_check_changes() {
        let mut seen = Seen::default();
        let s = status_of(VIEW);
        let a = seen.update(&s);
        assert_eq!(a.comments.len(), 4);
        assert_eq!(a.failing, Some(vec!["ci/legacy".into(), "test".into()]));
        assert!(!a.closed);
        assert!(seen.update(&s).is_empty());

        let json = serde_json::to_string(&seen).unwrap();
        let mut seen: Seen = serde_json::from_str(&json).unwrap();
        let mut more = s.clone();
        more.comments.push(PrComment {
            id: "IC_2".into(),
            author: "dave".into(),
            body: "ping".into(),
            path: None,
            line: None,
            created_at: None,
        });
        more.checks = Checks::Pending;
        let a = seen.update(&more);
        assert_eq!(
            a.comments.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["IC_2"]
        );
        assert_eq!(a.failing, None);
        assert_eq!(seen.checks, Checks::Pending);

        more.checks = Checks::Failing(vec!["test".into()]);
        more.state = PrState::Closed;
        let a = seen.update(&more);
        assert_eq!(a.failing, Some(vec!["test".into()]));
        assert!(a.closed);
        assert!(seen.update(&more).is_empty());
        assert_eq!(notes(4, &a), "PR #4 checks failing: test");
    }
}
