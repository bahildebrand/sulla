use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    pub sandbox: SandboxKind,
    pub copilot: CopilotConfig,
    pub claude: ClaudeConfig,
    pub sandbox: SandboxKind,
    pub incus: IncusConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxKind {
    #[default]
    Worktree,
    Incus,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct IncusConfig {
    pub bin: String,
    pub golden: String,
    pub snapshot: String,
    pub prefix: String,
    pub storage_pool: String,
    pub memory: String,
    pub cpus: Option<u32>,
    pub nofile: u64,
    pub reset_every: u32,
    pub reset_volumes: bool,
    pub mounts: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Default for IncusConfig {
    fn default() -> Self {
        Self {
            bin: "incus".into(),
            golden: "sulla-golden".into(),
            snapshot: "clean".into(),
            prefix: "sulla".into(),
            storage_pool: "default".into(),
            memory: "8GiB".into(),
            cpus: None,
            nofile: 262144,
            reset_every: 10,
            reset_volumes: true,
            mounts: vec!["~/.claude".into(), "~/.copilot".into()],
            env: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Copilot,
    Claude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxKind {
    #[default]
    Worktree,
    Incus,
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
            sandbox: SandboxKind::default(),
            copilot: CopilotConfig::default(),
            claude: ClaudeConfig::default(),
            sandbox: SandboxKind::default(),
            incus: IncusConfig::default(),
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

    pub fn repo_path(repo: &Path) -> PathBuf {
        repo.join(".sulla").join("config.toml")
    }

    pub fn load(repo: &Path) -> Result<Self> {
        Self::load_from(Self::path().as_deref(), &Self::repo_path(repo))
    }

    fn load_from(global: Option<&Path>, repo: &Path) -> Result<Self> {
        let mut merged = toml::Table::new();
        for path in global.into_iter().chain(std::iter::once(repo)) {
            if path.exists() {
                let table: toml::Table = toml::from_str(&std::fs::read_to_string(path)?)?;
                merge(&mut merged, table);
            }
        }
        Ok(toml::Value::Table(merged).try_into()?)
    }
}

fn merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
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

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sulla-cfg-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn repo_overrides_nested_key_and_keeps_siblings() {
        let d = tempdir("merge");
        let g = write(
            &d,
            "global.toml",
            "max_agents = 7\n[claude]\nbin = \"/opt/claude\"\nmodel = \"opus\"\nextra_args = [\"a\", \"b\"]",
        );
        let r = write(
            &d,
            "repo.toml",
            "sandbox = \"incus\"\n[claude]\nmodel = \"sonnet\"\nextra_args = [\"c\"]",
        );
        let cfg = Config::load_from(Some(&g), &r).unwrap();
        assert_eq!(cfg.max_agents, 7);
        assert_eq!(cfg.sandbox, SandboxKind::Incus);
        assert_eq!(cfg.claude.bin, "/opt/claude");
        assert_eq!(cfg.claude.model.as_deref(), Some("sonnet"));
        assert_eq!(cfg.claude.extra_args, vec!["c".to_string()]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn missing_repo_file_uses_global() {
        let d = tempdir("missing");
        let g = write(&d, "global.toml", "max_agents = 9\n[incus]\nmemory = \"4GiB\"");
        let cfg = Config::load_from(Some(&g), &d.join("nope.toml")).unwrap();
        assert_eq!(cfg.max_agents, 9);
        assert_eq!(cfg.incus.memory, "4GiB");
        assert_eq!(cfg.incus.golden, "sulla-golden");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn sandbox_defaults_to_worktree_and_parses_incus() {
        assert_eq!(Config::default().sandbox, SandboxKind::Worktree);
        let cfg: Config = toml::from_str("sandbox = \"incus\"").unwrap();
        assert_eq!(cfg.sandbox, SandboxKind::Incus);
    }
}
