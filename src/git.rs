use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::{Result, eyre::bail};

#[derive(Debug, Clone)]
pub struct Git {
    pub repo: PathBuf,
}

fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Debug)]
pub struct RebaseConflict {
    pub base: String,
    pub detail: String,
}

impl std::fmt::Display for RebaseConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rebase onto {} failed, worktree left untouched: {}",
            self.base, self.detail
        )
    }
}

impl std::error::Error for RebaseConflict {}

pub fn toplevel(path: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        run(path, &["rev-parse", "--show-toplevel"])?.trim(),
    ))
}

pub fn branch(id: &str) -> String {
    format!("sulla/{id}")
}

fn merged_ref(id: &str, part: &str) -> String {
    format!("refs/sulla/merged/{id}/{part}")
}

impl Git {
    pub fn new(repo: PathBuf) -> Self {
        Self { repo }
    }

    pub fn current_branch(&self) -> Result<String> {
        Ok(run(&self.repo, &["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_string())
    }

    pub fn worktree_path(&self, id: &str) -> PathBuf {
        self.repo.join(".sulla").join("worktrees").join(id)
    }

    pub fn ensure_ignored(&self) -> Result<()> {
        let common = PathBuf::from(run(&self.repo, &["rev-parse", "--git-common-dir"])?.trim());
        let common = if common.is_absolute() {
            common
        } else {
            self.repo.join(common)
        };
        let info = common.join("info");
        std::fs::create_dir_all(&info)?;
        let exclude = info.join("exclude");
        let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !existing.lines().any(|l| l.trim() == "/.sulla/") {
            let sep = if existing.is_empty() || existing.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            std::fs::write(&exclude, format!("{existing}{sep}/.sulla/\n"))?;
        }
        Ok(())
    }

    fn branch_exists(&self, name: &str) -> bool {
        run(
            &self.repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ],
        )
        .is_ok()
    }

    pub fn ensure_worktree(&self, id: &str, base: &str) -> Result<PathBuf> {
        let path = self.worktree_path(id);
        if path.join(".git").exists() {
            return Ok(path);
        }
        let br = branch(id);
        let p = path.to_string_lossy();
        if self.branch_exists(&br) {
            run(&self.repo, &["worktree", "add", &p, &br])?;
        } else {
            run(&self.repo, &["worktree", "add", "-b", &br, &p, base])?;
        }
        Ok(path)
    }

    pub fn diff(&self, id: &str, base: &str) -> Result<String> {
        let wt = self.worktree_path(id);
        let br = branch(id);
        if !wt.join(".git").exists() {
            return run(
                &self.repo,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    &format!("{base}...{br}"),
                ],
            );
        }
        let mb = run(&wt, &["merge-base", base, "HEAD"])?;
        let mut out = run(&wt, &["diff", "--no-color", "--no-ext-diff", mb.trim()])?;
        for f in run(&wt, &["ls-files", "--others", "--exclude-standard"])?.lines() {
            let o = Command::new("git")
                .arg("-C")
                .arg(&wt)
                .args(["diff", "--no-color", "--no-index", "--", "/dev/null", f])
                .output()?;
            out.push_str(&String::from_utf8_lossy(&o.stdout));
        }
        Ok(out)
    }

    pub fn log(&self, id: &str, base: &str) -> Result<String> {
        let wt = self.worktree_path(id);
        if wt.join(".git").exists() {
            Self::log_range(&wt, &format!("{base}..HEAD"))
        } else {
            Self::log_range(&self.repo, &format!("{base}..{}", branch(id)))
        }
    }

    pub fn merged_log(&self, id: &str) -> Result<String> {
        let (b, h) = (merged_ref(id, "base"), merged_ref(id, "head"));
        Self::log_range(&self.repo, &format!("{b}..{h}"))
    }

    fn log_range(dir: &Path, range: &str) -> Result<String> {
        run(
            dir,
            &[
                "log",
                "--no-color",
                "--format=commit %H%nAuthor: %an <%ae>%nDate:   %ad%n%n%B",
                range,
            ],
        )
    }

    pub fn merge(&self, id: &str, base: &str, message: &str) -> Result<()> {
        let wt = self.worktree_path(id);
        let br = branch(id);
        if !wt.join(".git").exists() {
            bail!("no worktree for {id}");
        }
        if !run(&wt, &["status", "--porcelain"])?.trim().is_empty() {
            run(&wt, &["add", "-A"])?;
            run(&wt, &["commit", "-m", message])?;
        }
        if run(&wt, &["rev-list", "--count", &format!("{base}..HEAD")])?.trim() == "0" {
            bail!("{br} has no commits beyond {base}");
        }
        if let Err(e) = run(&wt, &["rebase", base]) {
            let _ = run(&wt, &["rebase", "--abort"]);
            return Err(RebaseConflict {
                base: base.to_string(),
                detail: e.to_string(),
            }
            .into());
        }
        let merged_base = run(&wt, &["merge-base", base, "HEAD"])?.trim().to_string();
        let merged_head = run(&wt, &["rev-parse", "HEAD"])?.trim().to_string();
        if self.current_branch()? == base {
            let blocked = self.dirty_overlap(base, &br)?;
            if !blocked.is_empty() {
                bail!(
                    "uncommitted changes in {} conflict with {br}: {}. Commit or stash them first",
                    self.repo.display(),
                    blocked.join(", ")
                );
            }
            run(&self.repo, &["merge", "--ff-only", &br])?;
        } else {
            run(&self.repo, &["fetch", ".", &format!("{br}:{base}")])?;
        }
        run(
            &self.repo,
            &["update-ref", &merged_ref(id, "base"), &merged_base],
        )?;
        run(
            &self.repo,
            &["update-ref", &merged_ref(id, "head"), &merged_head],
        )?;
        self.remove(id)
    }

    pub fn merged_diff(&self, id: &str) -> Result<String> {
        let (b, h) = (merged_ref(id, "base"), merged_ref(id, "head"));
        if !self.ref_exists(&b) || !self.ref_exists(&h) {
            bail!("no recorded merge for {id}");
        }
        run(&self.repo, &["diff", "--no-color", "--no-ext-diff", &b, &h])
    }

    pub fn was_merged(&self, id: &str) -> bool {
        !self.worktree_path(id).join(".git").exists()
            && self.ref_exists(&merged_ref(id, "base"))
            && self.ref_exists(&merged_ref(id, "head"))
    }

    fn ref_exists(&self, r: &str) -> bool {
        run(&self.repo, &["rev-parse", "--verify", "--quiet", r]).is_ok()
    }

    fn dirty_overlap(&self, base: &str, br: &str) -> Result<Vec<String>> {
        let changed = run(&self.repo, &["diff", "--name-only", base, br])?;
        let changed: Vec<&str> = changed.lines().collect();
        let status = run(
            &self.repo,
            &["status", "--porcelain", "--untracked-files=all"],
        )?;
        Ok(status
            .lines()
            .filter_map(|l| l.get(3..))
            .map(|p| p.rsplit(" -> ").next().unwrap_or(p).to_string())
            .filter(|p| changed.contains(&p.as_str()))
            .collect())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let wt = self.worktree_path(id);
        if wt.exists() {
            run(
                &self.repo,
                &["worktree", "remove", "--force", &wt.to_string_lossy()],
            )?;
        }
        let br = branch(id);
        if self.branch_exists(&br) {
            run(&self.repo, &["branch", "-D", &br])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        run(dir, args).unwrap();
    }

    fn scratch(name: &str) -> (PathBuf, Git) {
        let dir = std::env::temp_dir().join(format!("sulla-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@t"]);
        git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "init"]);
        let g = Git::new(dir.clone());
        g.ensure_ignored().unwrap();
        (dir, g)
    }

