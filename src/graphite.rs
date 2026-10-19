use std::path::Path;
use std::process::Command;

use color_eyre::{Result, eyre::bail};

use crate::config::PrTool;
use crate::git;
use crate::health::Probe;

pub const GT: &str = "gt";

pub fn probe() -> Probe {
    Probe::new("gt", GT)
}

fn gt(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(GT).args(args).current_dir(dir).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = if err.trim().is_empty() {
            String::from_utf8_lossy(&out.stdout)
        } else {
            err
        };
        bail!("gt {}: {}", args.first().copied().unwrap_or(""), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn track_args<'a>(branch: &'a str, parent: &'a str) -> Vec<&'a str> {
    vec!["track", branch, "--parent", parent, "--no-interactive"]
}

fn submit_args(draft: bool) -> Vec<&'static str> {
    let mut args = vec![
        "submit",
        "--no-interactive",
        "--no-edit",
        "--no-ai",
        "--no-stack",
    ];
    if draft {
        args.push("--draft");
    }
    args
}

pub fn track(dir: &Path, branch: &str, parent: &str) -> Result<()> {
    gt(dir, &track_args(branch, parent)).map(|_| ())
}

pub fn submit(dir: &Path, draft: bool) -> Result<()> {
    gt(dir, &submit_args(draft)).map(|_| ())
}

fn untrack_args(branch: &str) -> Vec<&str> {
    vec!["untrack", branch, "--force", "--no-interactive"]
}

pub fn untrack(dir: &Path, branch: &str) -> Result<()> {
    gt(dir, &untrack_args(branch)).map(|_| ())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Track { id: String, parent: String },
    Submit(String),
}

pub fn plan(
    tool: PrTool,
    base: &str,
    moved: &[String],
    unmoved: &[String],
    stack_parent: impl Fn(&str) -> Option<String>,
    awaiting: impl Fn(&str) -> bool,
    clean: impl Fn(&str) -> bool,
) -> Vec<Step> {
    if tool != PrTool::Graphite {
        return Vec::new();
    }
    let mut seen = Vec::new();
    let mut steps = Vec::new();
    for id in moved.iter().chain(unmoved) {
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        let parent = stack_parent(id)
            .filter(|p| awaiting(p))
            .map(|p| git::branch(&p))
            .unwrap_or_else(|| base.to_string());
        steps.push(Step::Track {
            id: id.clone(),
            parent,
        });
    }
    steps.extend(
        moved
            .iter()
            .filter(|id| awaiting(id) && clean(id))
            .map(|id| Step::Submit(id.clone())),
    );
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn track(id: &str, parent: &str) -> Step {
        Step::Track {
            id: id.into(),
            parent: parent.into(),
        }
    }

    fn stack(id: &str) -> Option<String> {
        match id {
            "b" | "c" => Some("a".into()),
            "d" => Some("b".into()),
            _ => None,
        }
    }

    #[test]
    fn plan_is_a_noop_for_gh() {
        let steps = plan(
            PrTool::Gh,
            "main",
            &ids(&["b"]),
            &ids(&["c"]),
            stack,
            |_| true,
            |_| true,
        );
        assert!(steps.is_empty());
    }

    #[test]
    fn retracks_onto_awaiting_parent_else_base() {
        let awaiting = |id: &str| id == "a";
        let steps = plan(
            PrTool::Graphite,
            "main",
            &ids(&["b", "d"]),
            &ids(&["e"]),
            stack,
            awaiting,
            |_| true,
        );
        assert_eq!(
            steps,
            [
                track("b", "sulla/a"),
                track("d", "main"),
                track("e", "main"),
            ]
        );
    }

    #[test]
    fn resubmits_only_moved_clean_awaiting_branches() {
        let awaiting = |id: &str| ["a", "b", "c", "d"].contains(&id);
        let clean = |id: &str| id != "d";
        let steps = plan(
            PrTool::Graphite,
            "main",
            &ids(&["b", "d"]),
            &ids(&["c", "b"]),
            stack,
            awaiting,
            clean,
        );
        assert_eq!(
            steps,
            [
                track("b", "sulla/a"),
                track("d", "sulla/b"),
                track("c", "sulla/a"),
                Step::Submit("b".into()),
            ]
        );
    }

    #[test]
    fn untrack_never_prompts() {
        assert_eq!(
            untrack_args("sulla/a"),
            ["untrack", "sulla/a", "--force", "--no-interactive"]
        );
    }

    #[test]
    fn track_sets_parent_without_prompting() {
        assert_eq!(
            track_args("sulla/b", "main"),
            ["track", "sulla/b", "--parent", "main", "--no-interactive"]
        );
        assert_eq!(
            track_args("sulla/b", "sulla/a"),
            [
                "track",
                "sulla/b",
                "--parent",
                "sulla/a",
                "--no-interactive"
            ]
        );
    }

    #[test]
    fn submit_is_narrow_and_drafts_on_request() {
        let base = [
            "submit",
            "--no-interactive",
            "--no-edit",
            "--no-ai",
            "--no-stack",
        ];
        assert_eq!(submit_args(false), base);
        let mut draft = base.to_vec();
        draft.push("--draft");
        assert_eq!(submit_args(true), draft);
    }
}
