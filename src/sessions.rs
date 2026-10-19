use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use color_eyre::Result;

use crate::config::AgentKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    pub copilot: PathBuf,
    pub claude: PathBuf,
}

impl Roots {
    pub fn from_env() -> Self {
        Self::from_vars(|k| std::env::var_os(k))
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<OsString>) -> Self {
        let home = get("HOME").map(PathBuf::from).unwrap_or_default();
        let root = |kind: AgentKind| {
            get(env_var(kind))
                .map(PathBuf::from)
                .unwrap_or_else(|| default_root(kind, &home))
        };
        Self {
            copilot: root(AgentKind::Copilot),
            claude: root(AgentKind::Claude),
        }
    }

    pub fn get(&self, kind: AgentKind) -> &Path {
        match kind {
            AgentKind::Copilot => &self.copilot,
            AgentKind::Claude => &self.claude,
        }
    }
}

pub fn env_var(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Copilot => "COPILOT_HOME",
        AgentKind::Claude => "CLAUDE_CONFIG_DIR",
    }
}

pub fn default_root(kind: AgentKind, home: &Path) -> PathBuf {
    match kind {
        AgentKind::Copilot => home.join(".copilot"),
        AgentKind::Claude => home.join(".claude"),
    }
}

pub fn backup_dir(repo: &Path, id: &str) -> PathBuf {
    repo.join(".sulla").join("sessions").join(id)
}

fn live(root: &Path, kind: AgentKind, session: &str) -> Vec<PathBuf> {
    match kind {
        AgentKind::Copilot => {
            let rel = Path::new("session-state").join(session);
            if root.join(&rel).is_dir() {
                vec![rel]
            } else {
                Vec::new()
            }
        }
        AgentKind::Claude => {
            let Ok(dirs) = fs::read_dir(root.join("projects")) else {
                return Vec::new();
            };
            let file = format!("{session}.jsonl");
            let mut out = Vec::new();
            for d in dirs.flatten() {
                let rel = Path::new("projects").join(d.file_name());
                if root.join(&rel).join(&file).is_file() {
                    out.push(rel.join(&file));
                    if root.join(&rel).join(session).is_dir() {
                        out.push(rel.join(session));
                    }
                }
            }
            out
        }
    }
}

pub fn backup(root: &Path, kind: AgentKind, session: &str, dest: &Path) -> Result<bool> {
    let found = live(root, kind, session);
    if found.is_empty() {
        return Ok(false);
    }
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    for rel in &found {
        copy_all(&root.join(rel), &tmp.join(kind.name()).join(rel))?;
    }
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    fs::rename(&tmp, dest)?;
    Ok(true)
}

pub fn restore(root: &Path, kind: AgentKind, dest: &Path) -> Result<bool> {
    let src = dest.join(kind.name());
    if !src.is_dir() {
        return Ok(false);
    }
    copy_all(&src, root)?;
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    Live,
    Restored,
    Lost,
}

pub fn ensure(root: &Path, kind: AgentKind, session: &str, dest: &Path) -> Result<Resume> {
    if !live(root, kind, session).is_empty() {
        return Ok(Resume::Live);
    }
    if restore(root, kind, dest)? && !live(root, kind, session).is_empty() {
        return Ok(Resume::Restored);
    }
    Ok(Resume::Lost)
}

