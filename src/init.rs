use std::io::{BufRead, Write};
use std::path::Path;

use color_eyre::{Result, eyre::WrapErr};

use crate::beads::Beads;
use crate::config::{AgentKind, Config, SandboxKind, Workflow};
use crate::git::{self, Git};

pub struct Prompter<R, W> {
    input: R,
    output: W,
}

impl<R: BufRead, W: Write> Prompter<R, W> {
    pub fn new(input: R, output: W) -> Self {
        Self { input, output }
    }

    fn line(&mut self, label: &str, default: &str) -> Result<String> {
        if default.is_empty() {
            write!(self.output, "{label}: ")?;
        } else {
            write!(self.output, "{label} [{default}]: ")?;
        }
        self.output.flush()?;
        let mut buf = String::new();
        if self.input.read_line(&mut buf)? == 0 {
            writeln!(self.output)?;
        }
        Ok(buf.trim().to_string())
    }

    fn say(&mut self, msg: &str) -> Result<()> {
        writeln!(self.output, "{msg}")?;
        Ok(())
    }

    pub fn text(&mut self, label: &str, default: &str) -> Result<String> {
        let s = self.line(label, default)?;
        Ok(if s.is_empty() { default.to_string() } else { s })
    }

    pub fn optional(&mut self, label: &str, default: Option<&str>) -> Result<Option<String>> {
        let s = self.line(&format!("{label} (- for none)"), default.unwrap_or(""))?;
        Ok(match s.as_str() {
            "" => default.map(String::from),
            "-" => None,
            _ => Some(s),
        })
    }

    pub fn yes(&mut self, label: &str, default: bool) -> Result<bool> {
        loop {
            let s = self.line(label, if default { "Y/n" } else { "y/N" })?;
            match s.to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => self.say("  answer y or n")?,
            }
        }
    }

    pub fn number(&mut self, label: &str, default: usize) -> Result<usize> {
        loop {
            let s = self.line(label, &default.to_string())?;
            if s.is_empty() {
                return Ok(default);
            }
            match s.parse() {
                Ok(n) => return Ok(n),
                Err(_) => self.say("  enter a whole number")?,
            }
        }
    }

    pub fn choice(&mut self, label: &str, options: &[&str], default: &str) -> Result<String> {
        loop {
            let s = self.line(&format!("{label} ({})", options.join("/")), default)?;
            if s.is_empty() {
                return Ok(default.to_string());
            }
            if let Some(o) = options.iter().find(|o| o.eq_ignore_ascii_case(&s)) {
                return Ok(o.to_string());
            }
            self.say(&format!("  pick one of {}", options.join(", ")))?;
        }
    }
}

fn workflow_name(w: Workflow) -> &'static str {
    match w {
        Workflow::Local => "local",
        Workflow::Pr => "pr",
    }
}

fn sandbox_name(s: SandboxKind) -> &'static str {
    match s {
        SandboxKind::Worktree => "worktree",
        SandboxKind::Incus => "incus",
    }
}

struct Writer<'a> {
    table: toml::Table,
    inherited: &'a toml::Table,
}

impl Writer<'_> {
    fn set(&mut self, path: &[&str], value: Option<toml::Value>) {
        let (key, parents) = path.split_last().expect("non-empty path");
        let mut inherited = Some(self.inherited);
        for p in parents {
            inherited = inherited.and_then(|t| t.get(*p)).and_then(|v| v.as_table());
        }
        let same = inherited.and_then(|t| t.get(*key)) == value.as_ref();
        let mut table = &mut self.table;
        for p in parents {
            table = table
                .entry(p.to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                .as_table_mut()
                .expect("config section is a table");
        }
        match value {
            Some(v) if !same || table.contains_key(*key) => {
                table.insert(key.to_string(), v);
            }
            Some(_) => {}
            None => {
                table.remove(*key);
            }
        }
        if let Some((first, _)) = path.split_first()
            && parents.len() == 1
            && self
                .table
                .get(*first)
                .and_then(|v| v.as_table())
                .is_some_and(|t| t.is_empty())
        {
            self.table.remove(*first);
        }
    }
}

fn configure<R: BufRead, W: Write>(
    p: &mut Prompter<R, W>,
    cfg: &Config,
    current_branch: Option<&str>,
    existing: toml::Table,
    inherited: &toml::Table,
) -> Result<toml::Table> {
    let mut w = Writer {
        table: existing,
        inherited,
    };

    let names: Vec<&str> = AgentKind::ALL.iter().map(|k| k.name()).collect();
    let agent = p.choice("Agent", &names, cfg.agent.name())?;
    let agent = AgentKind::parse(&agent).unwrap_or(cfg.agent);
    w.set(&["agent"], Some(agent.name().into()));

    let model = match agent {
        AgentKind::Copilot => cfg.copilot.model.as_deref(),
        AgentKind::Claude => cfg.claude.model.as_deref(),
    };
    let model = p.optional(&format!("{} model", agent.name()), model)?;
    w.set(&[agent.name(), "model"], model.map(Into::into));

    let max = p.number("Max concurrent agents", cfg.max_agents)?;
    w.set(&["max_agents"], Some(toml::Value::Integer(max as i64)));

    let base = cfg.base_branch.as_deref().or(current_branch);
    let base = p.optional("Base branch", base)?;
    w.set(&["base_branch"], base.map(Into::into));

    let workflow = p.choice("Workflow", &["local", "pr"], workflow_name(cfg.workflow))?;
    w.set(&["workflow"], Some(workflow.as_str().into()));
    if workflow == "pr" {
        let remote = p.text("Pull request remote", &cfg.pr.remote)?;
        w.set(&["pr", "remote"], Some(remote.into()));
        let draft = p.yes("Open pull requests as drafts", cfg.pr.draft)?;
        w.set(&["pr", "draft"], Some(draft.into()));
    }

    let sandbox = p.choice("Sandbox", &["worktree", "incus"], sandbox_name(cfg.sandbox))?;
    w.set(&["sandbox"], Some(sandbox.as_str().into()));

    let kache_default = cfg.kache && sandbox != "incus";
    let kache = p.yes("Use kache as RUSTC_WRAPPER", kache_default)?;
    w.set(&["kache"], Some(kache.into()));

    let check = p.optional("Pre-merge check command", cfg.check.as_deref())?;
    w.set(&["check"], check.map(Into::into));

    Ok(w.table)
}

