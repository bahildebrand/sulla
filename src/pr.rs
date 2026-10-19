use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use color_eyre::{Result, eyre::bail};
use serde::{Deserialize, Serialize};

use crate::health::Probe;

pub const GH: &str = "gh";
pub const MARKER: &str = "<!-- sulla -->";
const GRAPHITE_STACK: &str = "<!-- Current dependencies on/for this PR: -->";
const LOG_LINES: usize = 40;
const LOG_WIDTH: usize = 240;

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

pub fn url(repo: &Path, number: u64) -> Result<String> {
    let n = number.to_string();
    Ok(
        gh(repo, &["pr", "view", &n, "--json", "url", "-q", ".url"])?
            .trim()
            .to_string(),
    )
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

pub fn edit(repo: &Path, number: u64, title: &str, body: &str) -> Result<()> {
    let n = number.to_string();
    gh(repo, &["pr", "edit", &n, "--title", title, "--body", body]).map(|_| ())
}

pub fn body(id: &str, description: &str) -> String {
    let description = description.trim();
    if description.is_empty() {
        format!("Bead: {id}")
    } else {
        format!("{description}\n\nBead: {id}")
    }
}

pub fn text(
    id: &str,
    title: &str,
    description: &str,
    commit: Option<(String, String)>,
) -> (String, String) {
    match commit {
        Some((subject, msg)) if !subject.is_empty() => {
            let body = if msg.is_empty() {
                body(id, description)
            } else {
                msg
            };
            (subject, body)
        }
        _ => (title.to_string(), body(id, description)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrComment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub approval: bool,
}

impl PrComment {
    fn location(&self) -> String {
        match (&self.path, self.line) {
            (Some(p), Some(l)) => format!(" on {p}:{l}"),
            (Some(p), None) => format!(" on {p}"),
            _ => String::new(),
        }
    }

    pub fn describe(&self) -> String {
        format!("{}{}: {}", self.author, self.location(), self.body.trim())
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
    let approval = r.state == "APPROVED";
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
        approval,
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
    comments.retain(|c| !c.body.contains(MARKER) && !c.body.contains(GRAPHITE_STACK));
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
    #[serde(default)]
    pub pending: Feedback,
    #[serde(default)]
    pub revising: Option<Feedback>,
    #[serde(default)]
    pub addressed: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
    #[serde(default)]
    pub comments: Vec<PrComment>,
    #[serde(default)]
    pub failing: Vec<String>,
}

impl Feedback {
    pub fn is_empty(&self) -> bool {
        self.comments.is_empty() && self.failing.is_empty()
    }

    pub fn count(&self) -> usize {
        self.comments.len() + self.failing.len()
    }
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

    pub fn queue(&mut self, a: &Activity) -> bool {
        let comments: Vec<PrComment> = a
            .comments
            .iter()
            .filter(|c| !c.approval && !self.addressed.contains(&c.id))
            .filter(|c| !self.pending.comments.iter().any(|p| p.id == c.id))
            .cloned()
            .collect();
        let failing = a.failing.clone().unwrap_or_default();
        if comments.is_empty() && failing.is_empty() {
            return false;
        }
        self.pending.comments.extend(comments);
        if !failing.is_empty() {
            self.pending.failing = failing;
        }
        true
    }

    pub fn take(&mut self) -> Feedback {
        let fb = std::mem::take(&mut self.pending);
        self.addressed
            .extend(fb.comments.iter().map(|c| c.id.clone()));
        let r = self.revising.get_or_insert_default();
        r.comments.extend(fb.comments.iter().cloned());
        if !fb.failing.is_empty() {
            r.failing = fb.failing.clone();
        }
        fb
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub name: String,
    pub log: Option<String>,
}

#[derive(Deserialize)]
struct RawPrCheck {
    #[serde(default)]
    name: String,
    #[serde(default)]
    bucket: String,
    #[serde(default)]
    link: String,
}

fn gh_lenient(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(GH).args(args).current_dir(repo).output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() && stdout.trim().is_empty() {
        bail!(
            "gh {}: {}",
            args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(stdout)
}

fn failing_links(json: &str) -> Result<Vec<(String, String)>> {
    if json.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str::<Vec<RawPrCheck>>(json)?
        .into_iter()
        .filter(|c| c.bucket == "fail")
        .map(|c| (c.name, c.link))
        .collect())
}

fn log_args(link: &str) -> Option<Vec<String>> {
    let id = |s: &str| {
        let s = s.split(['?', '#']).next()?;
        s.parse::<u64>().ok().map(|_| s.to_string())
    };
    let mut parts = link.split("/actions/runs/").nth(1)?.split('/');
    let run = id(parts.next()?)?;
    match (parts.next(), parts.next().and_then(id)) {
        (Some("job"), Some(job)) => Some(vec!["--job".into(), job]),
        _ => Some(vec![run]),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n).collect::<String>())
    }
}

pub fn excerpt(log: &str) -> Option<String> {
    let lines: Vec<String> = log
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .map(|l| truncate(l, LOG_WIDTH))
        .collect();
    let skip = lines.len().saturating_sub(LOG_LINES);
    (!lines.is_empty()).then(|| lines[skip..].join("\n"))
}

pub fn failures(repo: &Path, number: u64, names: &[String]) -> Vec<Failure> {
    if names.is_empty() {
        return Vec::new();
    }
    let n = number.to_string();
    let links = gh_lenient(repo, &["pr", "checks", &n, "--json", "name,bucket,link"])
        .and_then(|j| failing_links(&j))
        .unwrap_or_else(|e| {
            tracing::warn!("pr #{number} checks: {e:#}");
            Vec::new()
        });
    names
        .iter()
        .map(|name| {
            let log = links
                .iter()
                .find(|(n, _)| n == name)
                .and_then(|(_, l)| log_args(l))
                .and_then(|extra| {
                    let mut args = vec!["run", "view"];
                    args.extend(extra.iter().map(String::as_str));
                    args.push("--log-failed");
                    gh(repo, &args)
                        .map_err(|e| tracing::warn!("logs for {name}: {e:#}"))
                        .ok()
                })
                .and_then(|l| excerpt(&l));
            Failure {
                name: name.clone(),
                log,
            }
        })
        .collect()
}

pub fn revise_prompt(
    id: &str,
    number: u64,
    comments: &[PrComment],
    failures: &[Failure],
    conflict: Option<&str>,
) -> String {
    let mut out = format!(
        "PR #{number} for this bead got new feedback. Continue from your earlier work and address it.\n"
    );
    if let Some(c) = conflict {
        out.push_str(&format!("\n{c}\n"));
    }
    if !comments.is_empty() {
        out.push_str("\nReview comments:\n");
        for (i, c) in comments.iter().enumerate() {
            out.push_str(&format!("{}. {}{}:\n", i + 1, c.author, c.location()));
            for l in c.body.trim().lines() {
                out.push_str(&format!("   {l}\n"));
            }
        }
    }
    if !failures.is_empty() {
        out.push_str("\nFailing checks:\n");
        for f in failures {
            out.push_str(&format!("- {}\n", f.name));
            if let Some(log) = &f.log {
                out.push_str("  Log excerpt (gh run view --log-failed):\n  ```\n");
                for l in log.lines() {
                    out.push_str(&format!("  {l}\n"));
                }
                out.push_str("  ```\n");
            }
        }
    }
    out.push_str(&format!(
        "\nAddress each review comment, or explain in your final message why a comment should not lead to a change. Fix the failing checks. Commit your changes, then run the build and tests and make sure they pass. Do not push and do not comment on the PR, sulla publishes after human review. When done, run `bd set-state {id} sulla=review --reason \"addressed PR feedback\"` then stop and wait."
    ));
    out
}

pub fn reply(fb: &Feedback, head: &str) -> String {
    let short: String = head.chars().take(7).collect();
    let mut lines = vec![MARKER.to_string(), format!("Pushed `{short}` addressing:")];
    lines.extend(
        fb.comments
            .iter()
            .map(|c| format!("- {}{}", c.author, c.location())),
    );
    if !fb.failing.is_empty() {
        lines.push(format!("- failing checks: {}", fb.failing.join(", ")));
    }
    lines.join("\n")
}

pub fn post_comment(repo: &Path, number: u64, body: &str) -> Result<()> {
    gh(
        repo,
        &["pr", "comment", &number.to_string(), "--body", body],
    )
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_text_prefers_first_commit_over_bead() {
        let commit = |s: &str, b: &str| Some((s.to_string(), b.to_string()));
        assert_eq!(
            text(
                "m-1",
                "Bead title",
                "Bead desc",
                commit("feat: x", "### Summary")
            ),
            ("feat: x".into(), "### Summary".into())
        );
        assert_eq!(
            text("m-1", "Bead title", "Bead desc", commit("feat: x", "")),
            ("feat: x".into(), "Bead desc\n\nBead: m-1".into())
        );
        assert_eq!(
            text("m-1", "Bead title", "Bead desc", None),
            ("Bead title".into(), "Bead desc\n\nBead: m-1".into())
        );
    }

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
            approval: false,
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

    fn comment(id: &str, author: &str, body: &str, at: Option<(&str, u64)>) -> PrComment {
        PrComment {
            id: id.into(),
            author: author.into(),
            body: body.into(),
            path: at.map(|(p, _)| p.into()),
            line: at.map(|(_, l)| l),
            created_at: None,
            approval: false,
        }
    }

    #[test]
    fn builds_revise_prompt_from_comments_and_checks() {
        let comments = vec![
            comment(
                "rc1",
                "alice",
                "rename this\nand add a test",
                Some(("src/a.rs", 12)),
            ),
            comment("IC_1", "bob", "please split the PR", None),
        ];
        let failures = vec![
            Failure {
                name: "test".into(),
                log: Some("boom\nassert failed".into()),
            },
            Failure {
                name: "lint".into(),
                log: None,
            },
        ];
        let p = revise_prompt(
            "x-1",
            9,
            &comments,
            &failures,
            Some("Rebasing hit conflicts."),
        );
        assert_eq!(
            p,
            "PR #9 for this bead got new feedback. Continue from your earlier work and address it.\n\
             \nRebasing hit conflicts.\n\
             \nReview comments:\n\
             1. alice on src/a.rs:12:\n   rename this\n   and add a test\n\
             2. bob:\n   please split the PR\n\
             \nFailing checks:\n\
             - test\n  Log excerpt (gh run view --log-failed):\n  ```\n  boom\n  assert failed\n  ```\n\
             - lint\n\
             \nAddress each review comment, or explain in your final message why a comment should not lead to a change. Fix the failing checks. Commit your changes, then run the build and tests and make sure they pass. Do not push and do not comment on the PR, sulla publishes after human review. When done, run `bd set-state x-1 sulla=review --reason \"addressed PR feedback\"` then stop and wait."
        );
        let p = revise_prompt("x-1", 9, &comments[1..], &[], None);
        assert!(!p.contains("Failing checks"));
        assert!(!p.contains("conflicts"));
    }

    #[test]
    fn queues_actionable_feedback_and_marks_it_addressed() {
        let mut seen = Seen::default();
        let mut approve = comment("PRR_9", "bob", "approved", None);
        approve.approval = true;
        assert!(!seen.queue(&Activity {
            comments: vec![approve],
            ..Default::default()
        }));
        let a = Activity {
            comments: vec![comment("rc1", "alice", "fix", Some(("a.rs", 1)))],
            failing: Some(vec!["test".into()]),
            closed: false,
        };
        assert!(seen.queue(&a));
        assert!(!seen.queue(&Activity {
            comments: a.comments.clone(),
            ..Default::default()
        }));
        assert_eq!(seen.pending.count(), 2);
        let fb = seen.take();
        assert_eq!(fb.failing, ["test"]);
        assert!(seen.pending.is_empty());
        assert_eq!(seen.addressed, ["rc1"]);
        assert!(!seen.queue(&Activity {
            comments: a.comments.clone(),
            ..Default::default()
        }));
        let json = serde_json::to_string(&seen).unwrap();
        let back: Seen = serde_json::from_str(&json).unwrap();
        assert_eq!(back, seen);
        let body = reply(seen.revising.as_ref().unwrap(), "abcdef123456");
        assert_eq!(
            body,
            "<!-- sulla -->\nPushed `abcdef1` addressing:\n- alice on a.rs:1\n- failing checks: test"
        );
        let marked = format!(
            r#"{{"state":"OPEN","comments":[{{"id":"IC_9","author":{{"login":"me"}},"body":"{}"}}]}}"#,
            body.replace('\n', "\\n").replace('`', "")
        );
        assert!(parse_view(&marked, Vec::new()).unwrap().comments.is_empty());
    }

    #[test]
    fn ignores_graphite_stack_comment() {
        let view = r#"{"state":"OPEN","comments":[
            {"id":"IC_1","author":{"login":"me"},"body":"* **#12** \ud83d\udc48\n* `master`\n\nThis stack of pull requests is managed by Graphite.\n<!-- Current dependencies on/for this PR: -->"},
            {"id":"IC_2","author":{"login":"me"},"body":"rename this"}
        ]}"#;
        let ids: Vec<_> = parse_view(view, Vec::new())
            .unwrap()
            .comments
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["IC_2"]);
    }

    #[test]
    fn finds_failed_check_logs() {
        let json = r#"[
            {"name":"test","bucket":"fail","link":"https://github.com/o/r/actions/runs/11/job/22"},
            {"name":"lint","bucket":"fail","link":"https://github.com/o/r/actions/runs/33?check_suite_focus=true"},
            {"name":"build","bucket":"pass","link":"https://github.com/o/r/actions/runs/44/job/55"}
        ]"#;
        let links = failing_links(json).unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(
            log_args(&links[0].1),
            Some(vec!["--job".into(), "22".into()])
        );
        assert_eq!(log_args(&links[1].1), Some(vec!["33".into()]));
        assert_eq!(log_args("https://ci.example.com/build/1"), None);
        let log: String = (0..100).map(|i| format!("line {i}\n\n")).collect();
        let e = excerpt(&log).unwrap();
        assert_eq!(e.lines().count(), LOG_LINES);
        assert!(e.starts_with("line 60\n"));
        assert!(excerpt(&"x".repeat(500)).unwrap().ends_with("..."));
        assert_eq!(excerpt("  \n"), None);
    }
}
