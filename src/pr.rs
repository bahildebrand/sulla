use std::path::Path;
use std::process::Command;

use color_eyre::{Result, eyre::bail};
use serde::Deserialize;

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
}
