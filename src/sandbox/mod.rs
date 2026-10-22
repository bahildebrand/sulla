mod incus;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::Result;
use portable_pty::CommandBuilder;

pub use incus::Incus;

use crate::agent::Launch;
use crate::config::{AgentKind, Config, SandboxKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Merged,
    Discarded,
    Killed,
    Evicted,
    Exited,
}

impl Outcome {
    fn rank(self) -> u8 {
        match self {
            Outcome::Exited => 0,
            Outcome::Merged | Outcome::Killed | Outcome::Evicted => 1,
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

pub type Prepare = Box<dyn FnOnce() -> Result<Vec<String>> + Send>;

pub trait Sandbox: Send {
    fn name(&self) -> &str;
    fn prepare(&mut self, slot: usize, id: &str) -> Prepare;
    fn wrap(&self, slot: usize, launch: &Launch, cmd: CommandBuilder) -> Result<CommandBuilder>;
    fn finished(&mut self, slot: usize, id: &str, outcome: Outcome) -> Result<()>;
    fn shutdown(&mut self);
    fn instance(&self, _slot: usize) -> Option<String> {
        None
    }
    fn address(&self, _slot: usize) -> Option<String> {
        None
    }
    fn isolated(&self) -> bool {
        false
    }
    fn session_root(&self, _kind: AgentKind, host: &Path) -> Option<PathBuf> {
        Some(host.to_path_buf())
    }
    fn command(
        &self,
        _slot: usize,
        cwd: &Path,
        argv: &[OsString],
        env: &[(String, String)],
    ) -> Command {
        host_command(cwd, argv, env)
    }
    fn shell(&self, _slot: usize, cwd: &Path, env: &[(String, String)]) -> Command {
        host_shell(cwd, env)
    }
}

pub fn host_command(cwd: &Path, argv: &[OsString], env: &[(String, String)]) -> Command {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..])
        .current_dir(cwd)
        .envs(env.iter().cloned());
    c
}

pub fn host_shell(cwd: &Path, env: &[(String, String)]) -> Command {
    let shell = std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sh".into());
    host_command(cwd, &[shell], env)
}

pub struct Worktree;

impl Sandbox for Worktree {
    fn name(&self) -> &str {
        "worktree"
    }

    fn prepare(&mut self, _slot: usize, _id: &str) -> Prepare {
        Box::new(|| Ok(Vec::new()))
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

    pub fn holders(&self, slot: usize) -> usize {
        self.taken.values().filter(|t| **t == slot).count()
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
        assert_eq!(s.holders(0), 2);
        assert_eq!(s.holders(1), 1);
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
        assert_eq!(Evicted.worst(Exited), Evicted);
        assert_eq!(Evicted.worst(Discarded), Discarded);
        assert_eq!(Exited.worst(Exited), Exited);
    }

    #[test]
    fn host_shell_runs_in_worktree_with_env() {
        let env = [("SULLA_BEAD".to_string(), "x-1".to_string())];
        let c = Worktree.shell(0, Path::new("/tmp/wt"), &env);
        let shell = std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "sh".into());
        assert_eq!(c.get_program(), shell);
        assert_eq!(c.get_args().count(), 0);
        assert_eq!(c.get_current_dir(), Some(Path::new("/tmp/wt")));
        let envs: Vec<_> = c.get_envs().collect();
        assert_eq!(
            envs,
            [(
                std::ffi::OsStr::new("SULLA_BEAD"),
                Some(std::ffi::OsStr::new("x-1"))
            )]
        );
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
            model: None,
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
