use std::path::PathBuf;

use portable_pty::CommandBuilder;

use super::{Backend, Launch, session_id, task_prompt};
use crate::config::ClaudeConfig;

pub struct Claude {
    pub cfg: ClaudeConfig,
    pub repo: PathBuf,
}

impl Backend for Claude {
    fn name(&self) -> &str {
        "claude"
    }

    fn command(&self, launch: &Launch) -> CommandBuilder {
        let mut cmd = CommandBuilder::new(&self.cfg.bin);
        cmd.arg("--dangerously-skip-permissions");
        if let Some(m) = &self.cfg.model {
            cmd.args(["--model", m]);
        }
        cmd.args(&self.cfg.extra_args);
        let id = session_id(&self.repo, &launch.bead.id);
        let flag = if launch.resume {
            "--resume"
        } else {
            "--session-id"
        };
        cmd.args([flag, &id]);
        let prompt = match (&launch.prompt, launch.resume) {
            (Some(p), _) => Some(p.clone()),
            (None, true) => None,
            (None, false) => Some(task_prompt(&launch.bead)),
        };
        if let Some(p) = prompt {
            cmd.arg(p);
        }
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beads::Bead;

    fn launch(resume: bool, prompt: Option<&str>) -> Launch {
        let bead: Bead =
            serde_json::from_str(r#"{"id":"x-1","title":"t","status":"open"}"#).unwrap();
        Launch {
            bead,
            worktree: PathBuf::from("/w"),
            resume,
            prompt: prompt.map(String::from),
        }
    }

    fn args(cmd: &CommandBuilder) -> Vec<String> {
        cmd.get_argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn backend() -> Claude {
        Claude {
            cfg: ClaudeConfig::default(),
            repo: PathBuf::from("/r"),
        }
    }

    #[test]
    fn fresh_launch_sets_session_and_prompt() {
        let a = args(&backend().command(&launch(false, None)));
        assert_eq!(a[0], "claude");
        assert!(a.contains(&"--session-id".to_string()));
        assert!(a.last().unwrap().contains("bead x-1"));
    }

    #[test]
    fn resume_without_prompt_has_no_prompt_arg() {
        let a = args(&backend().command(&launch(true, None)));
        assert_eq!(a[a.len() - 2], "--resume");
        assert_eq!(a.len(), 4);
    }

    #[test]
    fn feedback_prompt_overrides_task_prompt() {
        let a = args(&backend().command(&launch(true, Some("fix it"))));
        assert_eq!(a.last().unwrap(), "fix it");
    }
}