fn read_table(path: &Path) -> Result<toml::Table> {
    if !path.exists() {
        return Ok(toml::Table::new());
    }
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}

fn inherited_table() -> Result<toml::Table> {
    let mut t = toml::Table::try_from(Config::default())?;
    if let Some(path) = Config::path() {
        crate::config::merge(&mut t, read_table(&path)?);
    }
    Ok(t)
}

pub fn run(dir: &Path) -> Result<()> {
    let stdin = std::io::stdin();
    let mut p = Prompter::new(stdin.lock(), std::io::stdout());
    let repo = git::toplevel(dir).wrap_err("not a git repository, run `git init` first")?;
    Beads::new(&repo)
        .list_all()
        .wrap_err("bd is not usable here, run `bd init` first")?;
    Git::new(repo.clone()).ensure_ignored()?;
    p.say("Setting up sulla. Press enter to keep the value in brackets.")?;

    let cfg = Config::load(&repo)?;
    let path = Config::repo_path(&repo);
    let existing = read_table(&path)?;
    let branch = Git::new(repo.clone())
        .current_branch()
        .ok()
        .filter(|b| b != "HEAD");
    let table = configure(
        &mut p,
        &cfg,
        branch.as_deref(),
        existing,
        &inherited_table()?,
    )?;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string(&table)?)?;
    Config::load(&repo)?;
    p.say(&format!("wrote {}", path.display()))?;
    p.say("run `sulla` in the repo to start")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(input: &str, existing: &str) -> (toml::Table, String) {
        let mut out = Vec::new();
        let inherited = toml::Table::try_from(Config::default()).unwrap();
        let existing: toml::Table = toml::from_str(existing).unwrap();
        let cfg = Config::default();
        let table = {
            let mut p = Prompter::new(input.as_bytes(), &mut out);
            configure(&mut p, &cfg, Some("main"), existing, &inherited).unwrap()
        };
        (table, String::from_utf8(out).unwrap())
    }

    #[test]
    fn enter_keeps_defaults_and_only_pins_base() {
        let (t, out) = answers("", "");
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t["base_branch"].as_str(), Some("main"));
        assert!(out.contains("Agent (copilot/claude) [copilot]: "));
        assert!(out.contains("Max concurrent agents [3]: "));
    }

    #[test]
    fn answers_are_written_and_invalid_input_reprompts() {
        let input = "Claude\nopus\nlots\n5\n-\npr\nupstream\ny\nincus\n\ncargo test\n";
        let (t, out) = answers(input, "poll_secs = 9\n[claude]\nbin = \"/x\"");
        let cfg: Config = toml::Value::Table(t.clone()).try_into().unwrap();
        assert!(out.contains("enter a whole number"));
        assert_eq!(cfg.agent, AgentKind::Claude);
        assert_eq!(cfg.claude.model.as_deref(), Some("opus"));
        assert_eq!(cfg.claude.bin, "/x");
        assert_eq!(cfg.max_agents, 5);
        assert_eq!(cfg.base_branch, None);
        assert_eq!(cfg.workflow, Workflow::Pr);
        assert_eq!(cfg.pr.remote, "upstream");
        assert!(cfg.pr.draft);
        assert_eq!(cfg.sandbox, SandboxKind::Incus);
        assert!(!cfg.kache);
        assert_eq!(cfg.check.as_deref(), Some("cargo test"));
        assert_eq!(cfg.poll_secs, 9);
        assert!(!t.contains_key("base_branch"));
    }

    #[test]
    fn existing_repo_values_are_defaults_and_can_be_cleared() {
        let mut out = Vec::new();
        let inherited = toml::Table::try_from(Config::default()).unwrap();
        let existing: toml::Table =
            toml::from_str("max_agents = 3\ncheck = \"make\"\n[copilot]\nmodel = \"m\"").unwrap();
        let cfg: Config = toml::Value::Table(existing.clone()).try_into().unwrap();
        let mut p = Prompter::new("\n\n\n\n\n\n\n-\n".as_bytes(), &mut out);
        let t = configure(&mut p, &cfg, None, existing, &inherited).unwrap();
        assert_eq!(t["max_agents"].as_integer(), Some(3));
        assert_eq!(t["copilot"]["model"].as_str(), Some("m"));
        assert!(!t.contains_key("check"));
        assert!(!t.contains_key("base_branch"));
    }
}
