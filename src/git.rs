use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use color_eyre::{Result, eyre::bail};

use crate::ui::review::Rejection;

pub const DEFAULT_PREFIX: &str = "sulla/";

#[derive(Debug, Clone)]
pub struct Git {
    pub repo: PathBuf,
    prefix: String,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub files: usize,
    pub adds: usize,
    pub dels: usize,
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

fn merged_ref(id: &str, part: &str) -> String {
    format!("refs/sulla/merged/{id}/{part}")
}

fn stack_ref(id: &str) -> String {
    format!("refs/sulla/stack/{id}")
}

fn revision_ref(id: &str) -> String {
    format!("refs/sulla/revise/{id}")
}

fn released_ref(id: &str) -> String {
    format!("refs/sulla/released/{id}")
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || a.canonicalize()
            .ok()
            .zip(b.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
}

fn parent_key(id: &str) -> String {
    format!("sulla.{id}.parent")
}

impl Git {
    pub fn new(repo: PathBuf) -> Self {
        Self {
            repo,
            prefix: DEFAULT_PREFIX.into(),
        }
    }

    pub fn with_prefix(mut self, prefix: &str) -> Self {
        self.prefix = prefix.to_string();
        self
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn branch(&self, id: &str) -> String {
        format!("{}{id}", self.prefix)
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

    pub fn has_branch(&self, id: &str) -> bool {
        self.branch_exists(&self.branch(id))
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
        let br = self.branch(id);
        let p = path.to_string_lossy();
        if self.branch_exists(&br) {
            run(&self.repo, &["worktree", "add", &p, &br])?;
        } else {
            run(&self.repo, &["worktree", "add", "-b", &br, &p, base])?;
        }
        Ok(path)
    }

    pub fn ensure_stacked_worktree(&self, id: &str, parent: &str) -> Result<PathBuf> {
        if self.worktree_path(id).join(".git").exists() || self.branch_exists(&self.branch(id)) {
            return self.ensure_worktree(id, &self.branch(parent));
        }
        let tip = run(&self.repo, &["rev-parse", &self.branch(parent)])?
            .trim()
            .to_string();
        let path = self.ensure_worktree(id, &tip)?;
        run(&self.repo, &["update-ref", &stack_ref(id), &tip])?;
        run(&self.repo, &["config", &parent_key(id), parent])?;
        Ok(path)
    }

    pub fn adopt_parent(&self, id: &str, parent: &str) -> Result<()> {
        let r = stack_ref(id);
        let br = self.branch(id);
        let tip = if self.has_branch(parent) {
            self.branch(parent)
        } else {
            merged_ref(parent, "head")
        };
        let fork = run(&self.repo, &["merge-base", &tip, &br]).map(|f| f.trim().to_string());
        let ancestor =
            |a: &str, b: &str| run(&self.repo, &["merge-base", "--is-ancestor", a, b]).is_ok();
        let kept = self.ref_exists(&r)
            && ancestor(&r, &br)
            && !fork.as_ref().is_ok_and(|f| ancestor(&r, f));
        if !kept {
            run(&self.repo, &["update-ref", &r, &fork?])?;
        }
        run(&self.repo, &["config", &parent_key(id), parent]).map(drop)
    }

    pub fn stack_parent(&self, id: &str) -> Option<String> {
        if !self.ref_exists(&stack_ref(id)) {
            return None;
        }
        run(&self.repo, &["config", "--get", &parent_key(id)])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn behind_parent(&self, id: &str) -> bool {
        self.stack_parent(id).is_some_and(|p| {
            run(
                &self.repo,
                &[
                    "merge-base",
                    "--is-ancestor",
                    &self.branch(&p),
                    &self.branch(id),
                ],
            )
            .is_err()
        })
    }

    pub fn stacks(&self) -> Vec<(String, String)> {
        let Ok(out) = run(
            &self.repo,
            &["config", "--get-regexp", r"^sulla\..*\.parent$"],
        ) else {
            return Vec::new();
        };
        out.lines()
            .filter_map(|l| {
                let (k, v) = l.split_once(' ')?;
                let id = k.strip_prefix("sulla.")?.strip_suffix(".parent")?;
                self.ref_exists(&stack_ref(id))
                    .then(|| (id.to_string(), v.trim().to_string()))
            })
            .collect()
    }

    pub fn children(&self, parent: &str) -> Vec<String> {
        self.stacks()
            .into_iter()
            .filter(|(_, p)| p == parent)
            .map(|(id, _)| id)
            .collect()
    }

    pub fn unmerged_branch(&self, id: &str, base: &str) -> bool {
        let br = self.branch(id);
        let merged = merged_ref(id, "head");
        self.branch_exists(&br)
            && !(self.ref_exists(&merged)
                && run(&self.repo, &["merge-base", "--is-ancestor", &merged, &br]).is_ok())
            && run(&self.repo, &["merge-base", "--is-ancestor", &br, base]).is_err()
    }

    pub fn unstack(&self, id: &str) {
        let _ = run(&self.repo, &["update-ref", "-d", &stack_ref(id)]);
        let _ = run(&self.repo, &["config", "--unset", &parent_key(id)]);
    }

    pub fn upstream(&self, id: &str, base: &str) -> String {
        let r = stack_ref(id);
        let head = if self.worktree_path(id).join(".git").exists() {
            "HEAD".to_string()
        } else {
            self.branch(id)
        };
        let dir = if head == "HEAD" {
            self.worktree_path(id)
        } else {
            self.repo.clone()
        };
        if self.ref_exists(&r) && run(&dir, &["merge-base", "--is-ancestor", &r, &head]).is_ok() {
            r
        } else {
            base.to_string()
        }
    }

    pub fn restack(&self, id: &str, base: &str) -> Result<()> {
        let Some(parent) = self.stack_parent(id) else {
            return Ok(());
        };
        let wt = self.worktree_path(id);
        if !wt.join(".git").exists() {
            bail!("no worktree for {id}");
        }
        let onto = if self.unmerged_branch(&parent, base) {
            self.branch(&parent)
        } else {
            base.to_string()
        };
        if !run(&wt, &["status", "--porcelain"])?.trim().is_empty() {
            bail!("{id} has uncommitted changes");
        }
        let from = self.upstream(id, base);
        if let Err(e) = run(&wt, &["rebase", "--onto", &onto, &from]) {
            let _ = run(&wt, &["rebase", "--abort"]);
            return Err(RebaseConflict {
                base: onto,
                detail: e.to_string(),
            }
            .into());
        }
        if onto == base {
            self.unstack(id);
        } else {
            let tip = run(&self.repo, &["rev-parse", &onto])?.trim().to_string();
            run(&self.repo, &["update-ref", &stack_ref(id), &tip])?;
        }
        Ok(())
    }

    pub fn rebase(&self, id: &str, onto: &str) -> Result<()> {
        if self.stack_parent(id).is_some() {
            return self.restack(id, onto);
        }
        let wt = self.worktree_path(id);
        if !wt.join(".git").exists() {
            bail!("no worktree for {id}");
        }
        if !run(&wt, &["status", "--porcelain"])?.trim().is_empty() {
            bail!("{id} has uncommitted changes");
        }
        if let Err(e) = run(&wt, &["rebase", onto]) {
            let _ = run(&wt, &["rebase", "--abort"]);
            return Err(RebaseConflict {
                base: onto.to_string(),
                detail: e.to_string(),
            }
            .into());
        }
        Ok(())
    }

    pub fn mark_revision(&self, id: &str) -> Result<()> {
        let head = run(&self.worktree_path(id), &["rev-parse", "HEAD"])?;
        run(&self.repo, &["update-ref", &revision_ref(id), head.trim()]).map(drop)
    }

    pub fn clear_revision(&self, id: &str) {
        let _ = run(&self.repo, &["update-ref", "-d", &revision_ref(id)]);
    }

    pub fn revision(&self, id: &str) -> Option<String> {
        let r = revision_ref(id);
        if !self.ref_exists(&r) {
            return None;
        }
        let wt = self.worktree_path(id);
        run(&wt, &["merge-base", "--is-ancestor", &r, "HEAD"]).ok()?;
        let moved = run(&wt, &["rev-parse", &r, "HEAD"]).is_ok_and(|out| {
            let mut revs = out.lines();
            revs.next() != revs.next()
        });
        (moved || self.dirty(id).is_ok_and(|s| !s.trim().is_empty())).then_some(r)
    }

    fn review_base(&self, id: &str, base: &str) -> Result<String> {
        if let Some(r) = self.revision(id) {
            return Ok(r);
        }
        let up = self.upstream(id, base);
        Ok(run(&self.worktree_path(id), &["merge-base", &up, "HEAD"])?
            .trim()
            .to_string())
    }

    pub fn diff(&self, id: &str, base: &str) -> Result<String> {
        let wt = self.worktree_path(id);
        let br = self.branch(id);
        if !wt.join(".git").exists() {
            let up = self.upstream(id, base);
            return run(
                &self.repo,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    &format!("{up}...{br}"),
                ],
            );
        }
        let mb = self.review_base(id, base)?;
        let mut out = run(&wt, &["diff", "--no-color", "--no-ext-diff", &mb])?;
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

    pub fn diff_stat(&self, id: &str, base: &str) -> Result<Option<DiffStat>> {
        let wt = self.worktree_path(id);
        if !wt.join(".git").exists() {
            return Ok(None);
        }
        let mb = self.review_base(id, base)?;
        let mut s = DiffStat::default();
        for l in run(&wt, &["diff", "--numstat", "--no-ext-diff", &mb])?.lines() {
            let mut cols = l.split('\t');
            s.files += 1;
            s.adds += cols.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            s.dels += cols.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        }
        for f in run(&wt, &["ls-files", "--others", "--exclude-standard"])?.lines() {
            s.files += 1;
            if let Ok(bytes) = std::fs::read(wt.join(f))
                && !bytes.contains(&0)
            {
                s.adds += bytes.iter().filter(|b| **b == b'\n').count()
                    + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
            }
        }
        Ok(Some(s))
    }

    pub fn log(&self, id: &str, base: &str) -> Result<String> {
        let wt = self.worktree_path(id);
        if wt.join(".git").exists() {
            let up = self.revision(id).unwrap_or_else(|| self.upstream(id, base));
            Self::log_range(&wt, &format!("{up}..HEAD"))
        } else {
            let up = self.upstream(id, base);
            Self::log_range(&self.repo, &format!("{up}..{}", self.branch(id)))
        }
    }

    pub fn summary(&self, id: &str, base: &str) -> Result<String> {
        let up = self.upstream(id, base);
        run(
            &self.worktree_path(id),
            &[
                "log",
                "--no-color",
                "--format=%h %s",
                &format!("{up}..HEAD"),
            ],
        )
    }

    pub fn first_commit(&self, id: &str, base: &str) -> Result<Option<(String, String)>> {
        let wt = self.worktree_path(id);
        let up = self.upstream(id, base);
        let revs = run(&wt, &["rev-list", "--reverse", &format!("{up}..HEAD")])?;
        let Some(first) = revs.lines().next() else {
            return Ok(None);
        };
        let subject = run(&wt, &["log", "-1", "--format=%s", first])?;
        let body = run(&wt, &["log", "-1", "--format=%b", first])?;
        Ok(Some((subject.trim().to_string(), body.trim().to_string())))
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

    fn commit_all(wt: &Path, message: &str) -> Result<()> {
        if !run(wt, &["status", "--porcelain"])?.trim().is_empty() {
            run(wt, &["add", "-A"])?;
            run(wt, &["commit", "-m", message])?;
        }
        Ok(())
    }

    pub fn drop_rejected(
        &self,
        id: &str,
        base: &str,
        message: &str,
        rejection: &Rejection,
    ) -> Result<()> {
        if rejection.is_empty() {
            return Ok(());
        }
        let wt = self.worktree_path(id);
        if !wt.join(".git").exists() {
            bail!("no worktree for {id}");
        }
        Self::commit_all(&wt, message)?;
        if !rejection.patch.is_empty() {
            let mut child = Command::new("git")
                .arg("-C")
                .arg(&wt)
                .args(["apply", "-R", "--whitespace=nowarn", "-"])
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(rejection.patch.as_bytes())?;
            }
            let out = child.wait_with_output()?;
            if !out.status.success() {
                bail!(
                    "rejected hunks no longer apply, reload the diff (R): {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
        let mb = self.review_base(id, base)?;
        for p in &rejection.restore {
            if run(&wt, &["cat-file", "-e", &format!("{mb}:{p}")]).is_ok() {
                run(&wt, &["checkout", &mb, "--", p])?;
            } else {
                run(&wt, &["rm", "-q", "-f", "--ignore-unmatch", "--", p])?;
            }
        }
        Self::commit_all(&wt, &format!("Drop changes rejected in review of {id}"))
    }

    #[cfg(test)]
    pub fn merge(&self, id: &str, base: &str, message: &str) -> Result<()> {
        self.land(id, base, message)?;
        self.remove(id)
    }

    pub fn land(&self, id: &str, base: &str, message: &str) -> Result<()> {
        let wt = self.worktree_path(id);
        let br = self.branch(id);
        if !wt.join(".git").exists() {
            bail!("no worktree for {id}");
        }
        Self::commit_all(&wt, message)?;
        let up = self.upstream(id, base);
        if run(&wt, &["rev-list", "--count", &format!("{up}..HEAD")])?.trim() == "0" {
            bail!("{br} has no commits beyond {up}");
        }
        if let Err(e) = run(&wt, &["rebase", "--onto", base, &up]) {
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
        Ok(())
    }

    pub fn push(&self, id: &str, remote: &str, force: bool) -> Result<()> {
        let br = self.branch(id);
        let refspec = format!("{br}:{br}");
        let mut args = vec!["push", "--set-upstream"];
        if force {
            args.push("--force-with-lease");
        }
        args.extend([remote, refspec.as_str()]);
        run(&self.repo, &args).map(drop)
    }

    pub fn fetch_base(&self, remote: &str, base: &str) -> Result<String> {
        run(&self.repo, &["fetch", "--quiet", remote, base])?;
        let r = format!("refs/remotes/{remote}/{base}");
        self.rev(&r)?;
        Ok(r)
    }

    pub fn sync_trunk(&self, remote: &str, trunk: &str) -> Result<()> {
        let upstream = self.fetch_base(remote, trunk)?;
        let local = format!("refs/heads/{trunk}");
        if !self.ref_exists(&local) || self.rev(&local)? == self.rev(&upstream)? {
            return Ok(());
        }
        if run(
            &self.repo,
            &["merge-base", "--is-ancestor", &local, &upstream],
        )
        .is_err()
        {
            return Ok(());
        }
        if self.current_branch()? != trunk {
            return run(&self.repo, &["fetch", ".", &format!("{upstream}:{local}")]).map(drop);
        }
        let blocked = self.dirty_overlap(trunk, &upstream)?;
        if !blocked.is_empty() {
            bail!(
                "{trunk} is behind {remote} and uncommitted changes in {} block fast-forwarding it: {}. Commit or stash them first",
                self.repo.display(),
                blocked.join(", ")
            );
        }
        run(&self.repo, &["merge", "--ff-only", "--quiet", &upstream]).map(drop)
    }

    pub fn retire(&self, id: &str, base: &str) -> Result<Option<PathBuf>> {
        let br = self.branch(id);
        if self.branch_exists(&br) {
            let up = self.upstream(id, base);
            let merged_base = run(&self.repo, &["merge-base", &up, &br])?
                .trim()
                .to_string();
            let merged_head = self.head(id)?;
            run(
                &self.repo,
                &["update-ref", &merged_ref(id, "base"), &merged_base],
            )?;
            run(
                &self.repo,
                &["update-ref", &merged_ref(id, "head"), &merged_head],
            )?;
        }
        let Some(kept) = self.checked_out(id) else {
            return self.remove(id).map(|()| None);
        };
        self.forget(id)?;
        Ok(Some(kept))
    }

    pub fn release(&self, id: &str) -> Result<()> {
        let head = self.head(id)?;
        run(&self.repo, &["update-ref", &released_ref(id), &head])?;
        let wt = self.worktree_path(id);
        if wt.exists() {
            run(
                &self.repo,
                &["worktree", "remove", "--force", &wt.to_string_lossy()],
            )?;
        }
        Ok(())
    }

    pub fn reclaim(&self, id: &str, base: &str) -> Result<String> {
        let wt = self.ensure_worktree(id, &self.branch(id))?;
        let r = released_ref(id);
        if !self.ref_exists(&r) {
            return Ok(String::new());
        }
        let released = self.rev(&r)?;
        let mut log = String::new();
        if released != self.head(id)? {
            run(&self.repo, &["update-ref", &revision_ref(id), &released])?;
            let up = self.upstream(id, base);
            log = run(
                &wt,
                &[
                    "log",
                    "--oneline",
                    "--no-color",
                    "--no-decorate",
                    "--right-only",
                    "--cherry-pick",
                    &format!("{released}...HEAD"),
                    &format!("^{up}"),
                    "--",
                ],
            )?;
        }
        run(&self.repo, &["update-ref", "-d", &r])?;
        Ok(log.trim().to_string())
    }

    pub fn checked_out(&self, id: &str) -> Option<PathBuf> {
        let head = format!("branch refs/heads/{}", self.branch(id));
        let own = self.worktree_path(id);
        let out = run(&self.repo, &["worktree", "list", "--porcelain"]).ok()?;
        out.split("\n\n")
            .filter(|w| w.lines().any(|l| l == head))
            .filter_map(|w| w.lines().find_map(|l| l.strip_prefix("worktree ")))
            .map(PathBuf::from)
            .find(|p| !same_path(p, &own))
    }

    pub fn commit_pending(&self, id: &str, message: &str) -> Result<()> {
        let wt = self.worktree_path(id);
        if !wt.join(".git").exists() {
            return Ok(());
        }
        Self::commit_all(&wt, message)
    }

    pub fn merged_diff(&self, id: &str) -> Result<String> {
        let (b, h) = (merged_ref(id, "base"), merged_ref(id, "head"));
        if !self.ref_exists(&b) || !self.ref_exists(&h) {
            bail!("no recorded merge for {id}");
        }
        run(&self.repo, &["diff", "--no-color", "--no-ext-diff", &b, &h])
    }

    pub fn head(&self, id: &str) -> Result<String> {
        Ok(run(
            &self.repo,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{}", self.branch(id)),
            ],
        )?
        .trim()
        .to_string())
    }

    pub fn dirty(&self, id: &str) -> Result<String> {
        run(&self.worktree_path(id), &["status", "--porcelain"])
    }

    pub fn was_merged(&self, id: &str) -> bool {
        !self.worktree_path(id).join(".git").exists()
            && self.ref_exists(&merged_ref(id, "base"))
            && self.ref_exists(&merged_ref(id, "head"))
    }

    pub fn rev(&self, r: &str) -> Result<String> {
        Ok(run(&self.repo, &["rev-parse", "--verify", r])?
            .trim()
            .to_string())
    }

    pub fn bead_branches(&self) -> Result<Vec<(String, String)>> {
        let out = run(
            &self.repo,
            &[
                "for-each-ref",
                "--format=%(refname:lstrip=2) %(objectname)",
                "refs/heads/",
            ],
        )?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_once(' '))
            .filter_map(|(br, sha)| {
                Some((br.strip_prefix(&self.prefix)?.to_string(), sha.to_string()))
            })
            .collect())
    }

    pub fn merge_conflicts(&self, a: &str, b: &str) -> Result<Vec<String>> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args([
                "merge-tree",
                "-z",
                "--write-tree",
                "--name-only",
                "--no-messages",
                a,
                b,
            ])
            .output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        match out.status.code() {
            Some(0) => Ok(Vec::new()),
            Some(1) if !stdout.trim().is_empty() => {
                let mut files: Vec<String> = stdout
                    .split('\0')
                    .skip(1)
                    .take_while(|f| !f.is_empty())
                    .map(str::to_string)
                    .collect();
                files.dedup();
                Ok(files)
            }
            _ => bail!(
                "git merge-tree {a} {b}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
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

    pub fn discard(&self, id: &str) -> Result<()> {
        for c in self.children(id) {
            self.unstack(&c);
        }
        self.remove(id)
    }

    pub fn fingerprint(&self, id: &str) -> Option<String> {
        let wt = self.worktree_path(id);
        let head = run(&wt, &["rev-parse", "HEAD"]).ok()?;
        let status = run(&wt, &["status", "--porcelain"]).ok()?;
        Some(format!("{head}{status}"))
    }

    fn forget(&self, id: &str) -> Result<()> {
        self.unstack(id);
        self.clear_revision(id);
        let _ = run(&self.repo, &["update-ref", "-d", &released_ref(id)]);
        let wt = self.worktree_path(id);
        if wt.exists() {
            run(
                &self.repo,
                &["worktree", "remove", "--force", &wt.to_string_lossy()],
            )?;
        }
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.forget(id)?;
        let br = self.branch(id);
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
    fn branches_use_the_configured_prefix() {
        let (dir, g) = scratch("prefix");
        let g = g.with_prefix("blake/");
        assert_eq!(g.branch("n-1"), "blake/n-1");
        let wt = g.ensure_worktree("n-1", "main").unwrap();
        assert_eq!(
            run(&wt, &["branch", "--show-current"]).unwrap().trim(),
            "blake/n-1"
        );
        assert!(g.has_branch("n-1"));
        git(&dir, &["branch", "sulla/old"]);
        let mut ids: Vec<String> = g
            .bead_branches()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        assert_eq!(ids, ["n-1"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn revision_rebases_and_scopes_review_to_new_commits() {
        let (dir, g) = scratch("revise");
        let wt = g.ensure_worktree("r-1", "main").unwrap();
        std::fs::write(wt.join("b.txt"), "b\n").unwrap();
        g.commit_pending("r-1", "r-1: work").unwrap();
        std::fs::write(dir.join("m.txt"), "m\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "main moved"]);
        g.rebase("r-1", "main").unwrap();
        assert!(run(&wt, &["merge-base", "--is-ancestor", "main", "HEAD"]).is_ok());

        g.mark_revision("r-1").unwrap();
        let d = g.diff("r-1", "main").unwrap();
        assert!(d.contains("b.txt"));
        assert!(g.log("r-1", "main").unwrap().contains("r-1: work"));
        assert_eq!(g.diff_stat("r-1", "main").unwrap().unwrap().files, 1);

        std::fs::write(wt.join("c.txt"), "c\n").unwrap();
        let d = g.diff("r-1", "main").unwrap();
        assert!(d.contains("c.txt") && !d.contains("b.txt"));
        g.commit_pending("r-1", "r-1: address feedback").unwrap();
        let d = g.diff("r-1", "main").unwrap();
        assert!(d.contains("c.txt") && !d.contains("b.txt"));
        let s = g.diff_stat("r-1", "main").unwrap().unwrap();
        assert_eq!((s.files, s.adds), (1, 1));
        let log = g.log("r-1", "main").unwrap();
        assert!(log.contains("address feedback") && !log.contains("r-1: work"));

        g.clear_revision("r-1");
        assert!(g.diff("r-1", "main").unwrap().contains("b.txt"));

        std::fs::write(dir.join("b.txt"), "main\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "conflict"]);
        let err = g.rebase("r-1", "main").unwrap_err();
        assert!(err.downcast_ref::<RebaseConflict>().is_some());
        assert!(g.dirty("r-1").unwrap().trim().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn push_sets_upstream_and_force_pushes_with_lease() {
        let (dir, g) = scratch("push");
        let remote = dir.with_extension("remote");
        let _ = std::fs::remove_dir_all(&remote);
        git(&dir, &["init", "-q", "--bare", &remote.to_string_lossy()]);
        git(
            &dir,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        let wt = g.ensure_worktree("p-1", "main").unwrap();
        std::fs::write(wt.join("b.txt"), "b\n").unwrap();
        g.commit_pending("p-1", "p-1: work").unwrap();
        g.push("p-1", "origin", false).unwrap();
        let up = run(&wt, &["rev-parse", "--abbrev-ref", "@{upstream}"]).unwrap();
        assert_eq!(up.trim(), "origin/sulla/p-1");
        let pushed = || {
            run(&dir, &["ls-remote", "origin", "refs/heads/sulla/p-1"])
                .unwrap()
                .split_whitespace()
                .next()
                .map(String::from)
        };
        assert_eq!(pushed(), g.head("p-1").ok());

        git(&wt, &["commit", "-q", "--amend", "-m", "p-1: amended"]);
        assert!(g.push("p-1", "origin", false).is_err());
        g.push("p-1", "origin", true).unwrap();
        assert_eq!(pushed(), g.head("p-1").ok());
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&remote).unwrap();
    }

    #[test]
    fn first_commit_is_oldest_beyond_upstream() {
        let (dir, g) = scratch("first");
        let wt = g.ensure_worktree("f-1", "main").unwrap();
        assert_eq!(g.first_commit("f-1", "main").unwrap(), None);
        std::fs::write(wt.join("b.txt"), "b\n").unwrap();
        git(&wt, &["add", "b.txt"]);
        git(
            &wt,
            &[
                "commit",
                "-q",
                "-m",
                "feat: add b\n\n### Summary\n\nAdds b.",
            ],
        );
        std::fs::write(wt.join("c.txt"), "c\n").unwrap();
        g.commit_pending("f-1", "f-1: work").unwrap();
        assert_eq!(
            g.first_commit("f-1", "main").unwrap(),
            Some(("feat: add b".into(), "### Summary\n\nAdds b.".into()))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sync_trunk_fast_forwards_trunk_checked_out_in_the_main_repo() {
        let (dir, g) = scratch("sync");
        let remote = dir.with_extension("remote");
        let other = dir.with_extension("other");
        for d in [&remote, &other] {
            let _ = std::fs::remove_dir_all(d);
        }
        git(&dir, &["init", "-q", "--bare", &remote.to_string_lossy()]);
        git(
            &dir,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        git(&dir, &["push", "-q", "origin", "main"]);
        g.ensure_worktree("s-1", "main").unwrap();
        git(
            &dir,
            &[
                "clone",
                "-q",
                "-b",
                "main",
                &remote.to_string_lossy(),
                &other.to_string_lossy(),
            ],
        );
        let advance = |name: &str| {
            std::fs::write(other.join(name), "x\n").unwrap();
            git(&other, &["add", name]);
            git(
                &other,
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-qm",
                    name,
                ],
            );
            git(&other, &["push", "-q", "origin", "main"]);
            g.rev("main").unwrap()
        };
        let upstream = || g.rev("refs/remotes/origin/main").unwrap();

        let stale = advance("u1.txt");
        g.sync_trunk("origin", "main").unwrap();
        assert_ne!(g.rev("main").unwrap(), stale);
        assert_eq!(g.rev("main").unwrap(), upstream());
        assert!(dir.join("u1.txt").exists());

        advance("u2.txt");
        std::fs::write(dir.join("u2.txt"), "mine\n").unwrap();
        let err = g.sync_trunk("origin", "main").unwrap_err().to_string();
        assert!(err.contains("u2.txt"), "{err}");
        std::fs::remove_file(dir.join("u2.txt")).unwrap();

        git(&dir, &["switch", "-q", "-c", "elsewhere"]);
        g.sync_trunk("origin", "main").unwrap();
        assert_eq!(g.rev("main").unwrap(), upstream());
        g.sync_trunk("origin", "main").unwrap();
        for d in [&dir, &remote, &other] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn retire_records_merge_and_children_restack_onto_remote_base() {
        let (dir, g) = scratch("retire");
        let remote = dir.with_extension("remote");
        let _ = std::fs::remove_dir_all(&remote);
        git(&dir, &["init", "-q", "--bare", &remote.to_string_lossy()]);
        git(
            &dir,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        git(&dir, &["push", "-q", "origin", "main"]);
        let wt = g.ensure_worktree("p", "main").unwrap();
        std::fs::write(wt.join("p.txt"), "p\n").unwrap();
        g.commit_pending("p", "p: work").unwrap();
        let cwt = g.ensure_stacked_worktree("c", "p").unwrap();
        std::fs::write(cwt.join("c.txt"), "c\n").unwrap();
        g.commit_pending("c", "c: work").unwrap();

        let squash = dir.with_extension("squash");
        let _ = std::fs::remove_dir_all(&squash);
        git(
            &dir,
            &[
                "clone",
                "-q",
                "-b",
                "main",
                &remote.to_string_lossy(),
                &squash.to_string_lossy(),
            ],
        );
        std::fs::write(squash.join("p.txt"), "p\n").unwrap();
        git(&squash, &["add", "p.txt"]);
        git(
            &squash,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "p (#1)",
            ],
        );
        git(&squash, &["push", "-q", "origin", "main"]);

        let children = g.children("p");
        assert_eq!(g.retire("p", "main").unwrap(), None);
        assert!(!wt.exists());
        assert!(!g.branch_exists("sulla/p"));
        assert!(g.merged_diff("p").unwrap().contains("p.txt"));
        assert_eq!(children, ["c"]);

        let onto = g.fetch_base("origin", "main").unwrap();
        g.restack("c", &onto).unwrap();
        assert_eq!(g.stack_parent("c"), None);
        let log = run(&cwt, &["log", "--format=%s", &format!("{onto}..HEAD")]).unwrap();
        assert_eq!(log.trim(), "c: work");
        for d in [&dir, &remote, &squash] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn released_branch_checked_out_elsewhere_survives_retire() {
        let (dir, g) = scratch("release");
        let wt = g.ensure_worktree("p", "main").unwrap();
        std::fs::write(wt.join("p.txt"), "p\n").unwrap();
        g.commit_pending("p", "p: work").unwrap();
        let cwt = g.ensure_stacked_worktree("c", "p").unwrap();
        g.mark_revision("p").unwrap();
        let head = g.head("p").unwrap();
        g.release("p").unwrap();
        assert!(!wt.exists());
        assert!(g.has_branch("p"));
        assert_eq!(g.rev("refs/sulla/released/p").unwrap(), head);
        assert!(g.ref_exists("refs/sulla/revise/p"));
        assert_eq!(g.checked_out("p"), None);
        assert_eq!(g.checked_out("c"), None);

        let other = dir.with_extension("other");
        let _ = std::fs::remove_dir_all(&other);
        git(
            &dir,
            &["worktree", "add", &other.to_string_lossy(), "sulla/p"],
        );
        let kept = g.checked_out("p").unwrap();
        assert!(same_path(&kept, &other), "{}", kept.display());
        assert!(g.unmerged_branch("p", "main"));

        assert_eq!(g.retire("p", "main").unwrap(), Some(kept));
        assert!(g.has_branch("p"));
        assert!(!g.ref_exists("refs/sulla/released/p"));
        assert!(!g.ref_exists("refs/sulla/revise/p"));
        assert!(g.merged_diff("p").unwrap().contains("p.txt"));
        assert!(!g.unmerged_branch("p", "main"));
        assert_eq!(g.stack_parent("c").as_deref(), Some("p"));
        g.restack("c", "main").unwrap();
        assert_eq!(g.stack_parent("c"), None);
        assert!(cwt.exists());
        git(
            &dir,
            &["worktree", "remove", "--force", &other.to_string_lossy()],
        );
        std::fs::remove_dir_all(&dir).unwrap();
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
        std::fs::write(wt.join("a.txt"), "x\ny\n").unwrap();
        assert_eq!(
            g.diff_stat("x-1", "main").unwrap(),
            Some(DiffStat {
                files: 2,
                adds: 3,
                dels: 1
            })
        );
        git(&wt, &["checkout", "a.txt"]);
        assert_eq!(g.diff_stat("nope", "main").unwrap(), None);
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
    fn merge_drops_rejected_hunks_and_files() {
        let (dir, g) = scratch("reject");
        let body: String = (1..=20).map(|n| format!("{n}\n")).collect();
        std::fs::write(dir.join("a.txt"), &body).unwrap();
        git(&dir, &["commit", "-qam", "lines"]);
        let wt = g.ensure_worktree("x-4", "main").unwrap();
        let changed = body.replace("2\n", "two\n").replace("19\n", "nineteen\n");
        std::fs::write(wt.join("a.txt"), changed).unwrap();
        std::fs::write(wt.join("junk.txt"), "junk\n").unwrap();
        git(&wt, &["add", "junk.txt"]);
        git(&wt, &["commit", "-qm", "work"]);
        std::fs::write(wt.join("keep.txt"), "keep\n").unwrap();

        let mut d = crate::ui::review::Diff::parse("x-4", &g.diff("x-4", "main").unwrap());
        let file = |d: &crate::ui::review::Diff, n: &str| {
            d.files.iter().position(|f| f.name == n).unwrap()
        };
        d.sel = file(&d, "a.txt");
        d.mark_hunk(false);
        d.sel = file(&d, "junk.txt");
        d.mark_file(false);
        let r = d.rejection();
        assert!(r.patch.contains("junk") && r.restore.is_empty());

        g.drop_rejected("x-4", "main", "x-4", &r).unwrap();
        g.merge("x-4", "main", "x-4").unwrap();
        let a = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert!(
            a.starts_with("1\n2\n3\n") && a.contains("nineteen\n"),
            "{a}"
        );
        assert!(!dir.join("junk.txt").exists());
        assert!(dir.join("keep.txt").exists());
        let log = run(&dir, &["log", "--format=%s", "main"]).unwrap();
        assert!(
            log.contains("Drop changes rejected in review of x-4"),
            "{log}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_rejection_aborts_without_touching_tree() {
        let (dir, g) = scratch("stale-reject");
        let wt = g.ensure_worktree("x-5", "main").unwrap();
        std::fs::write(wt.join("a.txt"), "b\n").unwrap();
        let mut d = crate::ui::review::Diff::parse("x-5", &g.diff("x-5", "main").unwrap());
        d.mark_hunk(false);
        std::fs::write(wt.join("a.txt"), "c\n").unwrap();
        let err = g.drop_rejected("x-5", "main", "x-5", &d.rejection());
        assert!(err.unwrap_err().to_string().contains("no longer apply"));
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "c\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stacked_chain_restacks_after_parent_merges() {
        let (dir, g) = scratch("stack");
        let commit = |wt: &Path, f: &str| {
            std::fs::write(wt.join(f), f).unwrap();
            git(wt, &["add", f]);
            git(wt, &["commit", "-qm", f]);
        };
        let p = g.ensure_worktree("p", "main").unwrap();
        commit(&p, "p.txt");
        let c = g.ensure_stacked_worktree("c", "p").unwrap();
        assert_eq!(g.stack_parent("c").as_deref(), Some("p"));
        commit(&c, "c.txt");
        let gc = g.ensure_stacked_worktree("gc", "c").unwrap();
        commit(&gc, "gc.txt");
        assert_eq!(g.children("p"), ["c"]);

        let diff = g.diff("c", "main").unwrap();
        assert!(diff.contains("c.txt") && !diff.contains("p.txt"), "{diff}");
        assert_eq!(g.diff_stat("c", "main").unwrap().unwrap().files, 1);
        assert!(!g.log("c", "main").unwrap().contains("p.txt"));

        commit(&p, "p2.txt");
        commit(&dir, "main.txt");
        g.merge("p", "main", "p").unwrap();
        assert_eq!(g.children("p"), ["c"]);
        g.restack("c", "main").unwrap();
        run(&c, &["merge-base", "--is-ancestor", "main", "HEAD"]).unwrap();
        assert!(c.join("p2.txt").exists() && c.join("main.txt").exists());
        assert_eq!(g.stack_parent("c"), None);
        assert_eq!(g.upstream("c", "main"), "main");
        assert_eq!(g.children("c"), ["gc"]);
        std::fs::write(gc.join("wip.txt"), "wip").unwrap();
        assert!(g.restack("gc", "main").is_err());
        assert!(gc.join("wip.txt").exists());
        std::fs::remove_file(gc.join("wip.txt")).unwrap();
        g.restack("gc", "main").unwrap();
        assert_eq!(g.stack_parent("gc").as_deref(), Some("c"));
        run(&gc, &["merge-base", "--is-ancestor", "sulla/c", "HEAD"]).unwrap();
        assert!(gc.join("main.txt").exists());
        std::fs::write(gc.join("wip.txt"), "wip").unwrap();
        let diff = g.diff("gc", "main").unwrap();
        assert!(
            diff.contains("gc.txt") && !diff.contains("b/c.txt"),
            "{diff}"
        );

        g.merge("c", "main", "c").unwrap();
        g.merge("gc", "main", "gc").unwrap();
        for f in ["p.txt", "p2.txt", "main.txt", "c.txt", "gc.txt", "wip.txt"] {
            assert!(dir.join(f).exists(), "{f}");
        }
        let n = run(&dir, &["rev-list", "--count", "main"]).unwrap();
        assert_eq!(n.trim(), "7");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discarding_parent_keeps_its_commits_in_children() {
        let (dir, g) = scratch("discard");
        let p = g.ensure_worktree("p", "main").unwrap();
        std::fs::write(p.join("p.txt"), "p").unwrap();
        git(&p, &["add", "p.txt"]);
        git(&p, &["commit", "-qm", "p"]);
        let c = g.ensure_stacked_worktree("c", "p").unwrap();
        std::fs::write(c.join("c.txt"), "c").unwrap();
        git(&c, &["add", "c.txt"]);
        git(&c, &["commit", "-qm", "c"]);

        g.discard("p").unwrap();
        assert_eq!(g.stack_parent("c"), None);
        assert_eq!(g.upstream("c", "main"), "main");
        let diff = g.diff("c", "main").unwrap();
        assert!(
            diff.contains("b/p.txt") && diff.contains("b/c.txt"),
            "{diff}"
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

    #[test]
    fn child_follows_parent_rewritten_after_stacking() {
        let (dir, g) = scratch("rewrite");
        let commit = |wt: &Path, f: &str, amend: bool| {
            std::fs::write(wt.join(f), f).unwrap();
            git(wt, &["add", f]);
            if amend {
                git(wt, &["commit", "-q", "--amend", "--no-edit"]);
            } else {
                git(wt, &["commit", "-qm", f]);
            }
        };
        let p = g.ensure_worktree("p", "main").unwrap();
        commit(&p, "p.txt", false);
        let c = g.ensure_stacked_worktree("c", "p").unwrap();
        commit(&c, "c.txt", false);
        assert!(!g.behind_parent("c"));

        commit(&p, "p2.txt", true);
        assert!(g.behind_parent("c"));
        g.restack("c", "main").unwrap();
        assert!(!g.behind_parent("c"));
        assert_eq!(g.stack_parent("c").as_deref(), Some("p"));
        assert!(c.join("p2.txt").exists() && c.join("c.txt").exists());
        assert_eq!(
            run(&c, &["rev-list", "--count", "sulla/p..HEAD"])
                .unwrap()
                .trim(),
            "1"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
