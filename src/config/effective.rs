use std::path::{Path, PathBuf};

use color_eyre::{Result, eyre::WrapErr};

use super::{Config, merge};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    Default,
    Global,
    Repo,
    Flag,
    Runtime,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Global => "global",
            Source::Repo => "repo",
            Source::Flag => "flag",
            Source::Runtime => "runtime",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Layer {
    pub path: PathBuf,
    pub table: Option<toml::Table>,
}

impl Layer {
    pub fn read(path: &Path) -> Result<Self> {
        let at = || path.display().to_string();
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path: path.to_path_buf(),
                    table: None,
                });
            }
            Err(e) => return Err(e).wrap_err_with(at),
        };
        let table: toml::Table = toml::from_str(&text).wrap_err_with(at)?;
        toml::from_str::<Config>(&text).wrap_err_with(at)?;
        Ok(Self {
            path: path.to_path_buf(),
            table: Some(table),
        })
    }

    pub fn exists(&self) -> bool {
        self.table.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct Layers {
    pub global: Option<Layer>,
    pub repo: Layer,
    pub flags: toml::Table,
    pub runtime: toml::Table,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: Vec<String>,
    pub value: toml::Value,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Unknown {
    pub path: Vec<String>,
    pub value: toml::Value,
    pub source: Source,
    pub file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Effective {
    pub entries: Vec<Entry>,
    pub unknown: Vec<Unknown>,
}

impl Layers {
    pub fn load(repo: &Path, flags: toml::Table) -> Result<Self> {
        Self::load_from(Config::path().as_deref(), &Config::repo_path(repo), flags)
    }

    pub fn load_from(global: Option<&Path>, repo: &Path, flags: toml::Table) -> Result<Self> {
        Ok(Self {
            global: global.map(Layer::read).transpose()?,
            repo: Layer::read(repo)?,
            flags,
            runtime: toml::Table::new(),
        })
    }

    pub fn files(&self) -> impl Iterator<Item = (Source, &Layer)> {
        self.global
            .iter()
            .map(|l| (Source::Global, l))
            .chain(std::iter::once((Source::Repo, &self.repo)))
    }

    fn tables(&self) -> impl DoubleEndedIterator<Item = (Source, &toml::Table)> {
        let global = self.global.as_ref().and_then(|l| l.table.as_ref());
        let repo = self.repo.table.as_ref();
        [
            (Source::Global, global),
            (Source::Repo, repo),
            (Source::Flag, Some(&self.flags)),
            (Source::Runtime, Some(&self.runtime)),
        ]
        .into_iter()
        .filter_map(|(s, t)| t.map(|t| (s, t)))
    }

    pub fn merged(&self) -> toml::Table {
        let mut merged = toml::Table::new();
        for (_, table) in self.tables() {
            merge(&mut merged, table.clone());
        }
        merged
    }

    pub fn config(&self) -> Result<Config> {
        Ok(toml::Value::Table(self.merged()).try_into()?)
    }

    pub fn source(&self, path: &[String]) -> Source {
        self.tables()
            .rev()
            .find(|(_, t)| lookup(t, path).is_some())
            .map_or(Source::Default, |(s, _)| s)
    }

    pub fn effective(&self) -> Result<Effective> {
        let full = toml::Table::try_from(self.config()?)?;
        let mut entries = Vec::new();
        self.leaves(&full, &mut Vec::new(), &mut entries);
        let mut unknown = Vec::new();
        for (source, layer) in self.files() {
            if let Some(table) = &layer.table {
                unknown_keys(table, &full, &mut Vec::new(), &mut |path, value| {
                    unknown.push(Unknown {
                        path,
                        value: value.clone(),
                        source,
                        file: layer.path.clone(),
                    })
                });
            }
        }
        Ok(Effective { entries, unknown })
    }

    fn leaves(&self, table: &toml::Table, path: &mut Vec<String>, out: &mut Vec<Entry>) {
        for (k, v) in table {
            path.push(k.clone());
            match v {
                toml::Value::Table(t) if !t.is_empty() => self.leaves(t, path, out),
                _ => out.push(Entry {
                    path: path.clone(),
                    value: v.clone(),
                    source: self.source(path),
                }),
            }
            path.pop();
        }
    }
}

fn lookup<'a>(table: &'a toml::Table, path: &[String]) -> Option<&'a toml::Value> {
    let (last, parents) = path.split_last()?;
    let mut t = table;
    for p in parents {
        t = t.get(p)?.as_table()?;
    }
    t.get(last)
}

fn unknown_keys(
    file: &toml::Table,
    known: &toml::Table,
    path: &mut Vec<String>,
    found: &mut impl FnMut(Vec<String>, &toml::Value),
) {
    for (k, v) in file {
        path.push(k.clone());
        match (known.get(k), v) {
            (None, _) => found(path.clone(), v),
            (Some(toml::Value::Table(kt)), toml::Value::Table(ft)) => {
                unknown_keys(ft, kt, path, found)
            }
            _ => {}
        }
        path.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("sulla-eff-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }

        fn file(&self, name: &str, body: Option<&str>) -> PathBuf {
            let p = self.0.join(name);
            if let Some(body) = body {
                std::fs::write(&p, body).unwrap();
            }
            p
        }

        fn layers(&self, global: Option<&str>, repo: Option<&str>, flags: &str) -> Layers {
            let g = self.file("global.toml", global);
            let r = self.file("repo.toml", repo);
            Layers::load_from(Some(&g), &r, toml::from_str(flags).unwrap()).unwrap()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    trait Get {
        fn get(&self, path: &[&str]) -> Option<&Entry>;
    }

    impl Get for Effective {
        fn get(&self, path: &[&str]) -> Option<&Entry> {
            self.entries
                .iter()
                .find(|e| e.path.iter().map(String::as_str).eq(path.iter().copied()))
        }
    }

    fn source(e: &Effective, path: &str) -> Source {
        let path: Vec<&str> = path.split('.').collect();
        e.get(&path)
            .unwrap_or_else(|| panic!("no entry {path:?}"))
            .source
    }

    #[test]
    fn defaults_cover_every_serialized_leaf() {
        let d = Dir::new("defaults");
        let layers = d.layers(None, None, "");
        assert!(!layers.global.as_ref().unwrap().exists());
        assert!(!layers.repo.exists());
        let e = layers.effective().unwrap();
        assert!(e.entries.iter().all(|e| e.source == Source::Default));
        assert!(e.unknown.is_empty());
        let max = e.get(&["max_agents"]).unwrap();
        assert_eq!(max.value, toml::Value::Integer(3));
        assert_eq!(
            e.get(&["incus", "mounts"]).unwrap().value,
            toml::Value::Array(vec!["~/.claude".into(), "~/.copilot".into()])
        );
        assert_eq!(
            e.get(&["presets"]).unwrap().value,
            toml::Value::Table(toml::Table::new())
        );
        assert!(e.get(&["base_branch"]).is_none());
        assert!(e.get(&["incus"]).is_none());
    }

    #[test]
    fn global_only_keys_come_from_global() {
        let d = Dir::new("global");
        let layers = d.layers(Some("max_agents = 7\n[claude]\nmodel = \"opus\""), None, "");
        let e = layers.effective().unwrap();
        let cfg = layers.config().unwrap();
        assert_eq!(cfg.max_agents, 7);
        assert_eq!(source(&e, "max_agents"), Source::Global);
        assert_eq!(source(&e, "claude.model"), Source::Global);
        assert_eq!(source(&e, "claude.bin"), Source::Default);
        assert_eq!(source(&e, "poll_secs"), Source::Default);
    }

    #[test]
    fn repo_wins_over_global() {
        let d = Dir::new("repo");
        let global = "max_agents = 7\n[claude]\nbin = \"/opt/claude\"\nmodel = \"opus\"\nextra_args = [\"a\", \"b\"]";
        let repo = "[claude]\nmodel = \"sonnet\"\nextra_args = [\"c\"]";
        let e = d.layers(Some(global), Some(repo), "").effective().unwrap();
        assert_eq!(source(&e, "max_agents"), Source::Global);
        assert_eq!(source(&e, "claude.bin"), Source::Global);
        assert_eq!(source(&e, "claude.model"), Source::Repo);
        assert_eq!(source(&e, "claude.extra_args"), Source::Repo);
        assert_eq!(
            e.get(&["claude", "extra_args"]).unwrap().value,
            toml::Value::Array(vec!["c".into()])
        );
    }

    #[test]
    fn nested_tables_merge_per_leaf() {
        let d = Dir::new("nested");
        let layers = d.layers(
            Some("[incus]\nmemory = \"4GiB\"\n[incus.browse]\nhosts = [\"*.test\"]"),
            Some("[incus.browse]\nports = [8000]\n[incus.env]\nFOO = \"1\""),
            "",
        );
        let e = layers.effective().unwrap();
        let cfg = layers.config().unwrap();
        assert_eq!(source(&e, "incus.memory"), Source::Global);
        assert_eq!(source(&e, "incus.golden"), Source::Default);
        assert_eq!(source(&e, "incus.browse.hosts"), Source::Global);
        assert_eq!(source(&e, "incus.browse.ports"), Source::Repo);
        assert_eq!(source(&e, "incus.env.FOO"), Source::Repo);
        assert_eq!(cfg.incus.browse.ports, vec![8000]);
        assert_eq!(cfg.incus.browse.hosts, vec!["*.test".to_string()]);
    }

    #[test]
    fn flags_win_over_files() {
        let d = Dir::new("flags");
        let layers = d.layers(
            Some("max_agents = 7\nbase_branch = \"develop\""),
            Some("max_agents = 2"),
            "max_agents = 5\nbase_branch = \"main\"",
        );
        let e = layers.effective().unwrap();
        let cfg = layers.config().unwrap();
        assert_eq!(cfg.max_agents, 5);
        assert_eq!(cfg.base_branch.as_deref(), Some("main"));
        assert_eq!(source(&e, "max_agents"), Source::Flag);
        assert_eq!(source(&e, "base_branch"), Source::Flag);
    }

    #[test]
    fn runtime_wins_over_flags() {
        let d = Dir::new("runtime");
        let mut layers = d.layers(None, None, "max_agents = 5");
        layers
            .runtime
            .insert("max_agents".into(), toml::Value::Integer(8));
        let e = layers.effective().unwrap();
        assert_eq!(layers.config().unwrap().max_agents, 8);
        assert_eq!(source(&e, "max_agents"), Source::Runtime);
    }

    #[test]
    fn unknown_keys_are_reported_with_their_file() {
        let d = Dir::new("unknown");
        let repo = "[incus]\nmemory = \"2GiB\"\nbowse = { ports = [1] }\n[nope]\na = 1\n[presets]\nperf = \"p\"";
        let layers = d.layers(Some("max_agent = 4\nmax_agents = 6"), Some(repo), "");
        let e = layers.effective().unwrap();
        let g = d.0.join("global.toml");
        let r = d.0.join("repo.toml");
        let found: Vec<(String, Source, PathBuf)> = e
            .unknown
            .iter()
            .map(|u| (u.path.join("."), u.source, u.file.clone()))
            .collect();
        assert_eq!(
            found,
            vec![
                ("max_agent".into(), Source::Global, g),
                ("incus.bowse".into(), Source::Repo, r.clone()),
                ("nope".into(), Source::Repo, r),
            ]
        );
        assert_eq!(layers.config().unwrap().max_agents, 6);
        assert_eq!(source(&e, "presets.perf"), Source::Repo);
    }

    #[test]
    fn parse_errors_name_the_file_and_position() {
        let d = Dir::new("parse");
        let g = d.file("global.toml", Some("max_agents = 3"));
        let r = d.file("repo.toml", Some("max_agents = 3\n[claude\n"));
        let err = Layers::load_from(Some(&g), &r, toml::Table::new()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&r.display().to_string()), "{msg}");
        assert!(msg.contains("line 2"), "{msg}");

        let r = d.file("repo.toml", Some("poll_secs = 1\nmax_agents = \"many\""));
        let msg = format!(
            "{:#}",
            Layers::load_from(None, &r, toml::Table::new()).unwrap_err()
        );
        assert!(msg.contains(&r.display().to_string()), "{msg}");
        assert!(msg.contains("line 2"), "{msg}");
    }
}
