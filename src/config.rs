pub mod edit;
pub mod effective;
pub mod reload;
pub mod show;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use color_eyre::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub max_agents: usize,
    pub done_retention_days: i64,
    pub poll_secs: u64,
    pub base_branch: Option<String>,
    pub branch_prefix: String,
    pub kache: bool,
    pub check: Option<String>,
    pub system_prompt: Option<String>,
    pub agent: AgentKind,
    pub sandbox: SandboxKind,
    pub copilot: CopilotConfig,
    pub claude: ClaudeConfig,
    pub incus: IncusConfig,
    pub notify: NotifyConfig,
    pub stall: StallConfig,
    pub budget: BudgetConfig,
    pub workflow: Workflow,
    pub pr: PrConfig,
    pub hooks: HooksConfig,
    pub ai_review: AiReviewConfig,
    pub presets: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiReviewConfig {
    pub auto: bool,
    pub max_rounds: u32,
}

impl Default for AiReviewConfig {
    fn default() -> Self {
        Self {
            auto: false,
            max_rounds: 3,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    pub setup: Option<String>,
}

impl HooksConfig {
    pub fn setup_script(&self) -> Option<&str> {
        self.setup.as_deref().filter(|s| !s.trim().is_empty())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub minutes: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Workflow {
    #[default]
    Local,
    Pr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrTool {
    #[default]
    Gh,
    Graphite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrConfig {
    pub tool: PrTool,
    pub remote: String,
    pub draft: bool,
    pub poll_secs: u64,
    pub backup_sessions: bool,
    pub reply: bool,
}

impl Default for PrConfig {
    fn default() -> Self {
        Self {
            tool: PrTool::Gh,
            remote: "origin".into(),
            draft: false,
            poll_secs: 120,
            backup_sessions: true,
            reply: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StallConfig {
    pub minutes: u64,
    pub prompts: Vec<String>,
}

impl Default for StallConfig {
    fn default() -> Self {
        Self {
            minutes: 5,
            prompts: vec!["Do you want to".into(), "No, and tell".into()],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyMethod {
    Bell,
    Desktop,
    Tmux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WhenPresent {
    #[default]
    Quiet,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    Review,
    Waiting,
    Dead,
    Stalled,
    Budget,
    Pr,
    Chat,
}

fn triggers<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Trigger>, D::Error> {
    let mut on = Vec::<Trigger>::deserialize(d)?;
    if on.contains(&Trigger::Stalled) && !on.contains(&Trigger::Dead) {
        on.push(Trigger::Dead);
    }
    Ok(on)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub methods: Vec<NotifyMethod>,
    #[serde(deserialize_with = "triggers")]
    pub on: Vec<Trigger>,
    pub cooldown_secs: u64,
    pub idle_secs: u64,
    pub when_present: WhenPresent,
    pub batch_secs: u64,
    pub notify_send: String,
    pub tmux_option: String,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            methods: vec![NotifyMethod::Bell],
            on: vec![
                Trigger::Review,
                Trigger::Waiting,
                Trigger::Dead,
                Trigger::Budget,
                Trigger::Pr,
            ],
            cooldown_secs: 60,
            idle_secs: 120,
            when_present: WhenPresent::Quiet,
            batch_secs: 5,
            notify_send: "notify-send".into(),
            tmux_option: "@sulla".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxKind {
    #[default]
    Worktree,
    Incus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub boot_timeout_secs: u64,
    pub mounts: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub init: Option<String>,
    pub agent: IncusAgent,
    pub browse: BrowseConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IncusAgent {
    #[default]
    Host,
    Container,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowseConfig {
    pub ports: Vec<u16>,
    pub hosts: Vec<String>,
    pub url: Option<String>,
    pub up: Option<String>,
    pub down: Option<String>,
    pub ready: Option<String>,
    pub ready_timeout_secs: u64,
    pub keep_minutes: u64,
}

impl Default for BrowseConfig {
    fn default() -> Self {
        Self {
            ports: Vec::new(),
            hosts: Vec::new(),
            url: None,
            up: None,
            down: None,
            ready: None,
            ready_timeout_secs: 300,
            keep_minutes: 30,
        }
    }
}

fn script(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|s| !s.trim().is_empty())
}

impl BrowseConfig {
    pub fn up_script(&self) -> Option<&str> {
        script(&self.up)
    }

    pub fn down_script(&self) -> Option<&str> {
        script(&self.down)
    }

    pub fn ready_script(&self) -> Option<&str> {
        script(&self.ready)
    }

    pub fn keep(&self) -> Duration {
        Duration::from_secs(self.keep_minutes.saturating_mul(60))
    }
}

impl IncusConfig {
    pub fn boot_timeout(&self) -> Duration {
        Duration::from_secs(self.boot_timeout_secs.max(1))
    }
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
            boot_timeout_secs: 60,
            mounts: vec!["~/.claude".into(), "~/.copilot".into()],
            env: BTreeMap::new(),
            init: None,
            agent: IncusAgent::default(),
            browse: BrowseConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Copilot,
    Claude,
}

impl AgentKind {
    pub const ALL: [AgentKind; 2] = [AgentKind::Copilot, AgentKind::Claude];

    pub fn name(self) -> &'static str {
        match self {
            AgentKind::Copilot => "copilot",
            AgentKind::Claude => "claude",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(s.trim()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    pub bin: String,
    pub model: Option<String>,
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
            branch_prefix: crate::git::DEFAULT_PREFIX.into(),
            kache: true,
            check: None,
            system_prompt: None,
            agent: AgentKind::default(),
            sandbox: SandboxKind::default(),
            copilot: CopilotConfig::default(),
            claude: ClaudeConfig::default(),
            incus: IncusConfig::default(),
            notify: NotifyConfig::default(),
            stall: StallConfig::default(),
            budget: BudgetConfig::default(),
            workflow: Workflow::default(),
            pr: PrConfig::default(),
            hooks: HooksConfig::default(),
            ai_review: AiReviewConfig::default(),
            presets: BTreeMap::new(),
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
        effective::Layers::load(repo, toml::Table::new())?.config()
    }

    #[cfg(test)]
    fn load_from(global: Option<&Path>, repo: &Path) -> Result<Self> {
        effective::Layers::load_from(global, repo, toml::Table::new())?.config()
    }
}

pub fn merge(base: &mut toml::Table, over: toml::Table) {
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
        assert_eq!(cfg.branch_prefix, "sulla/");
        let cfg: Config = toml::from_str("branch_prefix = \"blake/\"").unwrap();
        assert_eq!(cfg.branch_prefix, "blake/");
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
        let g = write(
            &d,
            "global.toml",
            "max_agents = 9\n[incus]\nmemory = \"4GiB\"",
        );
        let cfg = Config::load_from(Some(&g), &d.join("nope.toml")).unwrap();
        assert_eq!(cfg.max_agents, 9);
        assert_eq!(cfg.incus.memory, "4GiB");
        assert_eq!(cfg.incus.golden, "sulla-golden");
        assert_eq!(cfg.incus.boot_timeout_secs, 60);
        assert_eq!(cfg.incus.boot_timeout(), Duration::from_secs(60));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn system_prompt_is_optional_and_repo_overrides_global() {
        assert!(Config::default().system_prompt.is_none());
        let d = tempdir("system");
        let g = write(&d, "global.toml", "system_prompt = \"global rules\"");
        let r = write(
            &d,
            "repo.toml",
            "system_prompt = \"\"\"\nrepo rules\n\"\"\"",
        );
        let cfg = Config::load_from(Some(&g), &d.join("nope.toml")).unwrap();
        assert_eq!(cfg.system_prompt.as_deref(), Some("global rules"));
        let cfg = Config::load_from(Some(&g), &r).unwrap();
        assert_eq!(cfg.system_prompt.as_deref(), Some("repo rules\n"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn notify_parses_methods_and_triggers() {
        let cfg: Config =
            toml::from_str("[notify]\nmethods = [\"desktop\", \"tmux\"]\non = [\"review\"]")
                .unwrap();
        assert_eq!(
            cfg.notify.methods,
            [NotifyMethod::Desktop, NotifyMethod::Tmux]
        );
        assert_eq!(cfg.notify.on, [Trigger::Review]);
        assert_eq!(cfg.notify.tmux_option, "@sulla");
        assert_eq!(cfg.notify.when_present, WhenPresent::Quiet);
        assert_eq!((cfg.notify.idle_secs, cfg.notify.batch_secs), (120, 5));
        let cfg: Config =
            toml::from_str("[notify]\nwhen_present = \"always\"\nidle_secs = 30\nbatch_secs = 0")
                .unwrap();
        assert_eq!(cfg.notify.when_present, WhenPresent::Always);
        assert_eq!((cfg.notify.idle_secs, cfg.notify.batch_secs), (30, 0));
    }

    #[test]
    fn notify_defaults_to_dead_but_not_stalled_or_chat() {
        let on = NotifyConfig::default().on;
        assert!(on.contains(&Trigger::Dead));
        assert!(!on.contains(&Trigger::Stalled));
        assert!(!on.contains(&Trigger::Chat));
        let cfg: Config = toml::from_str("[notify]\non = [\"dead\", \"chat\"]").unwrap();
        assert_eq!(cfg.notify.on, [Trigger::Dead, Trigger::Chat]);
    }

    #[test]
    fn old_stalled_trigger_means_dead_and_stalled() {
        let cfg: Config = toml::from_str("[notify]\non = [\"review\", \"stalled\"]").unwrap();
        assert_eq!(
            cfg.notify.on,
            [Trigger::Review, Trigger::Stalled, Trigger::Dead]
        );
    }

    #[test]
    fn sandbox_defaults_to_worktree_and_parses_incus() {
        assert_eq!(Config::default().sandbox, SandboxKind::Worktree);
        let cfg: Config = toml::from_str("sandbox = \"incus\"").unwrap();
        assert_eq!(cfg.sandbox, SandboxKind::Incus);
    }

    #[test]
    fn incus_agent_defaults_to_host_and_parses_both() {
        assert_eq!(Config::default().incus.agent, IncusAgent::Host);
        let cfg: Config = toml::from_str("[incus]\nmemory = \"4GiB\"").unwrap();
        assert_eq!(cfg.incus.agent, IncusAgent::Host);
        let cfg: Config = toml::from_str("[incus]\nagent = \"host\"").unwrap();
        assert_eq!(cfg.incus.agent, IncusAgent::Host);
        let cfg: Config = toml::from_str("[incus]\nagent = \"container\"").unwrap();
        assert_eq!(cfg.incus.agent, IncusAgent::Container);
        assert!(toml::from_str::<Config>("[incus]\nagent = \"vm\"").is_err());
    }

    #[test]
    fn setup_hook_is_optional_and_ignores_blank() {
        assert_eq!(Config::default().hooks.setup_script(), None);
        let cfg: Config = toml::from_str("[hooks]\nsetup = \"  \"").unwrap();
        assert_eq!(cfg.hooks.setup_script(), None);
        let cfg: Config = toml::from_str("[hooks]\nsetup = \"bin/setup\"").unwrap();
        assert_eq!(cfg.hooks.setup_script(), Some("bin/setup"));
    }

    #[test]
    fn ai_review_is_manual_by_default_and_parses_auto() {
        assert_eq!(
            Config::default().ai_review,
            AiReviewConfig {
                auto: false,
                max_rounds: 3,
            }
        );
        let cfg: Config = toml::from_str("[ai_review]\nauto = true").unwrap();
        assert!(cfg.ai_review.auto);
        assert_eq!(cfg.ai_review.max_rounds, 3);
        let cfg: Config = toml::from_str("[ai_review]\nauto = true\nmax_rounds = 1").unwrap();
        assert_eq!(cfg.ai_review.max_rounds, 1);
    }

    #[test]
    fn incus_browse_parses_nested_table() {
        assert_eq!(Config::default().incus.browse, BrowseConfig::default());
        let cfg: Config = toml::from_str(
            "[incus.browse]\nports = [8000, 8001]\nhosts = [\"*.memfault.test\"]\nurl = \"http://app.memfault.test:8000\"",
        )
        .unwrap();
        assert_eq!(
            cfg.incus.browse,
            BrowseConfig {
                ports: vec![8000, 8001],
                hosts: vec!["*.memfault.test".into()],
                url: Some("http://app.memfault.test:8000".into()),
                ..BrowseConfig::default()
            }
        );
        assert_eq!(cfg.incus.browse.up_script(), None);
        assert_eq!(cfg.incus.browse.down_script(), None);
        assert_eq!(cfg.incus.browse.ready_script(), None);
        assert_eq!(cfg.incus.browse.ready_timeout_secs, 300);
        assert_eq!(cfg.incus.golden, "sulla-golden");
    }

    #[test]
    fn incus_browse_parses_app_lifecycle() {
        let cfg: Config = toml::from_str(
            "[incus.browse]\nports = [8000]\nup = \"docker compose up -d\"\ndown = \"docker compose down\"\nready = \"curl -fsS http://127.0.0.1:8000/health\"\nready_timeout_secs = 60",
        )
        .unwrap();
        let b = &cfg.incus.browse;
        assert_eq!(b.up_script(), Some("docker compose up -d"));
        assert_eq!(b.down_script(), Some("docker compose down"));
        assert_eq!(
            b.ready_script(),
            Some("curl -fsS http://127.0.0.1:8000/health")
        );
        assert_eq!(b.ready_timeout_secs, 60);
        let cfg: Config = toml::from_str("[incus.browse]\nup = \"  \"").unwrap();
        assert_eq!(cfg.incus.browse.up_script(), None);
        assert_eq!(cfg.incus.browse.ready_timeout_secs, 300);
        assert_eq!(cfg.incus.browse.keep(), Duration::from_secs(30 * 60));
        let cfg: Config = toml::from_str("[incus.browse]\nkeep_minutes = 0").unwrap();
        assert_eq!(cfg.incus.browse.keep(), Duration::ZERO);
    }

    #[test]
    fn workflow_defaults_to_local() {
        let cfg: Config = toml::from_str("max_agents = 2").unwrap();
        assert_eq!(cfg.workflow, Workflow::Local);
        assert_eq!(cfg.pr, PrConfig::default());
        assert_eq!(cfg.pr.tool, PrTool::Gh);
        assert_eq!(cfg.pr.remote, "origin");
        assert!(!cfg.pr.draft);
        assert_eq!(cfg.pr.poll_secs, 120);
        assert!(cfg.pr.backup_sessions);
        assert!(!cfg.pr.reply);
    }

    #[test]
    fn workflow_pr_parses_pr_table() {
        let cfg: Config = toml::from_str(
            "workflow = \"pr\"\n[pr]\ntool = \"graphite\"\nremote = \"upstream\"\ndraft = true\npoll_secs = 30\nbackup_sessions = false\nreply = true",
        )
        .unwrap();
        assert_eq!(cfg.workflow, Workflow::Pr);
        assert_eq!(
            cfg.pr,
            PrConfig {
                tool: PrTool::Graphite,
                remote: "upstream".into(),
                draft: true,
                poll_secs: 30,
                backup_sessions: false,
                reply: true,
            }
        );
    }
}
