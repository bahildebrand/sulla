use std::collections::{BTreeSet, HashMap, HashSet};

use color_eyre::Result;

use crate::git::Git;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Conflict {
    pub base: Vec<String>,
    pub others: Vec<(String, Vec<String>)>,
}

impl Conflict {
    pub fn files(&self) -> BTreeSet<&str> {
        self.base
            .iter()
            .chain(self.others.iter().flat_map(|(_, f)| f))
            .map(String::as_str)
            .collect()
    }

    pub fn summary(&self, base: &str) -> String {
        let mut with: Vec<&str> = Vec::new();
        if !self.base.is_empty() {
            with.push(base);
        }
        with.extend(self.others.iter().map(|(id, _)| id.as_str()));
        format!("conflicts with {}", with.join(", "))
    }
}

#[derive(Default)]
pub struct Scanner {
    cache: HashMap<(String, String), Vec<String>>,
}

impl Scanner {
    pub fn scan(
        &mut self,
        git: &Git,
        base: &str,
        open: &HashSet<String>,
    ) -> Result<HashMap<String, Conflict>> {
        let base_sha = git.rev(base)?;
        let mut branches: Vec<(String, String)> = git
            .bead_branches()?
            .into_iter()
            .filter(|(id, _)| open.contains(id))
            .collect();
        branches.sort();
        let mut cache = HashMap::new();
        let mut check = |a: &str, b: &str| -> Vec<String> {
            let key = if a <= b {
                (a.to_string(), b.to_string())
            } else {
                (b.to_string(), a.to_string())
            };
            let files = match self.cache.get(&key) {
                Some(f) => f.clone(),
                None => git.merge_conflicts(a, b).unwrap_or_else(|e| {
                    tracing::warn!("{e:#}");
                    Vec::new()
                }),
            };
            cache.insert(key, files.clone());
            files
        };
        let mut out: HashMap<String, Conflict> = HashMap::new();
        for (i, (id, sha)) in branches.iter().enumerate() {
            if *sha != base_sha {
                let files = check(&base_sha, sha);
                if !files.is_empty() {
                    out.entry(id.clone()).or_default().base = files;
                }
            }
            for (other, other_sha) in &branches[i + 1..] {
                if sha == other_sha {
                    continue;
                }
                let files = check(sha, other_sha);
                if files.is_empty() {
                    continue;
                }
                out.entry(id.clone())
                    .or_default()
                    .others
                    .push((other.clone(), files.clone()));
                out.entry(other.clone())
                    .or_default()
                    .others
                    .push((id.clone(), files));
            }
        }
        self.cache = cache;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn commit(dir: &Path, file: &str, text: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", file]);
        git(dir, &["commit", "-qm", file]);
    }

    #[test]
    fn flags_base_and_cross_branch_conflicts_for_open_beads() {
        let dir = std::env::temp_dir().join(format!("sulla-conflicts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@t"]);
        git(&dir, &["config", "user.name", "t"]);
        commit(&dir, "a.txt", "a\n");
        commit(&dir, "b.txt", "b\n");
        let g = Git::new(dir.clone());
        g.ensure_ignored().unwrap();
        let wt = |id: &str, file: &str, text: &str| {
            let p = g.ensure_worktree(id, "main").unwrap();
            commit(&p, file, text);
        };
        wt("x-1", "a.txt", "one\n");
        wt("x-2", "b.txt", "two\n");
        wt("x-3", "b.txt", "three\n");
        wt("x-4", "c.txt", "clean\n");
        wt("x-5", "b.txt", "closed\n");
        commit(&dir, "a.txt", "main\n");

        let open: HashSet<String> = ["x-1", "x-2", "x-3", "x-4"]
            .into_iter()
            .map(String::from)
            .collect();
        let mut s = Scanner::default();
        let c = s.scan(&g, "main", &open).unwrap();
        assert_eq!(c["x-1"].base, ["a.txt"]);
        assert!(c["x-1"].others.is_empty());
        assert!(c["x-2"].base.is_empty());
        assert_eq!(
            c["x-2"].others,
            [("x-3".to_string(), vec!["b.txt".to_string()])]
        );
        assert_eq!(
            c["x-3"].others,
            [("x-2".to_string(), vec!["b.txt".to_string()])]
        );
        assert_eq!(c["x-1"].summary("main"), "conflicts with main");
        assert!(!c.contains_key("x-4"));
        assert_eq!(s.cache.len(), 10);
        assert_eq!(s.scan(&g, "main", &open).unwrap(), c);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