    #[test]
    fn merge_rebases_and_fast_forwards_without_merge_commits() {
        let (dir, g) = scratch("ff");
        let wt = g.ensure_worktree("x-1", "main").unwrap();
        std::fs::write(wt.join("b.txt"), "b\n").unwrap();
        std::fs::write(dir.join("c.txt"), "c\n").unwrap();
        git(&dir, &["add", "c.txt"]);
        git(&dir, &["commit", "-qm", "main moved"]);

        let diff = g.diff("x-1", "main").unwrap();
        assert!(diff.contains("b.txt"));
        assert!(!diff.contains("c.txt"));

        git(&wt, &["add", "b.txt"]);
        git(&wt, &["commit", "-qm", "add b\n\nbody line"]);
        let log = g.log("x-1", "main").unwrap();
        assert!(log.starts_with("commit ") && log.contains("add b\n\nbody line"));
        assert!(!log.contains("main moved") && !log.contains("init"));

        g.merge("x-1", "main", "x-1: work").unwrap();
        let merges = run(&dir, &["rev-list", "--merges", "--count", "main"]).unwrap();
        assert_eq!(merges.trim(), "0");
        assert!(dir.join("b.txt").exists());
        assert!(!wt.exists());
        assert!(!g.branch_exists("sulla/x-1"));
        let merged = g.merged_diff("x-1").unwrap();
        assert!(merged.contains("b.txt") && !merged.contains("c.txt"));
        assert!(g.merged_log("x-1").unwrap().contains("add b"));
        assert!(
            run(&dir, &["status", "--porcelain"])
                .unwrap()
                .trim()
                .is_empty()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dirty_base_checkout_blocks_merge_with_file_list() {
        let (dir, g) = scratch("dirty");
        let wt = g.ensure_worktree("x-3", "main").unwrap();
        std::fs::write(wt.join("new.rs"), "agent\n").unwrap();
        std::fs::write(dir.join("new.rs"), "human\n").unwrap();
        std::fs::write(dir.join("other.rs"), "unrelated\n").unwrap();

        let err = g.merge("x-3", "main", "x-3").unwrap_err().to_string();
        assert!(err.contains("new.rs"), "{err}");
        assert!(!err.contains("other.rs"), "{err}");
        assert!(wt.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn conflicting_merge_aborts_and_keeps_worktree() {
        let (dir, g) = scratch("conflict");
        let wt = g.ensure_worktree("x-2", "main").unwrap();
        std::fs::write(wt.join("a.txt"), "agent\n").unwrap();
        std::fs::write(dir.join("a.txt"), "human\n").unwrap();
        git(&dir, &["commit", "-qam", "human"]);

        let err = g.merge("x-2", "main", "x-2").unwrap_err();
        assert!(err.downcast_ref::<RebaseConflict>().is_some());
        assert!(wt.exists());
        assert!(
            run(&wt, &["status", "--porcelain"])
                .unwrap()
                .trim()
                .is_empty()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
