mod incus;

use std::collections::HashMap;
use std::path::Path;

use color_eyre::Result;
use portable_pty::CommandBuilder;

pub use incus::Incus;

use crate::agent::Launch;
use crate::config::{Config, SandboxKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Merged,
    Discarded,
    Killed,
    Exited,
}

impl Outcome {
    fn rank(self) -> u8 {
        match self {
            Outcome::Exited => 0,
            Outcome::Merged | Outcome::Killed => 1,
            Outcome::Discarded => 2,
        }
    }

    pub fn worst(self, other: Outcome) -> Outcome {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

pub trait Sandbox: Send {
    fn name(&self) -> &str;
    fn prepare(&mut self, slot: usize, launch: &Launch) -> Result<()>;
    fn wrap(&self, slot: usize, launch: &Launch, cmd: CommandBuilder) -> Result<CommandBuilder>;
    fn finished(&mut self, slot: usize, id: &str, outcome: Outcome) -> Result<()>;
    fn shutdown(&mut self);
    fn instance(&self, _slot: usize) -> Option<String> {
        None
    }
    fn address(&self, _slot: usize) -> Option<String> {
        None
    }
}

pub struct Worktree;

impl Sandbox for Worktree {
    fn name(&self) -> &str {
        "worktree"
    }

    fn prepare(&mut self, _slot: usize, _launch: &Launch) -> Result<()> {
        Ok(())
    }

    fn wrap(&self, _slot: usize, _launch: &Launch, cmd: CommandBuilder) -> Result<CommandBuilder> {
        Ok(cmd)
    }

    fn finished(&mut self, _slot: usize, _id: &str, _outcome: Outcome) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) {}
}

pub fn build(cfg: &Config, repo: &Path) -> Result<Box<dyn Sandbox>> {
    Ok(match cfg.sandbox {
        SandboxKind::Worktree => Box::new(Worktree),
        SandboxKind::Incus => Box::new(Incus::new(cfg.incus.clone(), repo.to_path_buf())?),
    })
}

#[derive(Default)]
pub struct Slots {
    taken: HashMap<String, usize>,
}

impl Slots {
    pub fn acquire(&mut self, id: &str, cap: usize) -> Option<usize> {
        if let Some(s) = self.taken.get(id) {
            return Some(*s);
        }
        let slot = (0..cap).find(|s| !self.taken.values().any(|t| t == s))?;
        self.taken.insert(id.to_string(), slot);
        Some(slot)
    }

    pub fn acquire_with(&mut self, id: &str, partner: &str, cap: usize) -> Option<usize> {
        if !self.taken.contains_key(id)
            && let Some(s) = self.get(partner)
        {
            self.taken.insert(id.to_string(), s);
            return Some(s);
        }
        self.acquire(id, cap)
    }

    pub fn held(&self, slot: usize) -> bool {
        self.taken.values().any(|t| *t == slot)
    }

    pub fn get(&self, id: &str) -> Option<usize> {
        self.taken.get(id).copied()
    }

    pub fn release(&mut self, id: &str) -> Option<usize> {
        self.taken.remove(id)
    }

    pub fn free(&self, cap: usize) -> usize {
        (0..cap)
            .filter(|s| !self.taken.values().any(|t| t == s))
            .count()
    }

    pub fn clear(&mut self) {
        self.taken.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_reuse_lowest_and_respect_cap() {
        let mut s = Slots::default();
        assert_eq!(s.acquire("a", 3), Some(0));
        assert_eq!(s.acquire("b", 3), Some(1));
        assert_eq!(s.acquire("c", 3), Some(2));
        assert_eq!(s.acquire("d", 3), None);
        assert_eq!(s.free(3), 0);
        assert_eq!(s.acquire("b", 3), Some(1));
        assert_eq!(s.release("b"), Some(1));
        assert_eq!(s.release("a"), Some(0));
        assert_eq!(s.free(3), 2);
        assert_eq!(s.acquire("d", 3), Some(0));
        assert_eq!(s.acquire("e", 3), Some(1));
        assert_eq!(s.acquire("f", 3), None);
        assert_eq!(s.release("zzz"), None);
    }

    #[test]
    fn partner_shares_slot_until_both_release() {
        let mut s = Slots::default();
        assert_eq!(s.acquire("a", 2), Some(0));
        assert_eq!(s.acquire("b", 2), Some(1));
        assert_eq!(s.acquire("a~review", 2), None);
        assert_eq!(s.acquire_with("a~review", "a", 2), Some(0));
        assert_eq!(s.free(2), 0);
        assert_eq!(s.release("a"), Some(0));
        assert!(s.held(0));
        assert_eq!(s.release("a~review"), Some(0));
        assert!(!s.held(0));
        assert_eq!(s.acquire_with("c", "missing", 2), Some(0));
    }

    #[test]
    fn worst_outcome_prefers_harsher_reset() {
        use Outcome::*;
        assert_eq!(Exited.worst(Killed), Killed);
        assert_eq!(Killed.worst(Exited), Killed);
        assert_eq!(Merged.worst(Discarded), Discarded);
        assert_eq!(Exited.worst(Exited), Exited);
    }

    #[test]
    fn worktree_wrap_is_identity() {
        let launch = Launch {
            id: "x-1".into(),
            session: "s".into(),
            bead: true,
            worktree: "/tmp/wt".into(),
            resume: false,
            prompt: None,
        };
        let mut cmd = CommandBuilder::new("copilot");
        cmd.args(["--resume", "s", "-i", "hi there"]);
        cmd.cwd("/tmp/wt");
        cmd.env("SULLA_SLOT", "2");
        let argv = cmd.get_argv().clone();
        let env: Vec<_> = cmd
            .iter_extra_env_as_str()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let out = Worktree.wrap(2, &launch, cmd).unwrap();
        assert_eq!(out.get_argv(), &argv);
        assert_eq!(
            out.get_cwd().map(|c| c.to_os_string()),
            Some("/tmp/wt".into())
        );
        let out_env: Vec<_> = out
            .iter_extra_env_as_str()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(out_env, env);
    }
}
