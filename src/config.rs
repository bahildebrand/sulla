use std::path::PathBuf;

use color_eyre::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub max_agents: usize,
    pub done_retention_days: i64,
    pub poll_secs: u64,
    pub base_branch: Option<String>,
    pub kache: bool,
    pub agent: AgentKind,
    pub copilot: CopilotConfig,
    pub claude: ClaudeConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Copilot,
    Claude,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    pub bin: String,
    pub model: Option<String>,
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CopilotConfig {
    pub bin: String,
    pub model: Option<String>,
    pub extra_args: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_agents: 3,
            done_retention_days: 3,
            poll_secs: 2,
            base_branch: None,
            kache: true,
            agent: AgentKind::default(),
            copilot: CopilotConfig::default(),
            claude: ClaudeConfig::default(),
        }
    }
}

impl Default for CopilotConfig {
    fn default() -> Self {
        Self {
            bin: "copilot".into(),
            model: None,
            extra_args: Vec::new(),
        }
    }
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            bin: "claude".into(),
            model: None,
            extra_args: Vec::new(),
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "sulla").map(|d| d.config_dir().join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        match Self::path() {
            Some(p) if p.exists() => Ok(toml::from_str(&std::fs::read_to_string(p)?)?),
            _ => Ok(Self::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_keeps_defaults() {
        let cfg: Config = toml::from_str("max_agents = 5\n[copilot]\nmodel = \"x\"").unwrap();
        assert_eq!(cfg.max_agents, 5);
        assert_eq!(cfg.done_retention_days, 3);
        assert_eq!(cfg.copilot.bin, "copilot");
        assert_eq!(cfg.copilot.model.as_deref(), Some("x"));
    }

    #[test]
    fn agent_selects_claude() {
        let cfg: Config = toml::from_str("agent = \"claude\"\n[claude]\nmodel = \"m\"").unwrap();
        assert_eq!(cfg.agent, AgentKind::Claude);
        assert_eq!(cfg.claude.bin, "claude");
        assert_eq!(Config::default().agent, AgentKind::Copilot);
    }
}