pub fn remove(dest: &Path) -> Result<()> {
    match fs::remove_dir_all(dest) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

pub fn rebuild_prompt(task: &str, pr: Option<&str>, log: &str, feedback: Option<&str>) -> String {
    let mut out = format!(
        "{task}\n\nYour earlier agent session for this bead was lost, so you are starting fresh \
         in the existing worktree. The work below is already committed on your branch."
    );
    if let Some(pr) = pr {
        out.push_str(&format!("\n\nPull request: {pr}"));
    }
    let log = log.trim();
    if log.is_empty() {
        out.push_str("\n\nNo commits yet on this branch.");
    } else {
        out.push_str(&format!("\n\nCommits so far (git log base..HEAD):\n{log}"));
    }
    if let Some(f) = feedback.map(str::trim).filter(|f| !f.is_empty()) {
        out.push_str(&format!("\n\nNew feedback to address:\n{f}"));
    }
    out
}

fn copy_all(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_all(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        fs::copy(src, dst)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Home(PathBuf);

    impl Home {
        fn new(tag: &str) -> Self {
            let d =
                std::env::temp_dir().join(format!("sulla-sessions-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            Self(d)
        }

        fn roots(&self) -> Roots {
            let home = self.0.join("home");
            Roots::from_vars(|k| (k == "HOME").then(|| home.clone().into_os_string()))
        }

        fn dest(&self) -> PathBuf {
            backup_dir(&self.0.join("repo"), "x-1")
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn roots_follow_home_and_overrides() {
        let r = Roots::from_vars(|k| match k {
            "HOME" => Some("/h".into()),
            "CLAUDE_CONFIG_DIR" => Some("/c".into()),
            _ => None,
        });
        assert_eq!(r.copilot, PathBuf::from("/h/.copilot"));
        assert_eq!(r.claude, PathBuf::from("/c"));
    }

    #[test]
    fn copilot_backup_and_restore_round_trip() {
        let h = Home::new("copilot");
        let root = h.roots().copilot;
        let dir = root.join("session-state").join("s-1");
        write(&dir.join("events.jsonl"), "one\n");
        write(&dir.join("files").join("plan.md"), "plan");
        write(
            &root.join("session-state").join("s-2").join("events.jsonl"),
            "other",
        );
        let dest = h.dest();
        assert!(backup(&root, AgentKind::Copilot, "s-1", &dest).unwrap());
        let saved = dest.join("copilot").join("session-state").join("s-1");
        assert_eq!(
            fs::read_to_string(saved.join("events.jsonl")).unwrap(),
            "one\n"
        );
        assert!(saved.join("files").join("plan.md").is_file());
        assert!(
            !dest
                .join("copilot")
                .join("session-state")
                .join("s-2")
                .exists()
        );

        write(&dir.join("events.jsonl"), "one\ntwo\n");
        assert!(backup(&root, AgentKind::Copilot, "s-1", &dest).unwrap());
        assert_eq!(
            fs::read_to_string(saved.join("events.jsonl")).unwrap(),
            "one\ntwo\n"
        );

        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            ensure(&root, AgentKind::Copilot, "s-1", &dest).unwrap(),
            Resume::Restored
        );
        assert_eq!(
            fs::read_to_string(dir.join("events.jsonl")).unwrap(),
            "one\ntwo\n"
        );
        assert_eq!(
            ensure(&root, AgentKind::Copilot, "s-1", &dest).unwrap(),
            Resume::Live
        );
        remove(&dest).unwrap();
        assert!(!dest.exists());
        remove(&dest).unwrap();
    }

    #[test]
    fn claude_backup_restores_into_project_dir() {
        let h = Home::new("claude");
        let root = h.roots().claude;
        let proj = root.join("projects").join("-repo-wt");
        write(&proj.join("s-1.jsonl"), "{}\n");
        write(&proj.join("s-1").join("sub.jsonl"), "sub");
        write(&proj.join("s-2.jsonl"), "other");
        let dest = h.dest();
        assert!(backup(&root, AgentKind::Claude, "s-1", &dest).unwrap());
        fs::remove_dir_all(root.join("projects")).unwrap();
        assert_eq!(
            ensure(&root, AgentKind::Claude, "s-1", &dest).unwrap(),
            Resume::Restored
        );
        assert_eq!(fs::read_to_string(proj.join("s-1.jsonl")).unwrap(), "{}\n");
        assert!(proj.join("s-1").join("sub.jsonl").is_file());
        assert!(!proj.join("s-2.jsonl").exists());
    }

    #[test]
    fn missing_session_and_backup_is_lost() {
        let h = Home::new("lost");
        let roots = h.roots();
        let dest = h.dest();
        assert!(!backup(&roots.copilot, AgentKind::Copilot, "s-1", &dest).unwrap());
        assert!(!dest.exists());
        for kind in AgentKind::ALL {
            assert_eq!(
                ensure(roots.get(kind), kind, "s-1", &dest).unwrap(),
                Resume::Lost
            );
        }
    }

    #[test]
    fn backup_of_other_agent_is_not_restored() {
        let h = Home::new("kind");
        let roots = h.roots();
        write(
            &roots
                .copilot
                .join("session-state")
                .join("s-1")
                .join("events.jsonl"),
            "x",
        );
        let dest = h.dest();
        assert!(backup(&roots.copilot, AgentKind::Copilot, "s-1", &dest).unwrap());
        assert_eq!(
            ensure(&roots.claude, AgentKind::Claude, "s-1", &dest).unwrap(),
            Resume::Lost
        );
    }

    #[test]
    fn rebuild_prompt_carries_context() {
        let p = rebuild_prompt(
            "You are working on bead x-1",
            Some("https://github.com/o/r/pull/3"),
            "abc123 Add thing\n",
            Some("PR #3 got new feedback"),
        );
        assert!(p.starts_with("You are working on bead x-1"));
        assert!(p.contains("https://github.com/o/r/pull/3"));
        assert!(p.contains("abc123 Add thing"));
        assert!(p.contains("PR #3 got new feedback"));
        let p = rebuild_prompt("task", None, "", None);
        assert!(p.contains("No commits yet"));
        assert!(!p.contains("Pull request"));
        assert!(!p.contains("New feedback"));
    }
}
