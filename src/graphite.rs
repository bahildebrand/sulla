use std::path::Path;
use std::process::Command;

use color_eyre::{Result, eyre::bail};

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

#[cfg(test)]
mod tests {
    use super::*;

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
