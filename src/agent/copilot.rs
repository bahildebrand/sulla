use portable_pty::CommandBuilder;

use super::{Backend, Launch};
use crate::config::CopilotConfig;

pub struct Copilot {
    pub cfg: CopilotConfig,
}

impl Backend for Copilot {
    fn name(&self) -> &str {
        "copilot"
    }

    fn command(&self, launch: &Launch) -> CommandBuilder {
        let mut cmd = CommandBuilder::new(&self.cfg.bin);
        cmd.arg("--yolo");
        if let Some(m) = launch.model.as_ref().or(self.cfg.model.as_ref()) {
            cmd.args(["--model", m]);
        }
        cmd.args(&self.cfg.extra_args);
        cmd.args(["--session-id", &launch.session]);
        if let Some(p) = &launch.prompt {
            cmd.args(["-i", p]);
        }
        cmd
    }
}
