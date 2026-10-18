use std::path::PathBuf;

use portable_pty::CommandBuilder;

use super::{Backend, Launch, session_id, task_prompt};
use crate::config::CopilotConfig;

pub struct Copilot {
    pub cfg: CopilotConfig,
    pub repo: PathBuf,
}

impl Backend for Copilot {
    fn name(&self) -> &str {
        "copilot"
    }

    fn command(&self, launch: &Launch) -> CommandBuilder {
        let mut cmd = CommandBuilder::new(&self.cfg.bin);
        cmd.arg("--yolo");
        if let Some(m) = &self.cfg.model {
            cmd.args(["--model", m]);
        }
        cmd.args(&self.cfg.extra_args);
        cmd.args(["--session-id", &session_id(&self.repo, &launch.bead.id)]);
        let prompt = match (&launch.prompt, launch.resume) {
            (Some(p), _) => Some(p.clone()),
            (None, true) => None,
            (None, false) => Some(task_prompt(&launch.bead)),
        };
        if let Some(p) = prompt {
            cmd.args(["-i", &p]);
        }
        cmd
    }
}
