use portable_pty::CommandBuilder;

use super::{Backend, Launch};
use crate::config::ClaudeConfig;

pub struct Claude {
    pub cfg: ClaudeConfig,
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
        let flag = if launch.resume {
            "--resume"
        } else {
            "--session-id"
        };
        cmd.args([flag, &launch.session]);
        if let Some(p) = &launch.prompt {
            cmd.arg(p);
        }
        cmd
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn launch(resume: bool, prompt: Option<&str>) -> Launch {
        Launch {
            id: "x-1".into(),
            session: "s-1".into(),
            bead: true,
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
        }
    }

    #[test]
    fn fresh_launch_sets_session_and_prompt() {
        let a = args(&backend().command(&launch(false, Some("do it"))));
        assert_eq!(a[0], "claude");
        assert!(a.contains(&"--session-id".to_string()));
        assert_eq!(a.last().unwrap(), "do it");
    }

    #[test]
    fn resume_without_prompt_has_no_prompt_arg() {
        let a = args(&backend().command(&launch(true, None)));
        assert_eq!(a[a.len() - 2], "--resume");
        assert_eq!(a.len(), 4);
    }

    #[test]
    fn resume_passes_feedback_prompt() {
        let a = args(&backend().command(&launch(true, Some("fix it"))));
        assert_eq!(a[a.len() - 3], "--resume");
        assert_eq!(a.last().unwrap(), "fix it");
    }
}
